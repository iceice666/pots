//! Coding worker（M4，`proposal/agent-runtime.md#worker-adapter內建-worker-與外部-coding-cli`）。
//!
//! - **派工**：任務建立時（Planner 的 `create_task`、`quick_fix`）與被喚醒時，在同一個 transaction
//!   裡以 `queued` 排一個 `worker` 的 `agent_runs`（[`enqueue`]）；同一個任務已經有排隊中的 run 時
//!   把觸發併進去。
//! - **執行**：runner 取走 run 時任務變成 `Running`，準備任務的工作目錄（[`repo`]）與執行環境
//!   （[`exec`]），再依設定交給內建 worker（pots-llm 的 loop，[`tools`]）或 Claude Code
//!   （[`claude`]，平台工具經 [`mcp`]）。每個任務同時最多一個 run，整個實例最多
//!   `max_parallel` 個。
//! - **結束**：worker 以 `complete_task`、`fail_task`、`escalate` 或 `wait_for` 結束一輪；沒有呼叫
//!   就結束（或出錯、逾時）時任務轉成 `NeedsHuman`。任務結束時叫醒 Planner；快速修改沒有
//!   Planner，結果直接貼到對話流。
//! - **中斷**：取消任務、Goal 結束會中斷正在跑的 run；服務關閉時被中斷的 run 放回佇列（不算一次
//!   重試），當掉留下的 `running` 在下次啟動時由 [`requeue_stale`] 放回。
//! - **續接**：Claude Code 的 session ID 在啟動 CLI 之前寫進 `worker_sessions`；官方 session 存在
//!   專屬 home（容器）或你自己的 Claude Code 設定（本機）。被喚醒時用 `--resume` 接著做，找不到
//!   session 時開新的並帶完整 context。

pub mod claude;
pub mod code;
pub mod exec;
pub mod mcp;
pub mod repo;
pub mod subagent;
pub mod tools;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use pots_llm::agent::{self, RunConfig};
use pots_llm::{Item, LlmError};
use pots_proto::{
    Card, ExecKind, GoalId, GoalState, ProjectId, ServerEvent, TaskId, Timestamp,
    WorkerBackendKind, WorkerSettingsDraft, WorkerSettingsView,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::Row;
use sqlx::sqlite::SqliteConnection;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use uuid::Uuid;

use crate::Error;
use crate::artifacts::{self, Edit};
use crate::config::{Clock, WorkerConfig};
use crate::hub::{Hub, LogTxn};
use crate::planner::{self, Planner, PlannerTrigger};
use crate::profiles::{AgentProfile, Profiles, ToolName};
use crate::providers::ProviderRouter;
use crate::runs::explain;
use crate::store::{LOCAL_USER_ID, Store, parse_id};
use crate::tasks::{self, Cause, GoalRow, Principal, TaskRow, TaskState};
use crate::tools::{Scope, ToolCtx};
use crate::watcher::{WaitCond, Watcher};

use self::exec::{Env, ExecSpec};
use self::repo::{Checkout, RepoSource, Repos};

/// lease 長度；runner 每 [`HEARTBEAT`] 延長一次。
const LEASE: Duration = Duration::from_secs(90);
const HEARTBEAT: Duration = Duration::from_secs(30);
/// 沒有通知時多久檢查一次佇列。
const POLL: Duration = Duration::from_secs(15);
/// 一個 run 最多被取走幾次（當掉重來也算）。
pub const MAX_ATTEMPTS: i64 = 3;
/// `instance_settings` 裡 worker 設定的 key。
pub const SETTING_KEY: &str = "worker";
/// Claude Code 的提醒：一輪結束卻沒有呼叫結束工具時送一次。
const NUDGE: &str = "你還沒有用平台工具結束這一輪。請依情況呼叫 mcp__pots__complete_task、\
mcp__pots__fail_task、mcp__pots__escalate、mcp__pots__wait_for（開了 PR 的話先等 CI）或 \
mcp__pots__request_approval（CI 通過後申請 merge）。";
/// Claude Code 一律禁止的原生工具：背景常駐、排程類（等待一律走平台的 wait_for）。名稱依 OpenClaw
/// 的預設；背景執行的 Bash 由 [`claude_policy`] 擋（權限規則比對的是指令，不是 `run_in_background`）。
const CLAUDE_DISALLOWED: &[&str] = &["ScheduleWakeup", "CronCreate", "Monitor"];
/// Claude Code 的 worker 另外的說明：worker profile 的規則寫的是內建 worker 的工具名稱。
const CLAUDE_NOTE: &str = "\n\n# 在 Claude Code 裡\n\n\
上面的規則寫給所有 worker。你在 Claude Code 裡：讀檔、改檔、執行指令與開 subagent 用 Claude Code \
自己的工具（Read、Edit、Write、Bash、Task），規則裡的 read_file、edit_file、run、run_subagent 都換成它們。\
平台工具的名稱前面有 `mcp__pots__`，例如 mcp__pots__open_pr、mcp__pots__wait_for、mcp__pots__complete_task、\
mcp__pots__fail_task、mcp__pots__escalate、mcp__pots__request_approval、mcp__pots__post_to_team、\
mcp__pots__report_progress。\
不要用背景執行（run_in_background），也不要自己排程或等待：要等 CI 或其他事件一律用 mcp__pots__wait_for。";
/// worker 用 escalate 交回時任務原因的開頭（Planner 的決定只叫醒這種需要決定的任務）。
pub const ESCALATED: &str = "worker 需要決定：";
/// 快速修改的範圍上限。
pub const QUICK_MAX_FILES: u32 = 5;
pub const QUICK_MAX_LINES: u32 = 200;

/// 叫醒 worker 的原因，寫進 `agent_runs.trigger`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkerTrigger {
    /// 任務剛建立。
    Assigned,
    /// 等待的條件成立或逾時（`crate::watcher::wake_task`）。
    Wake { waited: WaitCond, result: String },
    /// 同一個 Goal 的成員交接給你。`planner` 是 Planner 發的（它可以叫醒需要決定的任務）。
    Handoff {
        from: String,
        text: String,
        #[serde(default)]
        planner: bool,
    },
    /// Planner 對你的問題的決定。
    Decision { text: String },
    /// 給 worker 的補充說明（例如原本在等的東西被交接打斷了）。
    Note { text: String },
    /// 上一次 run 沒跑完（服務中斷）。
    Retry { reason: String },
}

