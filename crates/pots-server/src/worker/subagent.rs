//! 唯讀的 subagent（`proposal/agent-collaboration.md#主-agentsubagent-與-team`）。
//!
//! - 主 agent（對話 agent、Planner、內建 worker）在一輪內用 `run_subagent` 開出 explorer 或
//!   reviewer，等它們回來；一次最多 [`MAX_SUBAGENTS`] 個並行，只看得到主 agent 給的 prompt。
//! - subagent 只有唯讀工具：平台狀態（同一個範圍）與程式碼。worker 開的讀自己的工作目錄，
//!   其他的讀 repo 預設分支的鏡像。subagent 不能再開 subagent（深度 1）。
//! - 每個 subagent 是一筆 `agent_runs`（`parent_run_id` 指向主 agent 的 run），不建任務；
//!   主 agent 被取消時一起停。用量算進 Goal。

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use pots_llm::agent::{self, RunConfig, ToolError};
use pots_llm::{Item, LlmError};
use pots_proto::{ModelRole, Timestamp};
use tokio::sync::Notify;
use tracing::Instrument as _;
use uuid::Uuid;

use super::code::{self, CodeAccess};
use super::tools::{MAX_SUBAGENTS, SubagentTask};
use crate::Error;
use crate::profiles::AgentProfile;
use crate::runs::explain;
use crate::tasks::{self, Principal};
use crate::tools::{Scope, ToolCtx, bad, internal};

/// 一個 subagent 的 prompt 最多幾個字。
const MAX_PROMPT_CHARS: usize = 8000;

pub async fn run(
    ctx: &Arc<ToolCtx>,
    profile: &str,
    jobs: Vec<SubagentTask>,
) -> Result<String, ToolError> {
    let worker = &ctx.worker;
    let profile: AgentProfile = match profile {
        "explorer" => worker.profiles().explorer.clone(),
        "reviewer" => worker.profiles().reviewer.clone(),
        p => return Err(bad(format!("profile 要是 explorer 或 reviewer：{p}"))),
    };
    if jobs.is_empty() || jobs.len() > MAX_SUBAGENTS {
        return Err(bad(format!("tasks 要有 1 到 {MAX_SUBAGENTS} 個")));
    }
    for j in &jobs {
        tasks::check_text("prompt ", &j.prompt, MAX_PROMPT_CHARS, true).map_err(bad)?;
    }
    // explorer／reviewer 各有自己的角色設定。profile 只會是上面兩個，不認得的名稱
    // （將來新增 subagent profile 卻還沒有對應的角色時）退回對話 agent 的設定，
    // 和主 agent 用同一個模型最不意外。
    let role = ModelRole::from_profile(&profile.name).unwrap_or(ModelRole::Chat);
    let route = worker
        .router()
        .resolve_role(role)
        .map_err(|e| bad(format!("沒有可用的模型：{}", explain(&e))))?;

    // 每個 subagent 看得到的程式碼。
    let mut views: Vec<Option<Arc<dyn CodeAccess>>> = Vec::new();
    for j in &jobs {
        let view: Option<Arc<dyn CodeAccess>> = match (&ctx.code, &ctx.work, &j.repo) {
            // worker 開的：讀自己的工作目錄。
            (Some(c), Some(_), None) => Some(c.clone()),
            _ => match mirror(ctx, j.repo.as_deref()).await? {
                Some(m) => Some(Arc::new(m)),
                None => None,
            },
        };
        views.push(view);
    }

    let futures =
        jobs.into_iter()
            .zip(views)
            .enumerate()
            .map(|(i, (job, code))| {
                let ctx = ctx.clone();
                let profile = profile.clone();
                let provider = route.provider.clone();
                let reasoning = route.reasoning;
                let meta = route.meta("builtin");
                let model_name = meta.model.clone();
                async move {
                    let run_id = Uuid::now_v7();
                    let budget = match crate::budget::for_run(
                        &ctx.store,
                        &ctx.hub,
                        &ctx.clock,
                        run_id,
                        ctx.project,
                        ctx.scope.goal(),
                        match ctx.scope {
                            Scope::Task { task, .. } => Some(task),
                            _ => None,
                        },
                        meta,
                    )
                    .await
                    {
                        Ok(b) => b,
                        Err(e) => {
                            tracing::warn!(error = %e, "無法建立 subagent 的預算");
                            return (i, "（subagent 沒有開始：伺服器內部錯誤）".into());
                        }
                    };
                    let sub = Arc::new(ToolCtx {
                        hub: ctx.hub.clone(),
                        store: ctx.store.clone(),
                        planner: ctx.planner.clone(),
                        project: ctx.project,
                        scope: ctx.scope,
                        actor: Principal::agent(&profile.name),
                        reader: format!("{}:{run_id}", profile.name),
                        artifact_kinds: Vec::new(),
                        run_id,
                        cards: Default::default(),
                        queued: None,
                        finished: Arc::new(AtomicBool::new(false)),
                        budget,
                        milestones: Default::default(),
                        goals_created: Default::default(),
                        clock: ctx.clock.clone(),
                        worker: ctx.worker.clone(),
                        work: ctx.work.clone(),
                        code: code.clone(),
                        stop: Arc::new(Notify::new()),
                        cancel: ctx.cancel.child_token(),
                    });
                    let mut instructions = profile.role_prompt().to_string();
                    match &code {
                        Some(c) => instructions
                            .push_str(&format!("\n\n你讀的程式碼是：{}。", c.describe())),
                        None => instructions
                            .push_str("\n\n這次沒有可以讀的程式碼，只能用平台狀態的工具。"),
                    }
                    let config = RunConfig {
                        reasoning,
                        provider,
                        model: model_name.clone(),
                        instructions: Some(instructions),
                        tools: crate::tools::build(&profile, sub.clone()),
                        hook: Arc::new(sub.gate()),
                        max_turns: profile.max_turns,
                        session_id: Some(format!("run:{run_id}")),
                    };
                    if let Err(e) =
                        record_start(&ctx, run_id, &profile.name, &model_name, &job.prompt).await
                    {
                        tracing::warn!(error = %e, "無法記錄 subagent run");
                    }
                    let (events, rx) = tokio::sync::mpsc::unbounded_channel();
                    drop(rx);
                    let span = tracing::info_span!(
                        "agent.run",
                        agent = %profile.name,
                        project = %ctx.project,
                        run = %run_id,
                        parent = %ctx.run_id,
                        model = %model_name,
                    );
                    let result = agent::run(
                        &config,
                        vec![Item::User {
                            text: job.prompt.clone(),
                        }],
                        events,
                        sub.cancel.clone(),
                    )
                    .instrument(span)
                    .await;
                    let (state, error, text) = match result {
                        Ok(r) => {
                            let text = r
                                .new_items
                                .iter()
                                .rev()
                                .find_map(|i| match i {
                                    Item::Assistant { text } if !text.trim().is_empty() => {
                                        Some(text.clone())
                                    }
                                    _ => None,
                                })
                                .unwrap_or_else(|| "（subagent 沒有回答）".into());
                            ("done", None, text)
                        }
                        Err(LlmError::Cancelled) => ("cancelled", None, "（已取消）".into()),
                        Err(e) => {
                            let why = explain(&e);
                            (
                                "failed",
                                Some(e.to_string()),
                                format!("（subagent 失敗：{why}）"),
                            )
                        }
                    };
                    if let Err(e) = record_end(&ctx, run_id, state, error).await {
                        tracing::warn!(error = %e, "無法記錄 subagent run 的結束");
                    }
                    (i, text)
                }
            });
    let mut results = futures::future::join_all(futures).await;
    results.sort_by_key(|(i, _)| *i);
    if ctx.cancel.is_cancelled() {
        return Err(bad("已取消"));
    }
    let mut out = String::new();
    for (i, text) in results {
        out.push_str(&format!("## subagent {}\n\n{}\n\n", i + 1, text.trim()));
    }
    Ok(out)
}

