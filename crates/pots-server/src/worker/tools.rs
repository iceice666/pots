//! M4 的工具：程式碼的唯讀工具、worker 的工具（改檔、執行、開 PR、回報、結束）、team 貼文、
//! `quick_fix` 與 `run_subagent`（`proposal/agent-collaboration.md#profile-一覽`）。
//!
//! 和 `crate::tools` 一樣：模型拿不到的工具就呼叫不到，範圍由工具自己檢查，錯誤以文字回給模型；
//! 狀態變更與事件在同一個 transaction。

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use pots_llm::ToolSpec;
use pots_llm::agent::{Tool, ToolError};
use pots_proto::{Card, TaskId, TeamPostKind};
use serde::Deserialize;
use serde_json::json;

use super::code::{self, check_path, shell_quote};
use super::{QUICK_MAX_FILES, QUICK_MAX_LINES, TaskEnd, WorkerTrigger};
use crate::Error;
use crate::profiles::ToolName;
use crate::tasks::{self, TaskState};
use crate::tools::{Scope, ToolCtx, bad, internal, object, parse, parse_id};

/// `run` 預設與最長的逾時。
const RUN_DEFAULT_SECS: u64 = 300;
const RUN_MAX_SECS: u64 = 1800;
/// 一個檔案最多寫多大（edit_file）。
const MAX_FILE_BYTES: usize = 1024 * 1024;
/// 進度最多幾個字。
const MAX_PROGRESS_CHARS: usize = 300;
/// 一次最多開幾個 subagent。
pub const MAX_SUBAGENTS: usize = 4;

#[derive(Clone, Copy, Debug)]
enum Kind {
    ListFiles,
    ReadFile,
    Grep,
    GitLog,
    GitDiff,
    EditFile,
    Run,
    OpenPr,
    ReportProgress,
    CompleteTask,
    FailTask,
    Escalate,
    PostToTeam,
    ReadTeam,
    QuickFix,
    RunSubagent,
    TryEnv,
    ApplyEnv,
}

struct WorkerTool {
    kind: Kind,
    ctx: Arc<ToolCtx>,
}

fn tool(kind: Kind, ctx: Arc<ToolCtx>) -> Arc<dyn Tool> {
    Arc::new(WorkerTool { kind, ctx })
}

/// 程式碼的唯讀工具（`ctx.code` 有值時）。工作目錄另有 `git_diff`。
pub fn code_tools(ctx: Arc<ToolCtx>) -> Vec<Arc<dyn Tool>> {
    let mut out = vec![
        tool(Kind::ListFiles, ctx.clone()),
        tool(Kind::ReadFile, ctx.clone()),
        tool(Kind::Grep, ctx.clone()),
        tool(Kind::GitLog, ctx.clone()),
    ];
    if ctx.work.is_some() {
        out.push(tool(Kind::GitDiff, ctx));
    }
    out
}