impl WorkerTrigger {
    fn describe(&self) -> String {
        match self {
            Self::Assigned => "這個任務剛派給你，請開始。".into(),
            Self::Wake { waited, result } => format!(
                "你之前在{}，現在有結果了：{}",
                waited.describe(),
                tasks::one_line(result, 1000)
            ),
            Self::Handoff { from, text, .. } => {
                format!("{from}交接給你：{}", tasks::one_line(text, 2000))
            }
            Self::Decision { text } => {
                format!("Planner 回答了：{}", tasks::one_line(text, 2000))
            }
            Self::Note { text } => tasks::one_line(text, 1000),
            Self::Retry { reason } => {
                format!("上一次執行沒有完成（{reason}），請檢查工作目錄的現況後接著做。")
            }
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TriggerList {
    triggers: Vec<WorkerTrigger>,
}

/// `agent_runs.trigger` 可能是清單，或 M3 寫的單一觸發。
fn parse_triggers(text: &str) -> Vec<WorkerTrigger> {
    if let Ok(list) = serde_json::from_str::<TriggerList>(text) {
        return list.triggers;
    }
    serde_json::from_str::<WorkerTrigger>(text)
        .map(|t| vec![t])
        .unwrap_or_default()
}

/// 排一個 worker run；同一個任務已經有排隊中的 run 時把觸發併進去。要和觸發它的狀態變更放在同一個
/// transaction，commit 後呼叫 [`Worker::kick`]。
pub async fn enqueue(
    conn: &mut SqliteConnection,
    project: ProjectId,
    goal: GoalId,
    task: TaskId,
    trigger: &WorkerTrigger,
) -> Result<(), Error> {
    // 沒有 repo 的任務（project 還沒設定 repo 時建立的）不派給 worker。
    let repo: Option<Option<String>> = sqlx::query_scalar("SELECT repo FROM tasks WHERE id = ?")
        .bind(task.to_string())
        .fetch_optional(&mut *conn)
        .await?;
    if !matches!(repo, Some(Some(_))) {
        return Ok(());
    }
    let queued = sqlx::query(
        "SELECT id, trigger FROM agent_runs WHERE agent = 'worker' AND task_id = ? AND state = 'queued' \
         ORDER BY started_at, id LIMIT 1",
    )
    .bind(task.to_string())
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(row) = queued {
        let id: String = row.try_get("id")?;
        let text: String = row.try_get("trigger")?;
        let mut list = TriggerList {
            triggers: parse_triggers(&text),
        };
        list.triggers.push(trigger.clone());
        sqlx::query("UPDATE agent_runs SET trigger = ? WHERE id = ?")
            .bind(serde_json::to_string(&list)?)
            .bind(id)
            .execute(conn)
            .await?;
        return Ok(());
    }
    let list = TriggerList {
        triggers: vec![trigger.clone()],
    };
    sqlx::query(
        "INSERT INTO agent_runs (id, project_id, agent, trigger, state, started_at, goal_id, task_id) \
         VALUES (?, ?, 'worker', ?, 'queued', ?, ?, ?)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(project.to_string())
    .bind(serde_json::to_string(&list)?)
    .bind(Timestamp::now().0)
    .bind(goal.to_string())
    .bind(task.to_string())
    .execute(conn)
    .await?;
    Ok(())
}

/// 啟動時：上次沒跑完的 worker run 放回佇列（附上「上次沒完成」），重試用完的標成失敗、任務轉
/// `NeedsHuman`。回傳 (放回幾個, 放棄的任務)。
pub async fn requeue_stale(conn: &mut SqliteConnection) -> Result<(u64, Vec<TaskId>), Error> {
    let now = Timestamp::now().0;
    let rows = sqlx::query(
        "SELECT id, task_id, attempts, trigger FROM agent_runs WHERE agent = 'worker' AND state = 'running'",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut requeued = 0;
    let mut gave_up = Vec::new();
    for row in rows {
        let id: String = row.try_get("id")?;
        let task: Option<String> = row.try_get("task_id")?;
        let attempts: i64 = row.try_get("attempts")?;
        let text: String = row.try_get("trigger")?;
        if attempts >= MAX_ATTEMPTS {
            sqlx::query(
                "UPDATE agent_runs SET state = 'failed', error = '服務中斷，重試次數已用完', \
                 finished_at = ?, lease_until = NULL WHERE id = ?",
            )
            .bind(now)
            .bind(&id)
            .execute(&mut *conn)
            .await?;
            if let Some(t) = task {
                gave_up.push(parse_id(t)?);
            }
            continue;
        }
        let mut list = TriggerList {
            triggers: parse_triggers(&text),
        };
        list.triggers.push(WorkerTrigger::Retry {
            reason: "服務中斷".into(),
        });
        sqlx::query(
            "UPDATE agent_runs SET state = 'queued', lease_until = NULL, trigger = ? WHERE id = ?",
        )
        .bind(serde_json::to_string(&list)?)
        .bind(&id)
        .execute(&mut *conn)
        .await?;
        requeued += 1;
    }
    Ok((requeued, gave_up))
}

/// 重試用完的任務：轉 `NeedsHuman` 並叫醒 Planner。
pub async fn note_gave_up(txn: &mut LogTxn<'_>, tasks_: &[TaskId]) -> Result<(), Error> {
    for &id in tasks_ {
        if let Some(t) = tasks::task(txn.conn(), id).await?
            && t.state == TaskState::Running
        {
            end_task(
                txn,
                &t,
                TaskEnd::NeedsHuman("worker 沒有完成：服務中斷，重試次數已用完".into()),
                &Principal::System,
                None,
            )
            .await?;
        }
    }
    Ok(())
}

/// 任務在哪个工作資料夾做（`WorkDir::key()`）：project 的工作資料夾；沒有設定時退回「唯一監看
/// 的 repo」（onboarding 之前的 project）。`arg` 只能是同一個資料夾（Planner 的舊參數）。
pub async fn pick_workdir(
    conn: &mut SqliteConnection,
    project: ProjectId,
    arg: Option<&str>,
) -> Result<Result<String, String>, Error> {
    let workdir = match crate::store::Store::workdir(conn, project).await? {
        Some(dir) => Some(dir),
        // onboarding 之前的 project 只有自動化分頁的「監看的 repo」：一個 repo 就照舊派工。
        None => match crate::automation::project_repos(conn, project)
            .await?
            .as_slice()
        {
            [one] => Some(pots_proto::WorkDir::Github { repo: one.clone() }),
            _ => None,
        },
    };
    let Some(workdir) = workdir else {
        return Ok(Err(
            "這個 project 還沒有工作資料夾：請使用者在新增專案的精靈或「設定工作資料夾」裡指定 GitHub repo 或本機資料夾".into(),
        ));
    };
    let key = workdir.key();
    let Some(raw) = arg.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(Ok(key));
    };
    let wanted = if raw.starts_with("local:") {
        raw.to_string()
    } else if std::path::Path::new(raw).is_absolute() {
        format!("local:{raw}")
    } else {
        match crate::watcher::event::normalize_repo(raw) {
            Ok(repo) => repo,
            Err(e) => return Ok(Err(e)),
        }
    };
    if wanted == key {
        Ok(Ok(key))
    } else {
        Ok(Err(format!(
            "這個 project 的工作資料夾是 {}，不是 {wanted}",
            workdir.label()
        )))
    }
}

/// 任務怎麼結束。
#[derive(Clone, Debug)]
pub enum TaskEnd {
    Done(String),
    Failed(String),
    NeedsHuman(String),
    /// 快速修改超出範圍：升級成一般 Goal 交給 Planner。一般 Goal 的 escalate 是 `NeedsHuman`。
    Escalate(String),
}

/// [`end_task`] 之後呼叫端在 commit 之後要做的事。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ends {
    /// 叫醒 Planner。
    pub planner: bool,
    /// 這個 Goal 結束了：中斷它其他正在跑的 run（[`Worker::close_goal`]）。
    pub closed: Option<GoalId>,
}

impl Ends {
    /// commit 之後通知 runner。`me` 是呼叫的任務（它自己的 run 正在收尾，不要中斷它）。
    pub fn notify(self, worker: &Worker, me: Option<TaskId>) {
        if self.planner {
            worker.kick_planner();
        }
        if let Some(g) = self.closed {
            worker.close_goal_except(g, me);
        }
    }
}

/// 結束任務：改狀態、叫醒 Planner（快速修改：結束 Goal 或升級、貼卡片），記 `GoalChanged`。
/// `run` 是發卡片的 run。
pub async fn end_task(
    txn: &mut LogTxn<'_>,
    task: &TaskRow,
    end: TaskEnd,
    by: &Principal,
    run: Option<Uuid>,
) -> Result<Ends, Error> {
    let Some(goal) = tasks::goal(txn.conn(), task.goal).await? else {
        return Ok(Ends::default());
    };
    let quick = tasks::goal_kind(txn.conn(), goal.id).await? == "quick";
    let (state, label, detail) = match &end {
        TaskEnd::Done(s) => (TaskState::Done { outcome: s.clone() }, "完成", s.clone()),
        TaskEnd::Failed(e) => (TaskState::Failed { error: e.clone() }, "失敗", e.clone()),
        TaskEnd::NeedsHuman(r) => (
            TaskState::NeedsHuman { reason: r.clone() },
            "需要決定",
            r.clone(),
        ),
        TaskEnd::Escalate(r) => (
            TaskState::NeedsHuman {
                reason: format!("升級：{r}"),
            },
            "升級",
            r.clone(),
        ),
    };
    tasks::set_task_state(
        txn.conn(),
        task,
        &state,
        &Cause {
            by: by.clone(),
            reason: tasks::one_line(&detail, 300),
        },
    )
    .await?;
    let mut ends = Ends::default();
    if !goal.state.is_active() {
        tasks::push_goal(txn, goal.id).await?;
        return Ok(ends);
    }
    let pr = task
        .pr_url
        .as_deref()
        .map(|u| format!("（{u}）"))
        .unwrap_or_default();
    if quick {
        match end {
            TaskEnd::Done(s) => {
                let summary = tasks::one_line(&s, 500);
                crate::tools::close_goal(
                    txn,
                    &goal,
                    GoalState::Done {
                        summary: summary.clone(),
                    },
                    by,
                    "快速修改完成",
                )
                .await?;
                ends.closed = Some(goal.id);
                crate::tools::post_card(
                    txn,
                    goal.project,
                    Card::Milestone {
                        goal: goal.id,
                        text: format!("快速修改完成：{summary}{pr}"),
                    },
                    run,
                )
                .await?;
            }
            TaskEnd::Failed(e) => {
                let e = tasks::one_line(&e, 500);
                crate::tools::close_goal(
                    txn,
                    &goal,
                    GoalState::Cancelled {
                        reason: format!("worker 沒有完成：{e}"),
                    },
                    by,
                    "快速修改失敗",
                )
                .await?;
                ends.closed = Some(goal.id);
                crate::tools::post_card(
                    txn,
                    goal.project,
                    Card::Milestone {
                        goal: goal.id,
                        text: format!("快速修改沒有完成：{e}{pr}"),
                    },
                    run,
                )
                .await?;
            }
            TaskEnd::Escalate(r) => {
                upgrade_quick(txn, &goal, task, &r).await?;
                ends.planner = true;
                crate::tools::post_card(
                    txn,
                    goal.project,
                    Card::Milestone {
                        goal: goal.id,
                        text: format!(
                            "快速修改超出範圍，已升級成一般 Goal 交給 Planner：{}",
                            tasks::one_line(&r, 300)
                        ),
                    },
                    run,
                )
                .await?;
            }
            TaskEnd::NeedsHuman(r) => {
                tasks::push_goal(txn, goal.id).await?;
                crate::tools::post_card(
                    txn,
                    goal.project,
                    Card::Milestone {
                        goal: goal.id,
                        text: format!("快速修改卡住了：{}{pr}", tasks::one_line(&r, 500)),
                    },
                    run,
                )
                .await?;
            }
        }
        return Ok(ends);
    }
    planner::enqueue(
        txn.conn(),
        goal.project,
        goal.id,
        &PlannerTrigger::TaskEnded {
            task: task.id,
            title: task.title.clone(),
            state: label.into(),
            detail: tasks::one_line(&format!("{detail}{pr}"), 1500),
        },
    )
    .await?;
    tasks::push_goal(txn, goal.id).await?;
    ends.planner = true;
    Ok(ends)
}

/// 快速修改升級成一般 Goal：改種類、在 Brief 補上原因與已做的分支，排 Planner。
async fn upgrade_quick(
    txn: &mut LogTxn<'_>,
    goal: &GoalRow,
    task: &TaskRow,
    reason: &str,
) -> Result<(), Error> {
    tasks::set_goal_kind(txn.conn(), goal.id, "feature").await?;
    if let Some(brief) =
        artifacts::for_goal(txn.conn(), goal.id, pots_proto::ArtifactKind::Brief).await?
    {
        // worker 寫的原因以引用區塊放進 Brief，標明出處（不是使用者的要求）。
        let quoted = crate::watcher::event::quote_untrusted(reason.trim());
        let mut body =
            format!("快速修改的 worker 回報超出範圍（以下是 worker 的說法）：\n\n{quoted}");
        if let Some(b) = &task.branch {
            body.push_str(&format!("\n\n已做的修改在分支 `{b}`"));
            if let Some(pr) = &task.pr_url {
                body.push_str(&format!("（PR：{pr}）"));
            }
            body.push('。');
        }
        let outcome = artifacts::agent_write(
            txn.conn(),
            &brief,
            brief.rev,
            &[Edit::AppendSection {
                heading: "升級原因".into(),
                body_md: body,
            }],
            &Principal::System,
            &format!("worker:{}", task.id),
        )
        .await?;
        if let Ok(artifacts::WriteOutcome::Written(new)) = outcome {
            artifacts::push_changed(txn, &new).await?;
        }
    }
    planner::enqueue(
        txn.conn(),
        goal.project,
        goal.id,
        &PlannerTrigger::Escalated {
            reason: tasks::one_line(reason, 1000),
        },
    )
    .await?;
    tasks::push_goal(txn, goal.id).await
}

/// 開一個快速修改：quick Goal（Brief 是描述）加一個任務，直接派給 worker，不經 Planner。
#[allow(clippy::too_many_arguments)]
pub async fn open_quick(
    txn: &mut LogTxn<'_>,
    project: ProjectId,
    title: &str,
    description: &str,
    repo: &str,
    by: &Principal,
    reader: Option<&str>,
) -> Result<Result<(GoalId, TaskId), String>, Error> {
    let brief = format!("## 要做的事\n\n{}", description.trim());
    let (goal, _) = match crate::tools::open_goal_with_brief(
        txn,
        project,
        title,
        &brief,
        by,
        &by.label(),
        reader,
        true,
    )
    .await?
    {
        Ok(g) => g,
        Err(e) => return Ok(Err(e)),
    };
    let title = tasks::one_line(title, tasks::MAX_TITLE_CHARS);
    let mut task = TaskRow::new(goal, title, tasks::truncate(description, 2000));
    task.repo = Some(repo.to_string());
    task.assignee = "worker".into();
    tasks::insert_task(
        txn.conn(),
        &task,
        &Cause {
            by: by.clone(),
            reason: "快速修改".into(),
        },
    )
    .await?;
    enqueue(txn.conn(), project, goal, task.id, &WorkerTrigger::Assigned).await?;
    tasks::push_goal(txn, goal).await?;
    Ok(Ok((goal, task.id)))
}

// ---------------------------------------------------------------------------
// 設定

/// 目前的 worker 設定（資料庫沒有時用 `defaults`）。
pub async fn load_settings(
    conn: &mut SqliteConnection,
    defaults: &WorkerSettingsDraft,
) -> Result<WorkerSettingsDraft, Error> {
    let text: Option<String> =
        sqlx::query_scalar("SELECT value FROM instance_settings WHERE key = ?")
            .bind(SETTING_KEY)
            .fetch_optional(conn)
            .await?;
    Ok(text
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| defaults.clone()))
}

/// 檢查設定；不合法時回說明。
pub fn check_settings(d: &WorkerSettingsDraft) -> Result<WorkerSettingsDraft, String> {
    let runtime = d.runtime.trim();
    let image = d.image.trim();
    let model = d.claude_model.trim();
    let token = |s: &str| {
        !s.is_empty()
            && !s.starts_with('-')
            && s.len() <= 200
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./:@+".contains(c))
    };
    if !token(runtime) {
        return Err("容器執行檔要是 docker、podman 或它們的路徑".into());
    }
    if !token(image) {
        return Err("image 名稱不對".into());
    }
    if !model.is_empty() && !token(model) {
        return Err("Claude Code 的模型名稱不對".into());
    }
    if !(1..=8).contains(&d.max_parallel) {
        return Err("同時執行的 worker 要在 1 到 8 個之間".into());
    }
    Ok(WorkerSettingsDraft {
        backend: d.backend,
        exec: d.exec,
        runtime: runtime.into(),
        image: image.into(),
        claude_model: model.into(),
        max_parallel: d.max_parallel,
    })
}

/// Claude Code 的設定目錄（容器的 `CLAUDE_CONFIG_DIR`：官方登入與 session），所有任務共用。
pub fn claude_config(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("claude-config")
}

/// 任務自己的 home（容器的 Claude Code worker）。
fn task_home(data_dir: &std::path::Path, task: TaskId) -> PathBuf {
    data_dir.join("worker-homes").join(task.to_string())
}

/// 設定畫面的檢視（含登入指令）。
pub fn settings_view(d: &WorkerSettingsDraft, data_dir: &std::path::Path) -> WorkerSettingsView {
    let login_command = match d.exec {
        ExecKind::Container => {
            #[cfg(unix)]
            // SAFETY: getuid／getgid 沒有前置條件。
            let user = unsafe { format!(" --user {}:{}", libc::getuid(), libc::getgid()) };
            #[cfg(not(unix))]
            let user = String::new();
            format!(
                "mkdir -p {cfg} && {rt} run -it --rm{user} -v {cfg}:{cc} -e CLAUDE_CONFIG_DIR={cc} -- {img} claude",
                cfg = code::shell_quote(&claude_config(data_dir).display().to_string()),
                rt = d.runtime,
                cc = exec::CONTAINER_CLAUDE_CONFIG,
                img = d.image,
            )
        }
        ExecKind::Local => "claude".into(),
    };
    WorkerSettingsView {
        backend: d.backend,
        exec: d.exec,
        runtime: d.runtime.clone(),
        image: d.image.clone(),
        claude_model: d.claude_model.clone(),
        login_command,
        max_parallel: d.max_parallel,
    }
}

/// 儲存設定並記一筆 `Worker` 事件。
pub async fn save_settings(
    txn: &mut LogTxn<'_>,
    draft: &WorkerSettingsDraft,
    data_dir: &std::path::Path,
) -> Result<(), Error> {
    Store::set_setting(txn.conn(), SETTING_KEY, &serde_json::to_string(draft)?).await?;
    let worker = settings_view(draft, data_dir);
    txn.push(|seq| ServerEvent::Worker { seq, worker }).await
}

// ---------------------------------------------------------------------------
// runner

/// 一次 run 的工作位置（給 `open_pr`、`edit_file`、`run`）。
pub struct WorkCtx {
    pub env: Arc<Env>,
    pub source: RepoSource,
    pub mirror: PathBuf,
    pub checkout: Checkout,
    pub quick: bool,
    pub task: TaskId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopReason {
    /// 任務被取消或 Goal 結束。
    Closed,
    /// 超過執行時間上限。
    Timeout,
}

struct Active {
    run: Uuid,
    goal: GoalId,
    cancel: CancellationToken,
    reason: Option<StopReason>,
}

struct Inner {
    hub: Arc<Hub>,
    store: Store,
    router: Arc<ProviderRouter>,
    profiles: Profiles,
    config: WorkerConfig,
    data_dir: PathBuf,
    instance: String,
    /// 這次啟動的 ID（容器的標籤：啟動時只清其他次啟動留下的）。
    boot: String,
    clock: Clock,
    repos: Repos,
    grants: Arc<mcp::Grants>,
    port: OnceLock<u16>,
    planner: OnceLock<Planner>,
    watcher: OnceLock<Watcher>,
    notify: Notify,
    active: Mutex<HashMap<TaskId, Active>>,
    tracker: TaskTracker,
    shutdown: CancellationToken,
}

#[derive(Clone)]
pub struct Worker {
    inner: Arc<Inner>,
}

struct Job {
    run: Uuid,
    project: ProjectId,
    goal: GoalId,
    task: TaskId,
    triggers: Vec<WorkerTrigger>,
    cancel: CancellationToken,
}

/// run 怎麼結束（還沒寫進任務狀態的部分）。
enum RunEnd {
    /// 任務已經不在、已結束或 Goal 結束了：沒有跑。
    Skipped,
    /// worker 呼叫了結束工具（狀態已由工具寫好）。
    Finished,
    /// 被中斷：任務被取消、Goal 結束、服務關閉或逾時。
    Cancelled,
    /// 沒有呼叫結束工具就停了、或出錯：任務轉 `NeedsHuman`。
    Stuck(String),
}

impl Worker {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        hub: Arc<Hub>,
        store: Store,
        router: Arc<ProviderRouter>,
        profiles: Profiles,
        config: WorkerConfig,
        data_dir: PathBuf,
        instance: String,
        clock: Clock,
    ) -> Self {
        let repos = Repos::new(&data_dir, config.git_base.clone());
        Self {
            inner: Arc::new(Inner {
                hub,
                store,
                router,
                profiles,
                config,
                data_dir,
                instance,
                boot: Uuid::new_v4().to_string(),
                clock,
                repos,
                grants: mcp::Grants::new(),
                port: OnceLock::new(),
                planner: OnceLock::new(),
                watcher: OnceLock::new(),
                notify: Notify::new(),
                active: Mutex::new(HashMap::new()),
                tracker: TaskTracker::new(),
                shutdown: CancellationToken::new(),
            }),
        }
    }

    /// 接上 Planner 與 watcher（它們也要用到 worker，所以晚一步）。
    pub fn bind(&self, planner: Planner, watcher: Watcher) {
        let _ = self.inner.planner.set(planner);
        let _ = self.inner.watcher.set(watcher);
    }

    /// 服務的 port（Claude Code 經 MCP 連回來）。
    pub fn set_port(&self, port: u16) {
        let _ = self.inner.port.set(port);
    }

    pub fn grants(&self) -> Arc<mcp::Grants> {
        self.inner.grants.clone()
    }

    pub fn repos(&self) -> &Repos {
        &self.inner.repos
    }

    pub fn profiles(&self) -> &Profiles {
        &self.inner.profiles
    }

    pub fn router(&self) -> &Arc<ProviderRouter> {
        &self.inner.router
    }

    pub fn data_dir(&self) -> &std::path::Path {
        &self.inner.data_dir
    }

    pub fn watcher(&self) -> Option<&Watcher> {
        self.inner.watcher.get()
    }

    fn planner(&self) -> Option<&Planner> {
        self.inner.planner.get()
    }

    pub fn kick_planner(&self) {
        if let Some(p) = self.planner() {
            p.kick();
        }
    }

    /// 目前的設定。
    pub async fn settings(&self) -> Result<WorkerSettingsDraft, Error> {
        let mut conn = self.inner.store.acquire().await?;
        load_settings(&mut conn, &self.inner.config.defaults).await
    }

    /// 開始處理佇列；容器模式時先清掉上次留下的容器。
    pub fn start(&self) {
        if !self.inner.config.run_queue {
            return;
        }
        let w = self.clone();
        self.inner.tracker.spawn(async move { w.runner().await });
        // 上次當機留下的容器：只在用過容器 worker 時清（不要每次啟動都去叫 docker），關閉時放棄。
        let w = self.clone();
        self.inner.tracker.spawn(async move {
            let used = async {
                let mut conn = w.inner.store.acquire().await?;
                let n: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM agent_runs WHERE agent = 'worker' AND attempts > 0",
                )
                .fetch_one(&mut *conn)
                .await?;
                Ok::<_, Error>(n > 0)
            };
            if let (Ok(s), Ok(true)) = (w.settings().await, used.await)
                && s.exec == ExecKind::Container
            {
                tokio::select! {
                    () = w.inner.shutdown.cancelled() => {}
                    () = exec::cleanup_stale(&s.runtime, &w.inner.instance, &w.inner.boot) => {}
                }
            }
        });
    }

    pub fn kick(&self) {
        self.inner.notify.notify_one();
    }

    /// 中斷這個任務正在跑的 run（任務被取消）。
    pub fn interrupt(&self, task: TaskId) {
        if let Some(a) = self.inner.active.lock().unwrap().get_mut(&task) {
            a.reason = Some(StopReason::Closed);
            a.cancel.cancel();
        }
    }

    /// Goal 結束了：中斷它所有正在跑的 run。
    pub fn close_goal(&self, goal: GoalId) {
        self.close_goal_except(goal, None);
    }

    /// 同上，但不中斷 `except` 這個任務的 run。
    pub fn close_goal_except(&self, goal: GoalId, except: Option<TaskId>) {
        for (task, a) in self.inner.active.lock().unwrap().iter_mut() {
            if a.goal == goal && Some(*task) != except {
                a.reason = Some(StopReason::Closed);
                a.cancel.cancel();
            }
        }
    }

    /// 中斷所有 run 並等它們收尾；被中斷的 run 回到佇列。Claude Code 要時間收尾（interrupt 之後的
    /// 寬限加上結束行程），所以至少等兩倍的寬限。
    pub async fn shutdown(&self, timeout: Duration) {
        self.inner.shutdown.cancel();
        self.inner.tracker.close();
        let timeout = timeout.max(self.inner.config.grace * 2 + Duration::from_secs(5));
        if tokio::time::timeout(timeout, self.inner.tracker.wait())
            .await
            .is_err()
        {
            tracing::warn!("worker run 沒有在時限內結束");
        }
    }

    async fn runner(&self) {
        loop {
            let max = self
                .settings()
                .await
                .map(|s| s.max_parallel as usize)
                .unwrap_or(1)
                .max(1);
            while !self.inner.shutdown.is_cancelled()
                && self.inner.active.lock().unwrap().len() < max
            {
                match self.claim().await {
                    Ok(Some(job)) => {
                        let w = self.clone();
                        self.inner
                            .tracker
                            .spawn(async move { w.execute(job).await });
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::error!(error = %e, "無法從佇列取出 worker run");
                        break;
                    }
                }
            }
            tokio::select! {
                () = self.inner.shutdown.cancelled() => return,
                () = self.inner.notify.notified() => {}
                () = tokio::time::sleep(POLL) => {}
            }
        }
    }

    /// 取出下一個可以跑的 run：`queued`、同一個任務沒有 run 在跑。任務已結束或 Goal 已結束的直接
    /// 標成取消；重試次數用完的標成失敗、任務轉 `NeedsHuman`。取走時任務變成 `Running`。
    async fn claim(&self) -> Result<Option<Job>, Error> {
        let inner = &self.inner;
        loop {
            let mut txn = inner.hub.begin().await?;
            let now = Timestamp::now().0;
            let row = sqlx::query(
                "SELECT r.id, r.project_id, r.goal_id, r.task_id, r.trigger, r.attempts FROM agent_runs r \
                 WHERE r.agent = 'worker' AND r.task_id IS NOT NULL AND r.state = 'queued' \
                 AND NOT EXISTS (SELECT 1 FROM agent_runs x WHERE x.task_id = r.task_id \
                     AND x.agent = 'worker' AND x.state = 'running') \
                 AND NOT EXISTS (SELECT 1 FROM projects p WHERE p.id = r.project_id \
                     AND p.breaker_until IS NOT NULL) \
                 ORDER BY r.started_at, r.id LIMIT 1",
            )
            .fetch_optional(txn.conn())
            .await?;
            let Some(row) = row else {
                return Ok(None);
            };
            let run: Uuid = parse_id(row.try_get("id")?)?;
            let project: ProjectId = parse_id(row.try_get("project_id")?)?;
            let task_id: TaskId = parse_id(row.try_get::<String, _>("task_id")?)?;
            let attempts: i64 = row.try_get("attempts")?;
            let triggers = parse_triggers(&row.try_get::<String, _>("trigger")?);
            let task = tasks::task(txn.conn(), task_id).await?;
            let goal = match &task {
                Some(t) => tasks::goal(txn.conn(), t.goal).await?,
                None => None,
            };
            // 交接（或 Planner 的回答）在任務等待或需要決定時也要送到：任務回到執行。
            // worker 的交接只叫醒等待中的任務；Planner 的交接與決定也能叫醒需要決定的任務。
            let handoff = triggers.iter().any(|t| {
                matches!(
                    t,
                    WorkerTrigger::Handoff { .. } | WorkerTrigger::Decision { .. }
                )
            });
            let from_planner = triggers.iter().any(|t| {
                matches!(
                    t,
                    WorkerTrigger::Handoff { planner: true, .. } | WorkerTrigger::Decision { .. }
                )
            });
            let runnable = matches!(
                (&task, &goal),
                (Some(t), Some(g)) if g.state.is_active()
                    && t.repo.is_some()
                    && (matches!(t.state, TaskState::Planned | TaskState::Running)
                        || (handoff && matches!(t.state, TaskState::Waiting { .. }))
                        || (from_planner && matches!(t.state, TaskState::NeedsHuman { .. })))
            );
            if !runnable || attempts >= MAX_ATTEMPTS {
                let (state, error) = if runnable {
                    ("failed", Some("重試次數已用完"))
                } else {
                    ("cancelled", None)
                };
                sqlx::query(
                    "UPDATE agent_runs SET state = ?, error = ?, finished_at = ?, lease_until = NULL WHERE id = ?",
                )
                .bind(state)
                .bind(error)
                .bind(now)
                .bind(run.to_string())
                .execute(txn.conn())
                .await?;
                if runnable && let Some(t) = &task {
                    let ends = if t.state == TaskState::Running {
                        end_task(
                            &mut txn,
                            t,
                            TaskEnd::NeedsHuman("worker 沒有完成：重試次數已用完".into()),
                            &Principal::System,
                            None,
                        )
                        .await?
                    } else {
                        Ends::default()
                    };
                    let w = self.clone();
                    txn.commit_then(|| ends.notify(&w, None)).await?;
                } else {
                    txn.commit().await?;
                }
                continue;
            }
            let (task, goal) = (task.unwrap(), goal.unwrap());
            let mut triggers = triggers;
            // 原本在等的東西被交接打斷了（訂閱會清掉）：告訴 worker。
            if let TaskState::Waiting { on, .. } = &task.state {
                let again = if matches!(on, WaitCond::Approval { .. }) {
                    "那筆核准已經撤回；需要的話處理完再用 request_approval 申請一次。"
                } else {
                    "需要的話處理完再用 wait_for 等一次。"
                };
                triggers.push(WorkerTrigger::Note {
                    text: format!("你原本在{}，被上面的訊息叫醒；{again}", on.describe()),
                });
            }
            sqlx::query(
                "UPDATE agent_runs SET state = 'running', lease_until = ?, started_at = ?, \
                 attempts = attempts + 1 WHERE id = ?",
            )
            .bind(now + LEASE.as_secs() as i64)
            .bind(now)
            .bind(run.to_string())
            .execute(txn.conn())
            .await?;
            if task.state != TaskState::Running {
                tasks::set_task_state(
                    txn.conn(),
                    &task,
                    &TaskState::Running,
                    &Cause {
                        by: Principal::System,
                        reason: String::new(),
                    },
                )
                .await?;
            }
            tasks::push_goal(&mut txn, goal.id).await?;
            let cancel = inner.shutdown.child_token();
            txn.commit_then(|| {
                inner.active.lock().unwrap().insert(
                    task_id,
                    Active {
                        run,
                        goal: goal.id,
                        cancel: cancel.clone(),
                        reason: None,
                    },
                );
            })
            .await?;
            return Ok(Some(Job {
                run,
                project,
                goal: goal.id,
                task: task_id,
                triggers,
                cancel,
            }));
        }
    }

    #[tracing::instrument(
        name = "agent.run",
        skip_all,
        fields(agent = "worker", project = %job.project, run = %job.run, goal = %job.goal, task = %job.task, model = tracing::field::Empty)
    )]
    async fn execute(&self, job: Job) {
        let inner = &self.inner;
        let beat = CancellationToken::new();
        let heartbeat = {
            let store = inner.store.clone();
            let beat = beat.clone();
            let run = job.run;
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        () = beat.cancelled() => return,
                        () = tokio::time::sleep(HEARTBEAT) => {}
                    }
                    if let Err(e) = extend_lease(&store, run).await {
                        tracing::warn!(%run, error = %e, "無法延長 worker run 的 lease");
                    }
                }
            })
        };
        // 執行時間上限。
        let timer = {
            let w = self.clone();
            let task = job.task;
            let limit = inner.config.run_timeout;
            let beat = beat.clone();
            tokio::spawn(async move {
                tokio::select! {
                    () = beat.cancelled() => {}
                    () = tokio::time::sleep(limit) => {
                        if let Some(a) = w.inner.active.lock().unwrap().get_mut(&task) {
                            a.reason.get_or_insert(StopReason::Timeout);
                            a.cancel.cancel();
                        }
                    }
                }
            })
        };

        let result = self.run(&job).await;
        beat.cancel();
        let _ = heartbeat.await;
        let _ = timer.await;

        let reason = {
            let mut active = inner.active.lock().unwrap();
            let reason = active.get(&job.task).and_then(|a| a.reason);
            if active.get(&job.task).is_some_and(|a| a.run == job.run) {
                active.remove(&job.task);
            }
            reason
        };
        if let Err(e) = self.finish(&job, result, reason).await {
            tracing::error!(run = %job.run, error = %e, "無法記錄 worker run 的結束");
            if let Err(e) = mark_failed(&inner.store, job.run).await {
                tracing::error!(run = %job.run, error = %e, "無法把 worker run 標成失敗");
            }
        }
        self.kick();
    }

    /// 跑一次 worker。
    async fn run(&self, job: &Job) -> Result<RunEnd, Error> {
        let inner = &self.inner;
        let settings = self.settings().await?;
        let Some(project) = inner.store.project(job.project).await? else {
            return Ok(RunEnd::Skipped);
        };
        let (task, goal, quick) = {
            let mut conn = inner.store.acquire().await?;
            let Some(task) = tasks::task(&mut conn, job.task).await? else {
                return Ok(RunEnd::Skipped);
            };
            let Some(goal) = tasks::goal(&mut conn, job.goal).await? else {
                return Ok(RunEnd::Skipped);
            };
            let quick = tasks::goal_kind(&mut conn, goal.id).await? == "quick";
            (task, goal, quick)
        };
        if !goal.state.is_active() || task.state != TaskState::Running {
            return Ok(RunEnd::Skipped);
        }
        let label = match settings.backend {
            WorkerBackendKind::Builtin => "worker · 內建",
            WorkerBackendKind::ClaudeCode => "worker · Claude Code",
        };
        {
            let mut txn = inner.hub.begin().await?;
            tasks::set_assignee(txn.conn(), task.id, label).await?;
            tasks::set_task_work(txn.conn(), task.id, None, None, Some("準備工作目錄…")).await?;
            tasks::push_goal(&mut txn, goal.id).await?;
            txn.commit().await?;
        }

        // 工作目錄。
        let Some(repo) = task.repo.clone() else {
            return Ok(RunEnd::Stuck("任務沒有指定 repo".into()));
        };
        let source = match RepoSource::parse(&repo) {
            Ok(s) => s,
            Err(e) => return Ok(RunEnd::Stuck(e)),
        };
        let Some(watcher) = self.watcher() else {
            return Ok(RunEnd::Stuck("服務還沒準備好".into()));
        };
        let gh = watcher.github();
        let gh_config = {
            let mut conn = inner.store.acquire().await?;
            crate::watcher::github_config(&mut conn).await?
        };
        // 本機資料夾沒有 remote：不帶 GitHub token。
        let token = if source.local_path().is_some() {
            None
        } else {
            gh.token()
        };
        let url = inner.repos.remote_url(&source, &gh.api(&gh_config));
        // 不和取消賽跑：中途被殺掉的 git 會留下 lock 檔（工作目錄本身是做好才改名的）。
        let prepared = async {
            let (mirror, base) = inner.repos.sync(&source, &url, token.as_deref()).await?;
            let branch = task
                .branch
                .clone()
                .unwrap_or_else(|| repo::branch_name(task.id, &task.title));
            let checkout = inner
                .repos
                .checkout(&source, &mirror, &base, task.id, &branch)
                .await?;
            Ok::<_, repo::RepoError>((mirror, checkout))
        }
        .await;
        let (mirror, checkout) = match prepared {
            Ok(p) => p,
            Err(e) => return Ok(RunEnd::Stuck(format!("無法準備 {repo} 的工作目錄：{e}"))),
        };
        if job.cancel.is_cancelled() {
            return Ok(RunEnd::Cancelled);
        }
        {
            let mut txn = inner.hub.begin().await?;
            tasks::set_task_base(txn.conn(), task.id, &checkout.base).await?;
            tasks::set_task_work(
                txn.conn(),
                task.id,
                Some(&checkout.branch),
                None,
                Some("啟動 worker…"),
            )
            .await?;
            tasks::push_goal(&mut txn, goal.id).await?;
            txn.commit().await?;
        }

        // 觸發訊息先組好（之後出錯時不會留下容器）。
        let message = {
            let mut txn = inner.hub.begin().await?;
            let m = render_prompt(&mut txn, &task, &goal, &job.triggers, &checkout, quick).await?;
            txn.commit().await?;
            m
        };

        // 執行環境。
        let spec = ExecSpec {
            kind: settings.exec,
            runtime: settings.runtime.clone(),
            image: settings.image.clone(),
            claude: (settings.backend == WorkerBackendKind::ClaudeCode).then(|| {
                exec::ClaudeMounts {
                    config: claude_config(&inner.data_dir),
                    home: task_home(&inner.data_dir, task.id),
                }
            }),
            instance: inner.instance.clone(),
            boot: inner.boot.clone(),
        };
        let run_name = job.run.to_string();
        let run_dir = inner.data_dir.join("worker-runs").join(&run_name);
        let task_name = task.id.to_string();
        let env = tokio::select! {
            () = job.cancel.cancelled() => return Ok(RunEnd::Cancelled),
            r = Env::start(&spec, &checkout.dir, &run_dir, &run_name, &task_name) => r,
        };
        let env = match env {
            Ok(e) => Arc::new(e),
            Err(e) => return Ok(RunEnd::Stuck(e)),
        };

        // USER.md 在最前面，Project Notes 接在 project 指示後面（Claude Code 一樣經 append-system-prompt
        // 帶入：本機模式的設定目錄是你自己的，容器的設定目錄由所有任務共用，都不適合寫 CLAUDE.md）。
        let (head, tail) = {
            let mut conn = inner.store.acquire().await?;
            crate::user_md::compose(&mut conn, project.id).await?
        };
        let mut instructions = format!("{head}{}", inner.profiles.worker.role_prompt());
        instructions.push_str(&format!(
            "\n\n這個任務屬於 project「{}」。",
            tasks::one_line(&project.name, 200)
        ));
        if !project.instructions.trim().is_empty() {
            instructions.push_str("\n\n# Project 指示\n\n");
            instructions.push_str(project.instructions.trim());
        }
        instructions.push_str(&tail);

        let work = Arc::new(WorkCtx {
            env: env.clone(),
            source,
            mirror,
            checkout: checkout.clone(),
            quick,
            task: task.id,
        });
        let result = match settings.backend {
            WorkerBackendKind::Builtin => {
                self.run_builtin(job, &task, work, instructions, message)
                    .await
            }
            WorkerBackendKind::ClaudeCode => {
                self.run_claude(job, &task, &settings, work, instructions, message)
                    .await
            }
        };
        match Arc::try_unwrap(env) {
            Ok(env) => env.stop().await,
            Err(env) => {
                // 還有工具拿著（不應該發生）：至少把容器收掉。
                if let Some(stop) = env.hard_stop() {
                    stop().await;
                }
            }
        }
        result
    }

    async fn tool_ctx(
        &self,
        job: &Job,
        task: &TaskRow,
        work: Arc<WorkCtx>,
        code: bool,
        meta: crate::budget::RunMeta,
    ) -> Result<Arc<ToolCtx>, Error> {
        let inner = &self.inner;
        let code: Option<Arc<dyn code::CodeAccess>> = code.then(|| {
            Arc::new(code::Worktree {
                env: work.env.clone(),
                base: work.checkout.base.clone(),
            }) as Arc<dyn code::CodeAccess>
        });
        let budget = crate::budget::for_run(
            &inner.store,
            &inner.hub,
            &inner.clock,
            job.run,
            job.project,
            Some(job.goal),
            Some(task.id),
            meta,
        )
        .await?;
        Ok(Arc::new(ToolCtx {
            hub: inner.hub.clone(),
            store: inner.store.clone(),
            planner: self.planner().expect("planner").clone(),
            project: job.project,
            scope: Scope::Task {
                goal: job.goal,
                task: task.id,
            },
            actor: Principal::agent("worker"),
            reader: format!("worker:{}", task.id),
            artifact_kinds: inner.profiles.worker.artifact_kinds.clone(),
            run_id: job.run,
            cards: Default::default(),
            queued: None,
            finished: Arc::new(AtomicBool::new(false)),
            budget,
            milestones: Default::default(),
            goals_created: Default::default(),
            clock: inner.clock.clone(),
            worker: self.clone(),
            work: Some(work),
            code,
            stop: Arc::new(Notify::new()),
            cancel: job.cancel.clone(),
        }))
    }

    async fn run_builtin(
        &self,
        job: &Job,
        task: &TaskRow,
        work: Arc<WorkCtx>,
        instructions: String,
        message: String,
    ) -> Result<RunEnd, Error> {
        let inner = &self.inner;
        let route = match inner.router.resolve() {
            Ok(r) => r,
            Err(e) => {
                return Ok(RunEnd::Stuck(format!(
                    "內建 worker 沒有可用的模型：{}",
                    explain(&e)
                )));
            }
        };
        tracing::Span::current().record("model", tracing::field::display(&route.model));
        let meta = route.meta("builtin");
        let ctx = self.tool_ctx(job, task, work, true, meta).await?;
        let config = RunConfig {
            reasoning: None,
            provider: route.provider,
            model: route.model,
            instructions: Some(instructions),
            tools: crate::tools::build(&inner.profiles.worker, ctx.clone()),
            hook: Arc::new(ctx.gate()),
            max_turns: inner.profiles.worker.max_turns,
            session_id: Some(format!("task:{}", task.id)),
        };
        let (events, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);
        let result = agent::run(
            &config,
            vec![Item::User { text: message }],
            events,
            job.cancel.clone(),
        )
        .await;
        let finished = ctx.finished.load(std::sync::atomic::Ordering::SeqCst);
        Ok(match result {
            _ if finished => RunEnd::Finished,
            Ok(r) => {
                let last = r
                    .new_items
                    .iter()
                    .rev()
                    .find_map(|i| match i {
                        Item::Assistant { text } if !text.trim().is_empty() => Some(text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                let why = if r.outcome == agent::RunOutcome::MaxTurns {
                    "worker 用完了單次的工具輪數上限，沒有回報結果".to_string()
                } else {
                    "worker 結束了這一輪，但沒有回報結果".to_string()
                };
                if last.trim().is_empty() {
                    RunEnd::Stuck(why)
                } else {
                    RunEnd::Stuck(format!("{why}。最後說：{}", tasks::one_line(&last, 500)))
                }
            }
            Err(LlmError::Cancelled) => RunEnd::Cancelled,
            Err(e) => RunEnd::Stuck(format!("內建 worker 出錯：{}", explain(&e))),
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_claude(
        &self,
        job: &Job,
        task: &TaskRow,
        settings: &WorkerSettingsDraft,
        work: Arc<WorkCtx>,
        instructions: String,
        message: String,
    ) -> Result<RunEnd, Error> {
        let inner = &self.inner;
        let Some(port) = inner.port.get().copied() else {
            return Ok(RunEnd::Stuck("服務還沒準備好".into()));
        };
        let env = work.env.clone();
        // 平台工具：worker profile 去掉 Claude Code 自己就有的（改檔、執行、subagent）。
        let mut profile: AgentProfile = inner.profiles.worker.clone();
        profile.tools.retain(|t| {
            !matches!(
                t,
                ToolName::EditFile | ToolName::Run | ToolName::RunSubagent
            )
        });
        let meta = crate::budget::RunMeta::claude_code(&settings.claude_model);
        tracing::Span::current().record(
            "model",
            tracing::field::display(if settings.claude_model.is_empty() {
                "claude-code"
            } else {
                settings.claude_model.as_str()
            }),
        );
        let ctx = self.tool_ctx(job, task, work, false, meta).await?;
        let tools = crate::tools::build(&profile, ctx.clone());
        let allowed: Vec<String> = tools
            .iter()
            .map(|t| format!("mcp__{}__{}", mcp::SERVER_NAME, t.spec().name))
            .collect();
        let grant = inner.grants.issue(tools);
        let host = match env.kind() {
            ExecKind::Container => exec::CONTAINER_HOST_ALIAS,
            ExecKind::Local => "127.0.0.1",
        };
        let mcp_config = mcp::client_config(&format!("http://{host}:{port}/mcp"), &grant.token);
        let path = env.run_dir().join("mcp.json");
        if let Err(e) = crate::datadir::write_private_file(&path, &serde_json::to_vec(&mcp_config)?)
        {
            return Ok(RunEnd::Stuck(format!("無法寫入 MCP 設定：{e}")));
        }
        let mcp_path = format!("{}/mcp.json", env.run_dir_seen());
        // 容器裡共用的 home 不可信（之前的 run 可能寫進 hooks）：不讀任何 settings.json，改用平台產生的
        // 設定（關掉所有 hooks）。本機用你自己的 Claude Code 設定。
        let (setting_sources, settings_path) = match env.kind() {
            ExecKind::Container => {
                let path = env.run_dir().join("settings.json");
                let settings = serde_json::json!({"disableAllHooks": true});
                if let Err(e) =
                    crate::datadir::write_private_file(&path, &serde_json::to_vec(&settings)?)
                {
                    return Ok(RunEnd::Stuck(format!("無法寫入 Claude Code 設定：{e}")));
                }
                (
                    String::new(),
                    Some(format!("{}/settings.json", env.run_dir_seen())),
                )
            }
            ExecKind::Local => ("user".to_string(), None),
        };
        let instructions = format!("{instructions}{CLAUDE_NOTE}");

        // session：有就續接（官方 session 檔還在時），否則開新的。
        let previous = session_of(&inner.store, task.id).await?;
        let resumable = previous.as_ref().is_some_and(|id| {
            session_exists(env.kind(), &claude_config(&inner.data_dir), task.id, id)
        });
        let mut mode = match (&previous, resumable) {
            (Some(id), true) => claude::SessionMode::Resume(id.clone()),
            _ => claude::SessionMode::New(Uuid::new_v4().to_string()),
        };
        let program = match env.kind() {
            ExecKind::Container => "claude".to_string(),
            ExecKind::Local => inner.config.claude_bin.clone(),
        };
        let mut retried = false;
        loop {
            let session_id = match &mode {
                claude::SessionMode::New(id) | claude::SessionMode::Resume(id) => id.clone(),
            };
            save_session(
                &inner.store,
                task.id,
                job.run,
                &session_id,
                env.kind(),
                &inner.data_dir,
                &work_dir(&ctx),
            )
            .await?;
            // 新的 session 與續接都送完整的觸發訊息：續接時任務與 Brief 可能已經改了。
            let prompt = message.clone();
            let run = claude::ClaudeRun {
                command: env.command(&program, &[]),
                session: mode.clone(),
                prompt,
                append_system_prompt: instructions.clone(),
                mcp_config_path: mcp_path.clone(),
                setting_sources: setting_sources.clone(),
                settings_path: settings_path.clone(),
                model: (!settings.claude_model.is_empty()).then(|| settings.claude_model.clone()),
                allowed_tools: allowed.clone(),
                disallowed_tools: CLAUDE_DISALLOWED.iter().map(|s| s.to_string()).collect(),
                policy: Arc::new(claude_policy),
                nudge: Some(NUDGE.into()),
                finished: ctx.finished.clone(),
                stop: ctx.stop.clone(),
                grace: inner.config.grace,
                hard_stop: env.hard_stop(),
            };
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let progress = {
                let hub = inner.hub.clone();
                let task = task.id;
                let goal = job.goal;
                let store = inner.store.clone();
                let run_id = job.run;
                tokio::spawn(async move {
                    while let Some(ev) = rx.recv().await {
                        let text = match ev {
                            claude::ClaudeEvent::Started {
                                session_id,
                                version,
                            } => {
                                if let Some(v) = version {
                                    let _ = save_cli_version(&store, task, &v).await;
                                }
                                tracing::debug!(%run_id, %session_id, "Claude Code session 開始");
                                continue;
                            }
                            claude::ClaudeEvent::Text(t) => tasks::one_line(&t, 200),
                            claude::ClaudeEvent::ToolUse { summary, .. } => summary,
                            claude::ClaudeEvent::PermissionDecided { .. } => continue,
                        };
                        if text.is_empty() {
                            continue;
                        }
                        let r: Result<(), Error> = async {
                            let mut txn = hub.begin().await?;
                            tasks::set_task_work(txn.conn(), task, None, None, Some(&text)).await?;
                            tasks::push_goal(&mut txn, goal).await?;
                            txn.commit().await
                        }
                        .await;
                        if let Err(e) = r {
                            tracing::warn!(error = %e, "無法更新 worker 進度");
                        }
                    }
                })
            };
            let outcome = claude::run(run, tx, job.cancel.clone()).await;
            let _ = progress.await;
            // Claude Code 回報整個行程的累計成本（不是帳單）；重試時累加。
            if let Some(cost) = outcome.cost_usd {
                let mut tx = inner.store.begin().await?;
                Store::add_run_cost(&mut tx, job.run, cost).await?;
                tx.commit().await?;
            }
            if let Some(id) = &outcome.session_id
                && *id != session_id
            {
                save_session(
                    &inner.store,
                    task.id,
                    job.run,
                    id,
                    env.kind(),
                    &inner.data_dir,
                    &work_dir(&ctx),
                )
                .await?;
            }
            let finished = ctx.finished.load(std::sync::atomic::Ordering::SeqCst);
            return Ok(match outcome.end {
                _ if finished => RunEnd::Finished,
                claude::ClaudeEnd::Finished => RunEnd::Finished,
                claude::ClaudeEnd::Cancelled => RunEnd::Cancelled,
                claude::ClaudeEnd::TurnEnded => {
                    let last = tasks::one_line(&outcome.last_text, 500);
                    RunEnd::Stuck(if last.is_empty() {
                        "Claude Code 結束了這一輪，但沒有回報結果".into()
                    } else {
                        format!("Claude Code 結束了這一輪，但沒有回報結果。最後說：{last}")
                    })
                }
                // 官方 session 不見了：CLI 可能以 error result 或只在 stderr 回報（比對英文訊息，
                // 沒有結構化的欄位）。
                claude::ClaudeEnd::Failed(
                    claude::ClaudeFailure::Cli(text)
                    | claude::ClaudeFailure::Exit { stderr: text, .. },
                ) if !retried
                    && matches!(mode, claude::SessionMode::Resume(_))
                    && text.contains("No conversation found") =>
                {
                    // 官方 session 不見了：開新的（帶完整 context）。
                    retried = true;
                    mode = claude::SessionMode::New(Uuid::new_v4().to_string());
                    continue;
                }
                claude::ClaudeEnd::Failed(f) => RunEnd::Stuck(describe_failure(&f)),
            });
        }
    }

    async fn finish(
        &self,
        job: &Job,
        result: Result<RunEnd, Error>,
        reason: Option<StopReason>,
    ) -> Result<(), Error> {
        let inner = &self.inner;
        let shutdown = inner.shutdown.is_cancelled();
        let mut txn = inner.hub.begin().await?;
        let task = tasks::task(txn.conn(), job.task).await?;
        let stop: Option<String> =
            sqlx::query_scalar("SELECT stop_reason FROM agent_runs WHERE id = ?")
                .bind(job.run.to_string())
                .fetch_optional(txn.conn())
                .await?
                .flatten();
        let mut ends = Ends::default();
        let (state, error): (&str, Option<String>) = match result {
            Ok(RunEnd::Skipped) => ("cancelled", None),
            Ok(RunEnd::Finished) => ("done", None),
            Ok(RunEnd::Cancelled) if shutdown && reason.is_none() => {
                // 服務關閉：放回佇列，下次啟動接著做（不算一次重試）。
                let text: String =
                    sqlx::query_scalar("SELECT trigger FROM agent_runs WHERE id = ?")
                        .bind(job.run.to_string())
                        .fetch_one(txn.conn())
                        .await?;
                let mut list = TriggerList {
                    triggers: parse_triggers(&text),
                };
                list.triggers.push(WorkerTrigger::Retry {
                    reason: "服務關閉".into(),
                });
                sqlx::query(
                    "UPDATE agent_runs SET state = 'queued', lease_until = NULL, trigger = ?, \
                     attempts = CASE WHEN attempts > 0 THEN attempts - 1 ELSE 0 END WHERE id = ?",
                )
                .bind(serde_json::to_string(&list)?)
                .bind(job.run.to_string())
                .execute(txn.conn())
                .await?;
                return txn.commit().await;
            }
            Ok(RunEnd::Cancelled) if reason == Some(StopReason::Timeout) => {
                if let Some(t) = task.as_ref().filter(|t| t.state == TaskState::Running) {
                    ends = end_task(
                        &mut txn,
                        t,
                        TaskEnd::NeedsHuman(format!(
                            "worker 超過執行時間上限（{} 分鐘）",
                            inner.config.run_timeout.as_secs() / 60
                        )),
                        &Principal::System,
                        Some(job.run),
                    )
                    .await?;
                }
                ("failed", Some("逾時".into()))
            }
            Ok(RunEnd::Cancelled) => ("cancelled", None),
            Ok(RunEnd::Stuck(why)) => {
                let why = match stop.as_deref() {
                    Some("budget") => {
                        "這個 Goal 的預算用完了，等你決定要不要追加（Inbox）".to_string()
                    }
                    Some("breaker") => "這個 project 的預算熔斷中，暫停所有 run".to_string(),
                    _ => why,
                };
                if let Some(t) = task.as_ref().filter(|t| t.state == TaskState::Running) {
                    ends = end_task(
                        &mut txn,
                        t,
                        TaskEnd::NeedsHuman(why.clone()),
                        &Principal::System,
                        Some(job.run),
                    )
                    .await?;
                }
                ("failed", Some(why))
            }
            Err(e) => {
                tracing::error!(run = %job.run, error = %e, "worker run 失敗");
                if let Some(t) = task.as_ref().filter(|t| t.state == TaskState::Running) {
                    ends = end_task(
                        &mut txn,
                        t,
                        TaskEnd::NeedsHuman("worker 沒有完成：伺服器內部錯誤".into()),
                        &Principal::System,
                        Some(job.run),
                    )
                    .await?;
                }
                ("failed", Some(e.to_string()))
            }
        };
        // 結束工具已經改過狀態；只有還停在 Running（例如被取消時任務其實已被取消）才不用動。
        sqlx::query(
            "UPDATE agent_runs SET state = ?, error = ?, finished_at = ?, lease_until = NULL WHERE id = ?",
        )
        .bind(state)
        .bind(error)
        .bind(Timestamp::now().0)
        .bind(job.run.to_string())
        .execute(txn.conn())
        .await?;
        if let Some(t) = &task {
            tasks::set_task_work(txn.conn(), t.id, None, None, Some("")).await?;
            tasks::push_goal(&mut txn, t.goal).await?;
        }
        let w = self.clone();
        txn.commit_then(|| ends.notify(&w, Some(job.task))).await?;
        if stop.as_deref() == Some("budget")
            && let Err(e) = crate::budget::request_increase(
                &inner.store,
                &inner.hub,
                &inner.clock,
                job.project,
                job.goal,
            )
            .await
        {
            tracing::warn!(run = %job.run, error = %e, "無法申請追加預算");
        }
        Ok(())
    }
}

fn work_dir(ctx: &ToolCtx) -> String {
    ctx.work
        .as_ref()
        .map(|w| w.checkout.dir.display().to_string())
        .unwrap_or_default()
}

/// Claude Code 原生工具的權限：容器就是沙箱，一律放行；只擋 push（push 只能經 `open_pr`）與背景執行。
/// 擋 push 是盡力而為（指令可以有無數種寫法），真正的保護是容器與工作目錄裡沒有任何憑證；本機執行
/// 沒有沙箱，你自己的 git 憑證在那裡也能用。
fn claude_policy(tool: &str, input: &Value) -> claude::Permission {
    if tool == "Bash" {
        if input.get("run_in_background").and_then(Value::as_bool) == Some(true) {
            return claude::Permission::Deny(
                "不能在背景執行：要等事件用 mcp__pots__wait_for".into(),
            );
        }
        if let Some(cmd) = input.get("command").and_then(Value::as_str)
            && mentions_git_push(cmd)
        {
            return claude::Permission::Deny("不能自己 push：完成後用 mcp__pots__open_pr".into());
        }
    }
    claude::Permission::Allow
}

/// 指令裡有沒有 `git … push`（`git` 之後跳過選項與它們的值，第一個子指令是 `push`）。
fn mentions_git_push(cmd: &str) -> bool {
    let words: Vec<&str> = cmd
        .split(|c: char| c.is_whitespace() || "|;&()`'\"$".contains(c))
        .filter(|w| !w.is_empty())
        .collect();
    for (i, w) in words.iter().enumerate() {
        if !(*w == "git" || w.ends_with("/git")) {
            continue;
        }
        let mut j = i + 1;
        while let Some(next) = words.get(j) {
            match *next {
                // 帶值的全域選項。
                "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--exec-path" => j += 2,
                o if o.starts_with('-') => j += 1,
                sub => return sub == "push",
            }
        }
    }
    false
}

fn describe_failure(f: &claude::ClaudeFailure) -> String {
    match f {
        // 不帶主機路徑（會進任務的原因、給 agent 看）：登入指令在設定裡。
        claude::ClaudeFailure::NeedsLogin(m) => format!(
            "Claude Code 需要重新登入（{}）：到設定的「Worker」類別照指示執行登入指令，在 Claude Code \
             裡用 /login 完成官方登入，再到任務按「重試」",
            tasks::one_line(m, 200),
        ),
        claude::ClaudeFailure::UsageLimit(m) => {
            format!("Claude Code 的用量額度用完了：{}", tasks::one_line(m, 300))
        }
        claude::ClaudeFailure::Cli(m) => {
            format!("Claude Code 回報錯誤：{}", tasks::one_line(m, 500))
        }
        claude::ClaudeFailure::Protocol(m) => {
            format!("Claude Code 的輸出不合預期：{}", tasks::one_line(m, 300))
        }
        claude::ClaudeFailure::Exit { code, stderr } => format!(
            "Claude Code 異常結束（{}）：{}",
            code.map(|c| format!("代碼 {c}"))
                .unwrap_or_else(|| "被訊號中止".into()),
            tasks::one_line(stderr, 400)
        ),
        claude::ClaudeFailure::Spawn(m) => {
            format!("無法啟動 Claude Code：{}", tasks::one_line(m, 300))
        }
    }
}

/// 官方 session 檔還在不在。容器的 home 是平台的專屬目錄，可以檢查；本機用你自己的 Claude Code
/// 設定，位置不一定，交給 CLI 判斷（找不到時開新的）。
fn session_exists(kind: ExecKind, config: &std::path::Path, task: TaskId, id: &str) -> bool {
    match kind {
        ExecKind::Container => {
            let dir: String = exec::container_workdir(&task.to_string())
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect();
            config
                .join("projects")
                .join(dir)
                .join(format!("{id}.jsonl"))
                .exists()
        }
        ExecKind::Local => true,
    }
}

async fn session_of(store: &Store, task: TaskId) -> Result<Option<String>, Error> {
    let mut conn = store.acquire().await?;
    Ok(sqlx::query_scalar(
        "SELECT external_session_id FROM worker_sessions WHERE task_id = ? AND backend = 'claude_code'",
    )
    .bind(task.to_string())
    .fetch_optional(&mut *conn)
    .await?
    .flatten())
}

async fn save_session(
    store: &Store,
    task: TaskId,
    run: Uuid,
    session: &str,
    kind: ExecKind,
    data_dir: &std::path::Path,
    worktree: &str,
) -> Result<(), Error> {
    let state_ref = match kind {
        ExecKind::Container => claude_config(data_dir).display().to_string(),
        ExecKind::Local => "local".to_string(),
    };
    let now = Timestamp::now().0;
    let mut tx = store.begin().await?;
    sqlx::query(
        "INSERT INTO worker_sessions (id, user_id, task_id, run_id, backend, external_session_id, \
         state_ref, worktree_ref, created_at, updated_at) VALUES (?, ?, ?, ?, 'claude_code', ?, ?, ?, ?, ?) \
         ON CONFLICT (task_id, backend) DO UPDATE SET run_id = excluded.run_id, \
         external_session_id = excluded.external_session_id, state_ref = excluded.state_ref, \
         worktree_ref = excluded.worktree_ref, updated_at = excluded.updated_at",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(LOCAL_USER_ID.to_string())
    .bind(task.to_string())
    .bind(run.to_string())
    .bind(session)
    .bind(state_ref)
    .bind(worktree)
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn save_cli_version(store: &Store, task: TaskId, version: &str) -> Result<(), Error> {
    let mut tx = store.begin().await?;
    sqlx::query(
        "UPDATE worker_sessions SET cli_version = ? WHERE task_id = ? AND backend = 'claude_code'",
    )
    .bind(version)
    .bind(task.to_string())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn mark_failed(store: &Store, run: Uuid) -> Result<(), Error> {
    let mut tx = store.begin().await?;
    sqlx::query(
        "UPDATE agent_runs SET state = 'failed', error = '無法記錄結束', finished_at = ?, \
         lease_until = NULL WHERE id = ? AND state = 'running'",
    )
    .bind(Timestamp::now().0)
    .bind(run.to_string())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn extend_lease(store: &Store, run: Uuid) -> Result<(), Error> {
    let mut tx = store.begin().await?;
    sqlx::query("UPDATE agent_runs SET lease_until = ? WHERE id = ? AND state = 'running'")
        .bind(Timestamp::now().0 + LEASE.as_secs() as i64)
        .bind(run.to_string())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// worker 的觸發訊息：為什麼叫醒你、任務、工作位置、Goal 的 Brief 與新的 team 貼文。
async fn render_prompt(
    txn: &mut LogTxn<'_>,
    task: &TaskRow,
    goal: &GoalRow,
    triggers: &[WorkerTrigger],
    checkout: &Checkout,
    quick: bool,
) -> Result<String, Error> {
    let mut out = String::from("# 為什麼叫醒你\n\n");
    if triggers.is_empty() {
        out.push_str("- 繼續這個任務。\n");
    }
    for t in triggers {
        out.push_str(&format!("- {}\n", t.describe()));
    }
    out.push_str(&format!(
        "\n# 你的任務\n\n- task_id：{}\n- 標題：{}\n",
        task.id,
        tasks::one_line(&task.title, 200)
    ));
    if !task.summary.is_empty() {
        out.push_str(&format!(
            "- 說明：{}\n",
            tasks::one_line(&task.summary, 2000)
        ));
    }
    if quick {
        out.push_str(&format!(
            "- 這是快速修改：最多改 {QUICK_MAX_FILES} 個檔案、{QUICK_MAX_LINES} 行，而且不需要設計決定；超過時用 escalate。\n"
        ));
    }
    let local = task
        .repo
        .as_deref()
        .and_then(|r| r.strip_prefix("local:"))
        .map(str::to_string);
    out.push_str(&format!(
        "\n# 工作位置\n\n- {}：{}\n- 分支：{}（從 {} 開出來的，已經切好）\n",
        if local.is_some() {
            "本機資料夾（沒有 remote、PR 與 CI）"
        } else {
            "repo"
        },
        local
            .as_deref()
            .unwrap_or(task.repo.as_deref().unwrap_or("")),
        checkout.branch,
        checkout.base
    ));
    if local.is_some() {
        out.push_str(
            "- 工作目錄是平台自己的副本，不是你資料夾裡的即時檔案。commit 之後用 open_pr 記下變更，\
             再直接 request_approval：核准後平台把分支套用進你的資料夾。\n",
        );
    }
    if let Some(pr) = &task.pr {
        let what = if pr.starts_with("local:") {
            "已經記下變更（等核准）"
        } else {
            "已經開了 PR"
        };
        out.push_str(&format!(
            "- {what}：{pr}{}\n",
            task.pr_url
                .as_deref()
                .map(|u| format!("（{u}）"))
                .unwrap_or_default()
        ));
    }
    out.push_str(&format!(
        "\n# Goal\n\n- 標題：{}\n",
        tasks::one_line(&goal.title, 200)
    ));
    if let Some(brief) =
        artifacts::for_goal(txn.conn(), goal.id, pots_proto::ArtifactKind::Brief).await?
    {
        out.push_str("\n## Brief\n\n<artifact>\n");
        out.push_str(brief.body.trim());
        out.push_str("\n</artifact>\n");
    }
    let me = Principal::agent("worker");
    let posts = crate::team::unread(
        txn.conn(),
        goal.id,
        &format!("worker:{}", task.id),
        &me,
        Some(task.id),
    )
    .await?;
    if !posts.is_empty() {
        out.push_str("\n# Team 的新貼文\n\n");
        out.push_str(&crate::team::render(txn.conn(), &posts).await?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers_parse_old_and_new_formats() {
        let old = serde_json::json!({
            "kind": "wake",
            "waited": {"kind": "at", "at": 5},
            "result": "時間到了",
        });
        let t = parse_triggers(&old.to_string());
        assert!(matches!(&t[..], [WorkerTrigger::Wake { result, .. }] if result == "時間到了"));
        let list = serde_json::to_string(&TriggerList {
            triggers: vec![WorkerTrigger::Assigned],
        })
        .unwrap();
        assert_eq!(parse_triggers(&list), vec![WorkerTrigger::Assigned]);
        assert!(parse_triggers("garbage").is_empty());
    }

    #[test]
    fn policy_blocks_push_only() {
        let bash = |c: &str| claude_policy("Bash", &serde_json::json!({"command": c}));
        assert_eq!(bash("cargo test"), claude::Permission::Allow);
        assert!(matches!(
            bash("git push origin HEAD"),
            claude::Permission::Deny(_)
        ));
        assert!(matches!(
            bash("make && git  push"),
            claude::Permission::Deny(_)
        ));
        assert!(matches!(
            bash("make\ngit push"),
            claude::Permission::Deny(_)
        ));
        assert!(matches!(bash("git -C . push"), claude::Permission::Deny(_)));
        assert!(matches!(
            bash("/usr/bin/git -c a=b --no-pager push"),
            claude::Permission::Deny(_)
        ));
        assert!(matches!(
            bash("sh -c 'git push'"),
            claude::Permission::Deny(_)
        ));
        assert_eq!(
            bash("git commit -m 'push later'"),
            claude::Permission::Allow
        );
        assert!(matches!(
            claude_policy(
                "Bash",
                &serde_json::json!({"command": "sleep 9", "run_in_background": true})
            ),
            claude::Permission::Deny(_)
        ));
        assert_eq!(bash("git log --grep push"), claude::Permission::Allow);
        assert_eq!(
            claude_policy("Edit", &serde_json::json!({})),
            claude::Permission::Allow
        );
    }

    #[test]
    fn settings_are_checked() {
        let d = WorkerConfig::default().defaults;
        assert!(check_settings(&d).is_ok());
        let mut bad = d.clone();
        bad.runtime = "docker; rm -rf /".into();
        assert!(check_settings(&bad).is_err());
        let mut bad = d;
        bad.max_parallel = 0;
        assert!(check_settings(&bad).is_err());
    }
}
