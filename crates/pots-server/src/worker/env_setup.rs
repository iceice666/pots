//! 環境 worker 的工具：`try_env`（實際建置與驗證一份環境規格）與 `apply_env`（套用驗證過的規格）
//! （`proposal/agent-runtime.md#project-環境由環境-worker-建立`）。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use pots_llm::agent::ToolError;
use pots_proto::{EnvSpec, ExecKind, ProjectId};
use serde::Deserialize;
use uuid::Uuid;

use super::exec::{Env, ExecSpec};
use super::{TaskEnd, WorkCtx};
use crate::Error;
use crate::project_env::{self, OUTPUT_TAIL};
use crate::tasks::{self, TaskState};
use crate::tools::{ToolCtx, bad, internal};

/// 一次 run 最多試幾次。
pub const MAX_TRIES: u32 = 12;

/// 環境 worker 這次 run 的狀態（一般 worker 沒有）。
pub struct EnvSetupCtx {
    pub project: ProjectId,
    /// 啟動驗證容器用的設定（image、env、快取每次換成要試的規格）。
    pub exec: ExecSpec,
    /// 「Worker」設定的 image（規格沒寫 base_image 時用）。
    pub default_image: String,
    pub data_dir: PathBuf,
    pub run: Uuid,
    /// 這次 run 裡 `try_env` 驗證通過的規格（hash）。
    pub verified: Mutex<HashSet<String>>,
    pub tries: AtomicU32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpecArgs {
    #[serde(default)]
    pub base_image: String,
    #[serde(default)]
    pub install: String,
    #[serde(default)]
    pub prepare: String,
    #[serde(default)]
    pub verify: String,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub caches: Vec<String>,
}

impl SpecArgs {
    fn spec(&self) -> EnvSpec {
        EnvSpec {
            base_image: self.base_image.clone(),
            install: self.install.clone(),
            prepare: self.prepare.clone(),
            verify: self.verify.clone(),
            env: self
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            caches: self.caches.clone(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyArgs {
    pub spec: SpecArgs,
    pub summary: String,
}

/// 規格參數的 JSON schema（`try_env` 與 `apply_env` 共用）。
pub fn spec_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "base_image": {"type": "string", "description": "基底 image；不給就用平台的 worker image（Debian bookworm，有 git、build-essential、python3、node 22）"},
            "install": {"type": "string", "description": "建 image 時以 root 執行的 sh 腳本（set -eu）：裝系統套件與工具鏈。看不到 repo；裝到 /opt 或 /usr/local，不要裝到 $HOME"},
            "prepare": {"type": "string", "description": "每次 run 開始時在工作目錄（repo 根目錄）以一般使用者執行的 sh 腳本：依 repo 準備（例如依 rust-toolchain 裝版本、下載依賴）。要能重複執行"},
            "verify": {"type": "string", "description": "證明環境能用的指令，例如 cargo check --workspace 或 npm test；只在 try_env 執行"},
            "env": {"type": "object", "additionalProperties": {"type": "string"}, "description": "環境變數，例如 {\"PATH\": \"/opt/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\", \"CARGO_TARGET_DIR\": \"/cache/target\"}"},
            "caches": {"type": "array", "items": {"type": "string"}, "description": "跨 run 保留的容器絕對路徑（同一個 project 共用），例如 /cache/target、/opt/cargo/registry"},
        },
        "additionalProperties": false,
    })
}

fn setup(work: &WorkCtx) -> Result<&EnvSetupCtx, ToolError> {
    work.setup
        .as_ref()
        .ok_or_else(|| bad("這個工具只有環境分析的任務能用"))
}

fn step_report(name: &str, out: &super::exec::ExecOutput, secs: u64) -> String {
    let status = if out.timed_out {
        "逾時".to_string()
    } else {
        match out.code {
            Some(0) => "成功".into(),
            Some(c) => format!("失敗（exit code {c}）"),
            None => "被訊號中止".into(),
        }
    };
    let output = format!("{}{}", out.stdout, out.stderr);
    let output = project_env::tail(output.trim(), OUTPUT_TAIL / 2);
    format!("## {name}：{status}（{secs} 秒）\n{output}\n")
}

/// 建置並驗證一份規格：建衍生 image、開新容器跑 `prepare` 與 `verify`。全部通過時記下 hash。
pub async fn try_env(ctx: &ToolCtx, work: &WorkCtx, args: SpecArgs) -> Result<String, ToolError> {
    let setup = setup(work)?;
    let spec = project_env::check_spec(&args.spec()).map_err(bad)?;
    let n = setup.tries.fetch_add(1, Ordering::SeqCst) + 1;
    if n > MAX_TRIES {
        return Err(bad(format!(
            "這次已經試了 {MAX_TRIES} 次；請用 fail_task 說明卡在哪裡，或用 ask_user 問使用者"
        )));
    }
    let _ = set_progress(ctx, &format!("試第 {n} 版環境：建置 image…")).await;
    let mut report = String::new();

    // 1. image。
    let started = Instant::now();
    let tag = if setup.exec.kind == ExecKind::Container {
        let built = tokio::select! {
            () = ctx.cancel.cancelled() => return Err(bad("已取消")),
            r = project_env::ensure_image(
                &setup.exec.runtime,
                &setup.data_dir,
                setup.project,
                &spec,
                &setup.default_image,
                &setup.exec.instance,
            ) => r,
        };
        match built {
            Ok(b) => {
                report.push_str(&format!(
                    "## 建置 image：成功（{}，{} 秒）\n{}\n",
                    b.tag,
                    started.elapsed().as_secs(),
                    if b.built {
                        project_env::tail(&b.log, OUTPUT_TAIL / 4)
                    } else {
                        "（沿用已建好的 image）".into()
                    }
                ));
                b.tag
            }
            Err(e) => {
                return Ok(format!(
                    "第 {n} 次：沒有通過。\n## 建置 image：失敗（{} 秒）\n{e}",
                    started.elapsed().as_secs()
                ));
            }
        }
    } else {
        // 本機執行（只有測試）：沒有 image 可建。
        report.push_str("## 建置 image：略過（本機執行）\n");
        String::new()
    };

    // 2. 新的容器：只掛這個任務的工作目錄與快取。
    let mut exec = setup.exec.clone();
    exec.image = tag;
    exec.env = spec.env.clone();
    exec.mounts = project_env::mounts(&setup.data_dir, setup.project, &spec);
    exec.claude = None;
    let name = format!("{}-try{n}", setup.run);
    let run_dir = setup.data_dir.join("worker-runs").join(&name);
    let task_name = work.task.to_string();
    let env = tokio::select! {
        () = ctx.cancel.cancelled() => return Err(bad("已取消")),
        r = Env::start(&exec, &work.checkout.dir, &run_dir, &name, &task_name) => r,
    };
    let env = match env {
        Ok(e) => e,
        Err(e) => {
            return Ok(format!(
                "第 {n} 次：沒有通過。\n{report}## 啟動容器：失敗\n{e}"
            ));
        }
    };

    // 3. prepare 與 verify。
    let mut passed = true;
    for (step, script, timeout) in [
        ("prepare", &spec.prepare, project_env::PREPARE_TIMEOUT),
        ("verify", &spec.verify, project_env::VERIFY_TIMEOUT),
    ] {
        if script.is_empty() {
            report.push_str(&format!("## {step}：（沒有）\n"));
            continue;
        }
        let _ = set_progress(ctx, &format!("試第 {n} 版環境：{step}…")).await;
        let started = Instant::now();
        let script = format!("set -e\n{script}");
        let out = tokio::select! {
            () = ctx.cancel.cancelled() => None,
            r = env.exec(&script, None, timeout) => Some(r),
        };
        let out = match out {
            None => {
                env.stop().await;
                return Err(bad("已取消"));
            }
            Some(Err(e)) => {
                passed = false;
                report.push_str(&format!("## {step}：無法執行\n{e}\n"));
                break;
            }
            Some(Ok(o)) => o,
        };
        report.push_str(&step_report(step, &out, started.elapsed().as_secs()));
        if !out.ok() {
            passed = false;
            break;
        }
    }
    env.stop().await;
    if spec.verify.is_empty() {
        passed = false;
        report.push_str("\n沒有 verify：至少要有一個證明環境能用的指令（例如建置或跑測試）。\n");
    }
    if passed {
        setup
            .verified
            .lock()
            .unwrap()
            .insert(project_env::spec_hash(&spec));
        Ok(format!(
            "第 {n} 次：通過。用同一份規格（一字不改）呼叫 apply_env 套用。\n{report}"
        ))
    } else {
        Ok(format!("第 {n} 次：沒有通過。\n{report}"))
    }
}

/// 套用驗證過的規格：新增一個版本並設為使用中，任務完成。
pub async fn apply_env(
    ctx: &ToolCtx,
    work: &WorkCtx,
    args: ApplyArgs,
) -> Result<String, ToolError> {
    let setup = setup(work)?;
    let spec = project_env::check_spec(&args.spec.spec()).map_err(bad)?;
    let summary = tasks::check_text("summary ", &args.summary, 4000, true).map_err(bad)?;
    if !setup
        .verified
        .lock()
        .unwrap()
        .contains(&project_env::spec_hash(&spec))
    {
        return Err(bad(
            "這份規格這次還沒有用 try_env 驗證通過（或驗證後又改了）：先用 try_env 驗證同一份規格",
        ));
    }
    let id = ctx
        .scope
        .task()
        .ok_or_else(|| bad("這個工具只有環境分析的任務能用"))?;
    let result: Result<Result<u32, String>, Error> = async {
        let mut txn = ctx.hub.begin().await?;
        let Some(task) = tasks::task(txn.conn(), id).await? else {
            return Ok(Err("找不到你的任務".into()));
        };
        if task.state != TaskState::Running {
            return Ok(Err(format!("任務現在是 {}，不能再套用", task.state.name())));
        }
        let rev =
            project_env::insert_rev(txn.conn(), setup.project, &spec, &summary, ctx.scope.goal())
                .await?;
        let ends = super::end_task(
            &mut txn,
            &task,
            TaskEnd::Done(format!(
                "環境已更新到第 {rev} 版：{}",
                tasks::one_line(&summary, 400)
            )),
            &ctx.actor,
            Some(ctx.run_id),
        )
        .await?;
        // end_task 關掉 Goal 時已經推過一次；這裡再推一次確保帶到新版本。
        project_env::push(&mut txn, setup.project).await?;
        let worker = ctx.worker.clone();
        txn.commit_then(|| ends.notify(&worker, Some(id))).await?;
        Ok(Ok(rev))
    }
    .await;
    let rev = result.map_err(internal)?.map_err(bad)?;
    ctx.finish();
    Ok(format!(
        "已套用成第 {rev} 版，之後的 worker 會用這個環境。這一輪到此結束"
    ))
}

async fn set_progress(ctx: &ToolCtx, text: &str) -> Result<(), Error> {
    let Some(task) = ctx.scope.task() else {
        return Ok(());
    };
    let goal = ctx.scope.goal();
    let mut txn = ctx.hub.begin().await?;
    tasks::set_task_work(txn.conn(), task, None, None, Some(text)).await?;
    if let Some(g) = goal {
        tasks::push_goal(&mut txn, g).await?;
    }
    txn.commit().await
}

/// 環境 worker 的觸發訊息補充：目前使用中的環境與基底 image。
pub fn describe_current(current: Option<&(u32, EnvSpec)>, default_image: &str) -> String {
    let mut s = String::from("\n\n# 目前的環境\n\n");
    match current {
        Some((rev, spec)) => {
            s.push_str(&format!(
                "使用中的是第 {rev} 版（你現在就跑在它裡面，但沒有先跑它的 prepare）：\n\n```json\n{}\n```\n",
                serde_json::to_string_pretty(spec).unwrap_or_default()
            ));
        }
        None => s.push_str(&format!(
            "這個 project 還沒有環境；你現在跑在平台的 worker image（{default_image}）裡。\n"
        )),
    }
    s
}