/// M4 的授權工具；其他的回 `None`（由 `crate::tools` 處理）。
pub fn granted(name: ToolName, ctx: Arc<ToolCtx>) -> Option<Arc<dyn Tool>> {
    let kind = match name {
        ToolName::EditFile => Kind::EditFile,
        ToolName::Run => Kind::Run,
        ToolName::OpenPr => Kind::OpenPr,
        ToolName::ReportProgress => Kind::ReportProgress,
        ToolName::CompleteTask => Kind::CompleteTask,
        ToolName::FailTask => Kind::FailTask,
        ToolName::Escalate => Kind::Escalate,
        ToolName::PostToTeam => Kind::PostToTeam,
        ToolName::ReadTeam => Kind::ReadTeam,
        ToolName::QuickFix => Kind::QuickFix,
        ToolName::RunSubagent => Kind::RunSubagent,
        ToolName::TryEnv => Kind::TryEnv,
        ToolName::ApplyEnv => Kind::ApplyEnv,
        _ => return None,
    };
    Some(tool(kind, ctx))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathArgs {
    #[serde(default)]
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LogArgs {
    #[serde(default)]
    path: String,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    path: String,
    #[serde(default)]
    old_text: Option<String>,
    new_text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunArgs {
    command: String,
    #[serde(default)]
    timeout_seconds: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenPrArgs {
    title: String,
    #[serde(default)]
    body: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextArgs {
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SummaryArgs {
    summary: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ErrorArgs {
    error: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReasonArgs {
    reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PostArgs {
    kind: String,
    text: String,
    #[serde(default)]
    to_task: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadTeamArgs {
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QuickFixArgs {
    title: String,
    description: String,
    #[serde(default)]
    repo: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentTask {
    pub prompt: String,
    #[serde(default)]
    pub repo: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubagentArgs {
    profile: String,
    tasks: Vec<SubagentTask>,
}

#[async_trait::async_trait]
impl Tool for WorkerTool {
    fn spec(&self) -> ToolSpec {
        let path =
            json!({"type": "string", "description": "相對於 repo 根目錄的路徑；不給就是整個 repo"});
        let (name, description, parameters) = match self.kind {
            Kind::ListFiles => (
                "list_files",
                "列出 repo 裡的檔案（遵守 .gitignore）。",
                object(json!({"path": path}), &[]),
            ),
            Kind::ReadFile => (
                "read_file",
                "讀一個檔案的內容（太長時只回前面的部分）。",
                object(json!({"path": {"type": "string"}}), &["path"]),
            ),
            Kind::Grep => (
                "grep",
                "用延伸正規表示式搜尋 repo（git grep），回傳 `檔案:行號:內容`。",
                object(
                    json!({"pattern": {"type": "string"}, "path": path}),
                    &["pattern"],
                ),
            ),
            Kind::GitLog => (
                "git_log",
                "最近的 commit（一行一個）。",
                object(
                    json!({"path": path, "limit": {"type": "integer", "description": "幾個（預設 20，最多 100）"}}),
                    &[],
                ),
            ),
            Kind::GitDiff => (
                "git_diff",
                "工作目錄的狀態：未 commit 的檔案、相對基準分支的 commit 與完整 diff。",
                object(json!({}), &[]),
            ),
            Kind::EditFile => (
                "edit_file",
                "修改一個檔案。給 old_text 時把它（必須剛好出現一次）換成 new_text；不給 old_text 時以 new_text \
                 作為整份內容（新建或覆蓋）。",
                object(
                    json!({
                        "path": {"type": "string"},
                        "old_text": {"type": "string", "description": "要被取代的原文（含足夠的上下文讓它唯一）"},
                        "new_text": {"type": "string"},
                    }),
                    &["path", "new_text"],
                ),
            ),
            Kind::Run => (
                "run",
                "在工作目錄裡用 sh 執行指令（例如跑測試、git commit），回傳結束代碼與輸出。不要用來 push。",
                object(
                    json!({
                        "command": {"type": "string"},
                        "timeout_seconds": {"type": "integer", "description": "預設 300，最多 1800"},
                    }),
                    &["command"],
                ),
            ),
            Kind::OpenPr => (
                "open_pr",
                "把任務分支推上去並開 PR（已經開過就更新同一個）。還沒 commit 的修改會先以 title 為訊息 commit。\
                 回傳 PR、commit 的完整 sha；之後用 wait_for（ci_finished）等 CI。",
                object(
                    json!({
                        "title": {"type": "string"},
                        "body": {"type": "string", "description": "PR 說明：改了什麼、為什麼、怎麼驗證"},
                    }),
                    &["title"],
                ),
            ),
            Kind::ReportProgress => (
                "report_progress",
                "回報一句進度，顯示在狀態面板。",
                object(json!({"text": {"type": "string"}}), &["text"]),
            ),
            Kind::CompleteTask => (
                "complete_task",
                "任務完成時呼叫：寫明做了什麼、產出（PR 與驗證結果，或評估／調查類任務的 Report artifact_id）。\
                 呼叫後這一輪就結束。",
                object(json!({"summary": {"type": "string"}}), &["summary"]),
            ),
            Kind::FailTask => (
                "fail_task",
                "任務做不下去（或 CI 失敗）時呼叫：寫明原因與目前的狀況，交給 Planner 決定。呼叫後這一輪就結束。",
                object(json!({"error": {"type": "string"}}), &["error"]),
            ),
            Kind::Escalate => (
                "escalate",
                "需要 Planner 或使用者決定時呼叫（例如需要設計決定、任務本身有問題）；快速修改超出範圍時會升級成一般 Goal。\
                 已做的修改留在分支上。呼叫後這一輪就結束。",
                object(json!({"reason": {"type": "string"}}), &["reason"]),
            ),
            Kind::PostToTeam => (
                "post_to_team",
                match self.ctx.scope {
                    Scope::Task { .. } => {
                        "在 Goal 的 team 發一則短訊：finding（發現，例如「auth 測試要先起 redis」）、question（問 Planner；\
                         給 to_task 就是問那個任務）、blocker（卡住了）、handoff（交接給 to_task，會叫醒它）。\
                         question 與 blocker 會叫醒 Planner。"
                    }
                    _ => {
                        "在 Goal 的 team 發一則短訊：decision（回答 worker 的問題、做決定）、finding、handoff（交接給 to_task，會叫醒它）。"
                    }
                },
                object(
                    json!({
                        "kind": {"type": "string", "enum": ["finding", "question", "decision", "blocker", "handoff"]},
                        "text": {"type": "string"},
                        "to_task": {"type": "string", "description": "question／handoff 的對象任務"},
                    }),
                    &["kind", "text"],
                ),
            ),
            Kind::ReadTeam => (
                "read_team",
                "讀 Goal 的 team 貼文（最新的在最後）。",
                object(
                    json!({"limit": {"type": "integer", "description": "幾則（預設 30）"}}),
                    &[],
                ),
            ),
            Kind::QuickFix => (
                "quick_fix",
                "小修改（改錯字、調設定、單一檔案的小 bug）的快速通道：直接派一個 worker 改、開 PR、等 CI，不經 Planner。\
                 最多 5 個檔案、200 行、不需要設計決定；worker 發現超出時會升級成一般 Goal。使用者會看到一張 Goal 卡片。",
                object(
                    json!({
                        "title": {"type": "string", "description": "簡短的標題"},
                        "description": {"type": "string", "description": "要改什麼、在哪裡、怎麼算完成"},
                        "repo": {"type": "string", "description": "owner/repo；project 只監看一個 repo 時可以不給"},
                    }),
                    &["title", "description"],
                ),
            ),
            Kind::RunSubagent => (
                "run_subagent",
                "開唯讀的 subagent 並等它回來：explorer 查程式碼、讀 log；reviewer 審查 diff（worker 才有自己的 diff）。\
                 每個 task 是一個獨立的 subagent（最多 4 個並行），只看得到你給的 prompt，所以要寫清楚要查什麼、要回答什麼。\
                 回傳各自的結論。",
                object(
                    json!({
                        "profile": {"type": "string", "enum": ["explorer", "reviewer"]},
                        "tasks": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "prompt": {"type": "string"},
                                    "repo": {"type": "string", "description": "要讀哪個 repo（owner/repo）；worker 讀自己的工作目錄，不用給"},
                                },
                                "required": ["prompt"],
                                "additionalProperties": false,
                            },
                        },
                    }),
                    &["profile", "tasks"],
                ),
            ),
            Kind::TryEnv => (
                "try_env",
                "實際建置並驗證一份環境規格：用 base_image 加 install 建 image（同樣的內容會沿用已建好的），\
                 開一個新的容器（掛這個任務的工作目錄與快取、帶 env），依序跑 prepare 與 verify。\
                 回傳每一步的結果與輸出的最後一段；全部成功才算通過。建置可能要很久。",
                super::env_setup::spec_schema(),
            ),
            Kind::ApplyEnv => (
                "apply_env",
                "套用這次 try_env 通過的環境規格（spec 要和通過的那份一字不差），之後這個 project 的 worker 都用它。\
                 summary 寫你分析了什麼（語言、工具鏈、系統套件與依據的檔案）、為什麼這樣裝。呼叫後任務完成、這一輪結束。",
                object(
                    json!({
                        "spec": super::env_setup::spec_schema(),
                        "summary": {"type": "string"},
                    }),
                    &["spec", "summary"],
                ),
            ),
        };
        ToolSpec {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }

    /// 和 `crate::tools` 一樣在另一個 task 裡執行：run 被取消時寫入不能停在一半。
    async fn call(&self, arguments: &str) -> Result<String, ToolError> {
        let t = WorkerTool {
            kind: self.kind,
            ctx: self.ctx.clone(),
        };
        let arguments = arguments.to_string();
        tokio::spawn(async move { t.dispatch(&arguments).await })
            .await
            .unwrap_or_else(|e| {
                tracing::error!(error = %e, "工具的 task 異常結束");
                Err(ToolError("伺服器內部錯誤，請稍後再試".into()))
            })
    }
}

impl WorkerTool {
    async fn dispatch(&self, a: &str) -> Result<String, ToolError> {
        // 結束工具（wait_for、complete_task…）之後同一輪剩下的呼叫不再改東西。
        let mutating = matches!(
            self.kind,
            Kind::EditFile
                | Kind::Run
                | Kind::OpenPr
                | Kind::ReportProgress
                | Kind::CompleteTask
                | Kind::FailTask
                | Kind::Escalate
                | Kind::PostToTeam
                | Kind::TryEnv
                | Kind::ApplyEnv
        );
        if mutating && self.ctx.finished.load(Ordering::SeqCst) {
            return Err(bad(
                "這一輪已經結束（前面的工具已經結束了任務或開始等待），沒有執行",
            ));
        }
        match self.kind {
            Kind::ListFiles => {
                let args: PathArgs = parse(a)?;
                self.code()?.list_files(&args.path).await.map_err(bad)
            }
            Kind::ReadFile => {
                let args: PathArgs = parse(a)?;
                self.code()?.read_file(&args.path).await.map_err(bad)
            }
            Kind::Grep => {
                let args: GrepArgs = parse(a)?;
                if args.pattern.is_empty() {
                    return Err(bad("pattern 不能是空的"));
                }
                self.code()?
                    .grep(&args.pattern, &args.path)
                    .await
                    .map_err(bad)
            }
            Kind::GitLog => {
                let args: LogArgs = parse(a)?;
                self.code()?
                    .git_log(&args.path, args.limit.unwrap_or(20))
                    .await
                    .map_err(bad)
            }
            Kind::GitDiff => match self.code()?.git_diff().await {
                Some(r) => r.map_err(bad),
                None => Err(bad("這裡沒有工作目錄可以比較")),
            },
            Kind::EditFile => self.edit_file(parse(a)?).await,
            Kind::Run => self.run(parse(a)?).await,
            Kind::OpenPr => self.open_pr(parse(a)?).await,
            Kind::ReportProgress => self.report_progress(parse(a)?).await,
            Kind::CompleteTask => {
                let args: SummaryArgs = parse(a)?;
                let s = tasks::check_text("summary ", &args.summary, 2000, true).map_err(bad)?;
                self.end(TaskEnd::Done(s)).await
            }
            Kind::FailTask => {
                let args: ErrorArgs = parse(a)?;
                let e = tasks::check_text("error ", &args.error, 2000, true).map_err(bad)?;
                self.end(TaskEnd::Failed(e)).await
            }
            Kind::Escalate => {
                let args: ReasonArgs = parse(a)?;
                let r = tasks::check_text("reason ", &args.reason, 2000, true).map_err(bad)?;
                let quick = self.ctx.work.as_ref().is_some_and(|w| w.quick);
                self.end(if quick {
                    TaskEnd::Escalate(r)
                } else {
                    TaskEnd::NeedsHuman(format!("{}{r}", super::ESCALATED))
                })
                .await
            }
            Kind::PostToTeam => self.post_to_team(parse(a)?).await,
            Kind::ReadTeam => self.read_team(parse(a)?).await,
            Kind::QuickFix => self.quick_fix(parse(a)?).await,
            Kind::RunSubagent => {
                let args: SubagentArgs = parse(a)?;
                super::subagent::run(&self.ctx, &args.profile, args.tasks).await
            }
            Kind::TryEnv => super::env_setup::try_env(&self.ctx, self.work()?, parse(a)?).await,
            Kind::ApplyEnv => super::env_setup::apply_env(&self.ctx, self.work()?, parse(a)?).await,
        }
    }

    fn code(&self) -> Result<&Arc<dyn code::CodeAccess>, ToolError> {
        self.ctx
            .code
            .as_ref()
            .ok_or_else(|| bad("這個 run 看不到程式碼"))
    }

    fn work(&self) -> Result<&Arc<super::WorkCtx>, ToolError> {
        self.ctx
            .work
            .as_ref()
            .ok_or_else(|| bad("這個工具只有 worker 能用"))
    }

    fn own_task(&self) -> Result<TaskId, ToolError> {
        self.ctx
            .scope
            .task()
            .ok_or_else(|| bad("這個工具只有 worker 能用"))
    }

    async fn edit_file(&self, args: EditArgs) -> Result<String, ToolError> {
        let work = self.work()?;
        let path = check_path(&args.path).map_err(bad)?;
        if path.is_empty() || path == ".git" || path.starts_with(".git/") {
            return Err(bad("不能改這個路徑"));
        }
        let q = shell_quote(&path);
        let timeout = Duration::from_secs(60);
        let content = match &args.old_text {
            None => args.new_text.clone(),
            Some(old) => {
                if old.is_empty() {
                    return Err(bad("old_text 不能是空的；要整份覆蓋時不要給 old_text"));
                }
                let out = work
                    .env
                    .exec(&format!("cat -- {q}"), None, timeout)
                    .await
                    .map_err(bad)?;
                if out.code != Some(0) {
                    return Err(bad(format!("讀不到 {path}：{}", out.stderr.trim())));
                }
                if out.truncated {
                    return Err(bad("檔案太大，請用 run 修改"));
                }
                if out.stdout.contains('\u{FFFD}') {
                    return Err(bad("這個檔案不是 UTF-8 文字，請用 run 修改"));
                }
                match out.stdout.matches(old.as_str()).count() {
                    0 => return Err(bad("找不到 old_text（要和檔案內容完全一樣，含空白與縮排）")),
                    1 => out.stdout.replacen(old.as_str(), &args.new_text, 1),
                    n => {
                        return Err(bad(format!(
                            "old_text 出現了 {n} 次，請多帶一些上下文讓它唯一"
                        )));
                    }
                }
            }
        };
        if content.len() > MAX_FILE_BYTES {
            return Err(bad("檔案太大"));
        }
        let out = work
            .env
            .exec(
                &format!("mkdir -p -- \"$(dirname -- {q})\" && cat > {q}"),
                Some(content.into_bytes()),
                timeout,
            )
            .await
            .map_err(bad)?;
        if !out.ok() {
            return Err(bad(format!("寫不進 {path}：{}", out.stderr.trim())));
        }
        Ok(format!("已寫入 {path}"))
    }

    async fn run(&self, args: RunArgs) -> Result<String, ToolError> {
        let work = self.work()?;
        if args.command.trim().is_empty() {
            return Err(bad("command 不能是空的"));
        }
        let secs = args
            .timeout_seconds
            .unwrap_or(RUN_DEFAULT_SECS)
            .clamp(1, RUN_MAX_SECS);
        let out = tokio::select! {
            () = self.ctx.cancel.cancelled() => return Err(bad("已取消")),
            r = work.env.exec(&args.command, None, Duration::from_secs(secs)) => r.map_err(bad)?,
        };
        let mut text = if out.timed_out {
            format!("逾時（{secs} 秒），指令已被中止\n")
        } else {
            format!(
                "exit code：{}\n",
                out.code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "（被訊號中止）".into())
            )
        };
        if !out.stdout.is_empty() {
            text.push_str("--- stdout\n");
            text.push_str(&out.stdout);
            if !out.stdout.ends_with('\n') {
                text.push('\n');
            }
        }
        if !out.stderr.is_empty() {
            text.push_str("--- stderr\n");
            text.push_str(&out.stderr);
        }
        Ok(text)
    }

    async fn open_pr(&self, args: OpenPrArgs) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        let work = self.work()?;
        let title = tasks::check_line("title ", &args.title, 200).map_err(bad)?;
        let body = tasks::check_text("body ", &args.body, 20_000, false).map_err(bad)?;
        let repos = ctx.worker.repos();
        // 1. 沒 commit 的修改先 commit（在執行環境裡做：工作目錄不信任）。
        let out = work
            .env
            .exec(
                &format!(
                    "{g} add -A && ({g} diff --cached --quiet || {g} commit --quiet --no-verify -m {})",
                    shell_quote(&title),
                    g = super::repo::SAFE_GIT,
                ),
                None,
                Duration::from_secs(120),
            )
            .await
            .map_err(bad)?;
        if !out.ok() {
            return Err(bad(format!(
                "commit 失敗：{}",
                tasks::truncate(out.stderr.trim(), 500)
            )));
        }
        // 2. 在執行環境裡打 bundle 交回平台（平台不在主機上對工作目錄執行 git），取回後檢查範圍。
        let out_name = format!("{}.bundle", work.task);
        let script = super::repo::Repos::bundle_script(
            &work.checkout,
            &format!("{}/{out_name}", work.env.out_dir_seen()),
        );
        let out = work
            .env
            .exec(&script, None, Duration::from_secs(300))
            .await
            .map_err(bad)?;
        if !out.ok() {
            if out.stderr.contains("empty bundle") {
                return Err(bad(
                    "任務分支相對基準分支沒有任何 commit，沒有東西可以開 PR",
                ));
            }
            return Err(bad(format!(
                "無法取得任務分支：{}",
                tasks::truncate(out.stderr.trim(), 500)
            )));
        }
        let pushed = repos
            .collect(
                &work.source,
                &work.mirror,
                &work.env.out_dir().join(&out_name),
                &work.checkout,
                work.task,
            )
            .await
            .map_err(|e| bad(format!("無法取得任務分支：{e}")))?;
        let ahead = repos
            .ahead(&work.mirror, &work.checkout, work.task)
            .await
            .map_err(|e| bad(e.to_string()))?;
        if ahead == 0 {
            return Err(bad(
                "任務分支相對基準分支沒有任何 commit，沒有東西可以開 PR",
            ));
        }
        if work.quick
            && (pushed.files > QUICK_MAX_FILES
                || pushed.insertions + pushed.deletions > QUICK_MAX_LINES)
        {
            return Err(bad(format!(
                "快速修改最多 {QUICK_MAX_FILES} 個檔案、{QUICK_MAX_LINES} 行，這次改了 {} 個檔案、{} 行：\
                 用 escalate 升級成一般 Goal（修改會留在分支上）",
                pushed.files,
                pushed.insertions + pushed.deletions
            )));
        }
        // CI 設定（workflow、composite action）改了的話，推到 repo 就會以 repo 的秘密執行 worker 寫的
        // 東西：交給人決定。這只是盡力而為（CI 呼叫的腳本也會被執行），見 M4 的已知限制。
        if let Some(p) = pushed.paths.iter().find(|p| p.starts_with(".github/")) {
            return Err(bad(format!(
                "這次改到了 .github 底下的設定（{p}）：平台不會替 worker 推上去。把這些修改拿掉，或用 escalate 交給使用者決定"
            )));
        }
        // 任務在這期間被取消或已經結束：不推、不開 PR。
        {
            let mut conn = ctx.store.acquire().await.map_err(internal)?;
            let state = tasks::task(&mut conn, work.task)
                .await
                .map_err(internal)?
                .map(|t| t.state);
            if state != Some(TaskState::Running) {
                return Err(bad("任務已經不在執行中，沒有推上去"));
            }
        }
        // 本機資料夾：沒有 remote、PR 與 CI。變更留在鏡像的任務分支上，核准後由平台套用。
        if let Some(path) = work.source.local_path() {
            let change = work.source.key();
            let progress = "已記錄變更，等使用者核准".to_string();
            let result: Result<(), Error> = async {
                let mut txn = ctx.hub.begin().await?;
                tasks::set_task_work(
                    txn.conn(),
                    work.task,
                    Some(&work.checkout.branch),
                    Some((&change, None)),
                    Some(&progress),
                )
                .await?;
                if let Some(goal) = ctx.scope.goal() {
                    tasks::push_goal(&mut txn, goal).await?;
                }
                txn.commit().await
            }
            .await;
            result.map_err(internal)?;
            return Ok(json!({
                "change": change,
                "path": path,
                "branch": work.checkout.branch,
                "base": work.checkout.base,
                "sha": pushed.sha,
                "changed": {"files": pushed.files, "insertions": pushed.insertions, "deletions": pushed.deletions},
                "next": format!(
                    "這是本機資料夾：沒有 PR 與 CI，變更留在任務分支上。用 request_approval 請使用者核准，核准後平台把分支套用到 {path}。",
                ),
            })
            .to_string());
        }
        // 3. push 與開 PR（token 只在平台這邊用）。
        let Some(watcher) = ctx.worker.watcher() else {
            return Err(bad("服務還沒準備好"));
        };
        let gh = watcher.github();
        let config = {
            let mut conn = ctx.store.acquire().await.map_err(internal)?;
            crate::watcher::github_config(&mut conn)
                .await
                .map_err(internal)?
        };
        let token = gh
            .token()
            .ok_or_else(|| bad("還沒設定 GitHub token，請使用者到設定的「GitHub」類別設定"))?;
        let url = repos.remote_url(&work.source, &gh.api(&config));
        repos
            .push(
                &work.source,
                &work.mirror,
                &url,
                Some(&token),
                &work.checkout,
                work.task,
            )
            .await
            .map_err(|e| bad(format!("push 失敗：{e}")))?;
        let repo = work.source.key().to_string();
        let (number, pr_url, created) = gh
            .open_pull(
                &config,
                &repo,
                &work.checkout.branch,
                &work.checkout.base,
                &title,
                &body,
            )
            .await
            .map_err(|e| bad(format!("無法開 PR：{e}")))?;
        let change = format!("{repo}#{number}");
        let progress = if created {
            format!("已開 PR #{number}")
        } else {
            format!("已更新 PR #{number}")
        };
        let result: Result<(), Error> = async {
            let mut txn = ctx.hub.begin().await?;
            tasks::set_task_work(
                txn.conn(),
                work.task,
                Some(&work.checkout.branch),
                Some((&change, Some(pr_url.as_str()))),
                Some(&progress),
            )
            .await?;
            if let Some(goal) = ctx.scope.goal() {
                tasks::push_goal(&mut txn, goal).await?;
            }
            txn.commit().await
        }
        .await;
        result.map_err(internal)?;
        Ok(json!({
            "pr": change,
            "url": pr_url,
            "created": created,
            "branch": work.checkout.branch,
            "base": work.checkout.base,
            "sha": pushed.sha,
            "changed": {"files": pushed.files, "insertions": pushed.insertions, "deletions": pushed.deletions},
            "next": format!(
                "用 wait_for 等 CI：on = {{\"kind\": \"ci_finished\", \"repo\": \"{repo}\", \"sha\": \"{}\"}}",
                pushed.sha
            ),
        })
        .to_string())
    }

    async fn report_progress(&self, args: TextArgs) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        let task = self.own_task()?;
        let text = tasks::check_text("text ", &args.text, MAX_PROGRESS_CHARS, true).map_err(bad)?;
        let text = tasks::one_line(&text, MAX_PROGRESS_CHARS);
        let result: Result<(), Error> = async {
            let mut txn = ctx.hub.begin().await?;
            tasks::set_task_work(txn.conn(), task, None, None, Some(&text)).await?;
            if let Some(goal) = ctx.scope.goal() {
                tasks::push_goal(&mut txn, goal).await?;
            }
            txn.commit().await
        }
        .await;
        result.map_err(internal)?;
        Ok("已更新進度".into())
    }

    async fn end(&self, end: TaskEnd) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        let id = self.own_task()?;
        let result: Result<Result<bool, String>, Error> = async {
            let mut txn = ctx.hub.begin().await?;
            let Some(task) = tasks::task(txn.conn(), id).await? else {
                return Ok(Err("找不到你的任務".into()));
            };
            if task.state != TaskState::Running {
                return Ok(Err(format!(
                    "任務現在是 {}，不能再結束一次",
                    task.state.name()
                )));
            }
            let ends = super::end_task(&mut txn, &task, end, &ctx.actor, Some(ctx.run_id)).await?;
            let worker = ctx.worker.clone();
            txn.commit_then(|| ends.notify(&worker, Some(id))).await?;
            Ok(Ok(ends.planner))
        }
        .await;
        result.map_err(internal)?.map_err(bad)?;
        ctx.finish();
        Ok("已記錄，這一輪到此結束".into())
    }

    async fn post_to_team(&self, args: PostArgs) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        let goal = ctx
            .scope
            .goal()
            .ok_or_else(|| bad("team 貼文只能在 Goal 裡發"))?;
        let kind = crate::team::parse_kind(&args.kind)
            .ok_or_else(|| bad(format!("不認得的種類：{}", args.kind)))?;
        let worker = matches!(ctx.scope, Scope::Task { .. });
        match kind {
            TeamPostKind::Decision if worker => return Err(bad("只有 Planner 能發 decision")),
            TeamPostKind::Question | TeamPostKind::Blocker if !worker => {
                return Err(bad(
                    "Planner 用 decision 回答；question／blocker 是 worker 用的",
                ));
            }
            _ => {}
        }
        let text = tasks::check_text("text ", &args.text, crate::team::MAX_POST_CHARS, true)
            .map_err(bad)?;
        let to = args
            .to_task
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|s| parse_id::<TaskId>("to_task", s))
            .transpose()?;
        if kind == TeamPostKind::Handoff && to.is_none() {
            return Err(bad("handoff 要給 to_task"));
        }
        if to.is_some() && ctx.scope.task() == to {
            return Err(bad("不能發給自己"));
        }
        let now = ctx.clock.now();
        let result: Result<Result<(bool, Option<TaskId>), String>, Error> = async {
            let mut txn = ctx.hub.begin().await?;
            let Some(g) = tasks::goal(txn.conn(), goal).await? else {
                return Ok(Err("找不到 Goal".into()));
            };
            if !g.state.is_active() {
                return Ok(Err(format!("Goal「{}」已經{}", g.title, g.state.label())));
            }
            if matches!(kind, TeamPostKind::Question | TeamPostKind::Blocker)
                && to.is_none()
                && tasks::kind_without_planner(&tasks::goal_kind(txn.conn(), goal).await?)
            {
                return Ok(Err(
                    "這個 Goal 沒有 Planner：快速修改需要決定的事用 escalate 升級成一般 Goal；\
                     環境分析要問使用者用 ask_user"
                        .into(),
                ));
            }
            // 交接與 Planner 對某個任務的決定會叫醒那個任務。worker 的交接只叫醒還在進行（或等待）的
            // 任務；Planner 也能叫醒需要決定的任務（它就是做決定的人）。
            let mut wake_task = None;
            if let Some(to) = to {
                match tasks::task(txn.conn(), to).await? {
                    Some(t) if t.goal == goal => {
                        let wakes_it = match kind {
                            TeamPostKind::Handoff => true,
                            TeamPostKind::Decision => !worker,
                            _ => false,
                        };
                        let state_ok = match &t.state {
                            TaskState::Planned | TaskState::Running | TaskState::Waiting { .. } => {
                                true
                            }
                            // 只叫醒在等 Planner 決定的（worker 用 escalate 交回的）；重新登入、
                            // 重試用完這類要你處理的不叫醒。
                            TaskState::NeedsHuman { reason } => {
                                !worker && reason.starts_with(super::ESCALATED)
                            }
                            _ => false,
                        };
                        if wakes_it && state_ok && t.repo.is_some() {
                            wake_task = Some(t);
                        }
                    }
                    _ => return Ok(Err(format!("這個 Goal 沒有任務 {to}"))),
                }
            }
            let (_, wakes) = match crate::team::insert(
                txn.conn(),
                goal,
                kind,
                &ctx.actor,
                ctx.scope.task(),
                to,
                &text,
                now,
            )
            .await?
            {
                Ok(r) => r,
                Err(e) => return Ok(Err(e)),
            };
            if wakes.planner {
                crate::planner::enqueue(
                    txn.conn(),
                    g.project,
                    goal,
                    &crate::planner::PlannerTrigger::TeamPost {
                        post: kind.label().into(),
                        from: ctx.actor.label(),
                        text: tasks::one_line(&text, 1000),
                    },
                )
                .await?;
            }
            let mut kicked = None;
            if let Some(t) = wake_task {
                // 等待中或需要決定的任務先回到佇列（訂閱一起清掉）；正在跑的由排隊的 run 接著送到。
                if matches!(
                    t.state,
                    TaskState::Waiting { .. } | TaskState::NeedsHuman { .. }
                ) {
                    tasks::set_task_state(
                        txn.conn(),
                        &t,
                        &TaskState::Planned,
                        &tasks::Cause {
                            by: ctx.actor.clone(),
                            reason: format!("收到{}", kind.label()),
                        },
                    )
                    .await?;
                }
                super::enqueue(
                    txn.conn(),
                    g.project,
                    goal,
                    t.id,
                    &match kind {
                        TeamPostKind::Decision => WorkerTrigger::Decision {
                            text: tasks::one_line(&text, 2000),
                        },
                        _ => WorkerTrigger::Handoff {
                            from: ctx.actor.label(),
                            text: tasks::one_line(&text, 2000),
                            planner: !worker,
                        },
                    },
                )
                .await?;
                kicked = Some(t.id);
            }
            tasks::push_goal(&mut txn, goal).await?;
            let worker = ctx.worker.clone();
            let planner = ctx.planner.clone();
            txn.commit_then(|| {
                if wakes.planner {
                    planner.kick();
                }
                if kicked.is_some() {
                    worker.kick();
                }
            })
            .await?;
            Ok(Ok((wakes.planner, kicked)))
        }
        .await;
        let (planner, task) = result.map_err(internal)?.map_err(bad)?;
        Ok(json!({
            "posted": kind.label(),
            "woke_planner": planner,
            "woke_task": task.map(|t| t.to_string()),
        })
        .to_string())
    }

    async fn read_team(&self, args: ReadTeamArgs) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        let goal = ctx
            .scope
            .goal()
            .ok_or_else(|| bad("team 貼文只能在 Goal 裡讀"))?;
        let mut conn = ctx.store.acquire().await.map_err(internal)?;
        let posts = crate::team::recent(&mut conn, goal, args.limit.unwrap_or(30).clamp(1, 100))
            .await
            .map_err(internal)?;
        if posts.is_empty() {
            return Ok("（還沒有 team 貼文）".into());
        }
        crate::team::render(&mut conn, &posts)
            .await
            .map_err(internal)
    }

    async fn quick_fix(&self, args: QuickFixArgs) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        if ctx.scope != Scope::Project {
            return Err(bad("quick_fix 只有對話 agent 能用"));
        }
        let title =
            tasks::check_line("title ", &args.title, tasks::MAX_TITLE_CHARS).map_err(bad)?;
        let description =
            tasks::check_text("description ", &args.description, 4000, true).map_err(bad)?;
        if ctx.goals_created.load(Ordering::SeqCst) >= 3 {
            return Err(bad("一次回覆最多開 3 個 Goal"));
        }
        let result: Result<Result<(pots_proto::GoalId, TaskId, String), String>, Error> = async {
            let mut txn = ctx.hub.begin().await?;
            let repo =
                match super::pick_workdir(txn.conn(), ctx.project, args.repo.as_deref()).await? {
                    Ok(r) => r,
                    Err(e) => return Ok(Err(e)),
                };
            let opened = super::open_quick(
                &mut txn,
                ctx.project,
                &title,
                &description,
                &repo,
                &ctx.actor,
                Some(&ctx.reader),
            )
            .await?;
            let (goal, task) = match opened {
                Ok(x) => x,
                Err(e) => return Ok(Err(e)),
            };
            let worker = ctx.worker.clone();
            txn.commit_then(|| worker.kick()).await?;
            Ok(Ok((goal, task, repo)))
        }
        .await;
        let (goal, task, repo) = result.map_err(internal)?.map_err(bad)?;
        ctx.goals_created.fetch_add(1, Ordering::SeqCst);
        ctx.cards.lock().unwrap().push(Card::GoalCreated { goal });
        Ok(json!({
            "goal_id": goal.to_string(),
            "task_id": task.to_string(),
            "repo": repo,
            "note": "已派一個 worker：它會在新分支上修改、開 PR、等 CI，結果會貼回對話。",
        })
        .to_string())
    }
}