/// repo 預設分支的鏡像；沒有 repo 時是 `None`。
async fn mirror(ctx: &ToolCtx, repo: Option<&str>) -> Result<Option<code::Mirror>, ToolError> {
    let repo = {
        let mut conn = ctx.store.acquire().await.map_err(internal)?;
        match super::pick_workdir(&mut conn, ctx.project, repo)
            .await
            .map_err(internal)?
        {
            Ok(r) => r,
            Err(_) if repo.is_none() => return Ok(None),
            Err(e) => return Err(bad(e)),
        }
    };
    let source = super::repo::RepoSource::parse(&repo).map_err(bad)?;
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
    let repos = ctx.worker.repos();
    let url = repos.remote_url(&source, &gh.api(&config));
    let (dir, branch) = repos
        .sync(&source, &url, gh.token().as_deref())
        .await
        .map_err(|e| bad(format!("無法取得 {repo}：{e}")))?;
    Ok(Some(code::Mirror { repo, dir, branch }))
}

async fn record_start(
    ctx: &ToolCtx,
    run: Uuid,
    agent: &str,
    model: &str,
    prompt: &str,
) -> Result<(), Error> {
    let mut tx = ctx.store.begin().await?;
    sqlx::query(
        "INSERT INTO agent_runs (id, project_id, agent, trigger, state, started_at, goal_id, task_id, \
         parent_run_id, model) VALUES (?, ?, ?, ?, 'running', ?, ?, ?, ?, ?)",
    )
    .bind(run.to_string())
    .bind(ctx.project.to_string())
    .bind(agent)
    .bind(serde_json::json!({"kind": "subagent", "prompt": tasks::truncate(prompt, 2000)}).to_string())
    .bind(Timestamp::now().0)
    .bind(ctx.scope.goal().map(|g| g.to_string()))
    .bind(match ctx.scope {
        Scope::Task { task, .. } => Some(task.to_string()),
        _ => None,
    })
    .bind(ctx.run_id.to_string())
    .bind(model)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn record_end(
    ctx: &ToolCtx,
    run: Uuid,
    state: &str,
    error: Option<String>,
) -> Result<(), Error> {
    let mut tx = ctx.store.begin().await?;
    sqlx::query("UPDATE agent_runs SET state = ?, error = ?, finished_at = ? WHERE id = ?")
        .bind(state)
        .bind(error)
        .bind(Timestamp::now().0)
        .bind(run.to_string())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
