//! Planner：每個 Goal 一個、不掛記憶的 agent，run 排在持久化的 `agent_runs` 佇列裡
//! （`proposal/agent-runtime.md#執行模型`）。
//!
//! - 觸發（Goal 建立、steer、你改了 Brief）在改狀態的同一個 transaction 裡以 `queued` 寫入
//!   [`enqueue`]；同一個 Goal 已經有排隊中的 run 時把觸發併進去，不另排一個。
//! - runner 取走 run 時改成 `running` 並持有 lease，執行期間定期心跳；每個 Goal 同時最多一個
//!   run。SQLite 沒有 `FOR UPDATE SKIP LOCKED`，由單一 runner 在寫入 transaction 裡挑選與標記。
//! - steer 會中斷這個 Goal 正在跑的 run，之後跑新排的那個；Goal 結束時中斷並清掉排隊的。
//! - 服務關閉時被中斷的 run 回到 `queued`；當掉時留下的 `running` 在下次啟動時由
//!   [`requeue_stale`] 放回佇列（重試超過 [`MAX_ATTEMPTS`] 次就標成失敗）。
//!
//! 每次 run 的 context 從狀態重建：Brief、Ledger、任務樹與觸發原因（[`render_trigger`]），
//! 不讀對話。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pots_llm::agent::{self, RunConfig};
use pots_llm::{Item, LlmError};
use pots_proto::{ArtifactKind, Card, GoalId, ProjectId, Timestamp};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::sqlite::SqliteConnection;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use uuid::Uuid;

use crate::Error;
use crate::artifacts;
use crate::hub::{Hub, LogTxn};
use crate::profiles::AgentProfile;
use crate::providers::ProviderRouter;
use crate::runs::explain;
use crate::store::{Store, parse_id};
use crate::tasks::{self, GoalRow, Principal};
use crate::tools::{self, Scope, ToolCtx};

/// lease 長度；runner 每 [`HEARTBEAT`] 延長一次。
const LEASE: Duration = Duration::from_secs(90);
const HEARTBEAT: Duration = Duration::from_secs(30);
/// 沒有通知時多久檢查一次佇列（例如 lease 過期的 run）。
const POLL: Duration = Duration::from_secs(30);
/// 同時跑幾個 Planner run（不同 Goal）。
const MAX_PARALLEL: usize = 4;
/// 一個 run 最多被取走幾次（當掉重來也算）。
pub const MAX_ATTEMPTS: i64 = 3;
/// 狀態面板上 Planner 備註的長度上限。
const MAX_NOTE_CHARS: usize = 300;

/// 叫醒 Planner 的原因，寫進 `agent_runs.trigger`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlannerTrigger {
    GoalCreated {
        by: String,
    },
    Steer {
        by: String,
        reason: String,
    },
    /// 你在編輯器改了 Brief（或還原成舊版本）。
    BriefEdited {
        rev: u32,
    },
    /// 這個 Goal 訂閱的資源上有新的外部事件（listener 開的 Goal）。`event` 是給 agent 看的說明。
    External {
        event: String,
    },
    /// Planner 自己設定的排程到了。
    Scheduled {
        prompt: String,
    },
    /// 一個任務結束了（完成、失敗或需要決定）。
    TaskEnded {
        task: pots_proto::TaskId,
        title: String,
        state: String,
        detail: String,
    },
    /// worker 在 team 發了問題或阻礙。
    TeamPost {
        post: String,
        from: String,
        text: String,
    },
    /// 快速修改超出範圍，升級成這個 Goal。
    Escalated {
        reason: String,
    },
    /// 你核准了追加預算（M6）。
    BudgetApproved {
        tokens: u64,
    },
    /// 預算熔斷解除了（M6）。
    BreakerCleared,
}

impl PlannerTrigger {
    fn describe(&self) -> String {
        match self {
            Self::GoalCreated { by } => format!("{by}剛建立這個 Goal，請開始規劃。"),
            Self::Steer { by, reason } => {
                format!("{by}要求改方向：{}", tasks::one_line(reason, 1000))
            }
            Self::BriefEdited { rev } => {
                format!("使用者直接修改了 Brief（現在是 v{rev}），請依新的 Brief 重新檢查規劃。")
            }
            Self::External { event } => {
                format!("這個 Goal 追蹤的資源有新的外部事件，看看要不要調整規劃：\n\n{event}")
            }
            Self::Scheduled { prompt } => {
                format!(
                    "你之前設定的排程時間到了：{}",
                    tasks::one_line(prompt, 2000)
                )
            }
            Self::TaskEnded {
                task,
                title,
                state,
                detail,
            } => format!(
                "任務「{}」（task_id：{task}）{state}：{}",
                tasks::one_line(title, 200),
                tasks::one_line(detail, 1500)
            ),
            Self::TeamPost { post, from, text } => format!(
                "{from}在 team 發了{post}：{}（用 post_to_team 發 decision 回答）",
                tasks::one_line(text, 1000)
            ),
            Self::Escalated { reason } => format!(
                "這個 Goal 原本是快速修改，worker 發現超出範圍而升級：{}。請依 Brief 重新規劃（已做的修改在任務的分支上）。",
                tasks::one_line(reason, 1000)
            ),
            Self::BudgetApproved { tokens } => {
                format!("使用者核准追加 {tokens} tokens 的預算，請繼續這個 Goal。")
            }
            Self::BreakerCleared => "使用者的預算熔斷已解除，請繼續這個 Goal。".to_string(),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TriggerList {
    triggers: Vec<PlannerTrigger>,
}

/// 排一個 Planner run；同一個 Goal 已經有排隊中的 run 時把觸發併進去。
/// 要和觸發它的狀態變更放在同一個 transaction，commit 後呼叫 [`Planner::kick`]
/// （steer 用 [`Planner::steer`]）。快速修改（`quick` 種類的 Goal）沒有 Planner：不排
/// （升級成一般 Goal 時先改種類再排）。
pub async fn enqueue(
    conn: &mut SqliteConnection,
    project: ProjectId,
    goal: GoalId,
    trigger: &PlannerTrigger,
) -> Result<(), Error> {
    if tasks::goal_kind(conn, goal).await? == "quick" {
        return Ok(());
    }
    let queued = sqlx::query(
        "SELECT id, trigger FROM agent_runs WHERE agent = 'planner' AND goal_id = ? AND state = 'queued' \
         ORDER BY started_at, id LIMIT 1",
    )
    .bind(goal.to_string())
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(row) = queued {
        let id: String = row.try_get("id")?;
        let text: String = row.try_get("trigger")?;
        let mut list: TriggerList = serde_json::from_str(&text).unwrap_or_default();
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
        "INSERT INTO agent_runs (id, project_id, agent, trigger, state, started_at, goal_id) \
         VALUES (?, ?, 'planner', ?, 'queued', ?, ?)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(project.to_string())
    .bind(serde_json::to_string(&list)?)
    .bind(Timestamp::now().0)
    .bind(goal.to_string())
    .execute(conn)
    .await?;
    Ok(())
}

/// Goal 結束時清掉排隊中的 run。
pub async fn drop_queued(conn: &mut SqliteConnection, goal: GoalId) -> Result<(), Error> {
    sqlx::query(
        "UPDATE agent_runs SET state = 'cancelled', finished_at = ? \
         WHERE agent = 'planner' AND goal_id = ? AND state = 'queued'",
    )
    .bind(Timestamp::now().0)
    .bind(goal.to_string())
    .execute(conn)
    .await?;
    Ok(())
}

/// 重試次數用完時狀態面板顯示的原因。
const GAVE_UP: &str =
    "Planner 沒有完成：服務中斷，重試次數已用完。改 Brief 或在對話裡要求改方向可以再叫醒它。";

/// 啟動時：上次沒跑完的 Planner run 放回佇列；重試次數用完的標成失敗。
/// 資料目錄有檔案鎖，這時不可能有別的行程持有 lease。回傳放回幾個，與放棄的 run 所屬的 Goal
/// （呼叫端用 [`note_gave_up`] 更新它們的狀態面板）。
pub async fn requeue_stale(conn: &mut SqliteConnection) -> Result<(u64, Vec<GoalId>), Error> {
    let now = Timestamp::now().0;
    let failed: Vec<String> = sqlx::query_scalar(
        "SELECT goal_id FROM agent_runs WHERE agent = 'planner' AND state = 'running' \
         AND attempts >= ? AND goal_id IS NOT NULL",
    )
    .bind(MAX_ATTEMPTS)
    .fetch_all(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE agent_runs SET state = 'failed', error = '服務中斷，重試次數已用完', finished_at = ?, \
         lease_until = NULL WHERE agent = 'planner' AND state = 'running' AND attempts >= ?",
    )
    .bind(now)
    .bind(MAX_ATTEMPTS)
    .execute(&mut *conn)
    .await?;
    let done = sqlx::query(
        "UPDATE agent_runs SET state = 'queued', lease_until = NULL \
         WHERE agent = 'planner' AND state = 'running'",
    )
    .execute(&mut *conn)
    .await?;
    let failed = failed
        .into_iter()
        .map(parse_id)
        .collect::<Result<Vec<GoalId>, _>>()?;
    Ok((done.rows_affected(), failed))
}

/// Planner 放棄了這些 Goal 的 run：狀態面板顯示原因（不再停在「規劃中」）。
pub async fn note_gave_up(txn: &mut LogTxn<'_>, goals: &[GoalId]) -> Result<(), Error> {
    for &goal in goals {
        if tasks::goal(txn.conn(), goal)
            .await?
            .is_some_and(|g| g.state.is_active())
        {
            tasks::set_planner_note(txn.conn(), goal, GAVE_UP).await?;
            tasks::push_goal(txn, goal).await?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopReason {
    Steer,
    GoalClosed,
}

struct Active {
    run: Uuid,
    cancel: CancellationToken,
    reason: Option<StopReason>,
}

struct Inner {
    hub: Arc<Hub>,
    store: Store,
    router: Arc<ProviderRouter>,
    profile: AgentProfile,
    clock: crate::config::Clock,
    worker: crate::worker::Worker,
    notify: Notify,
    active: Mutex<HashMap<GoalId, Active>>,
    tracker: TaskTracker,
    shutdown: CancellationToken,
}

#[derive(Clone)]
pub struct Planner {
    inner: Arc<Inner>,
}

/// 取走的一個 run。
struct Job {
    run: Uuid,
    project: ProjectId,
    goal: GoalId,
    triggers: Vec<PlannerTrigger>,
    cancel: CancellationToken,
}

impl Planner {
    pub fn new(
        hub: Arc<Hub>,
        store: Store,
        router: Arc<ProviderRouter>,
        profile: AgentProfile,
        clock: crate::config::Clock,
        worker: crate::worker::Worker,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                hub,
                store,
                router,
                profile,
                clock,
                worker,
                notify: Notify::new(),
                active: Mutex::new(HashMap::new()),
                tracker: TaskTracker::new(),
                shutdown: CancellationToken::new(),
            }),
        }
    }

    pub fn worker(&self) -> &crate::worker::Worker {
        &self.inner.worker
    }

    /// 排程、等待與 listener 用的時鐘（工具共用）。
    pub fn clock(&self) -> &crate::config::Clock {
        &self.inner.clock
    }

    /// 開始處理佇列。
    pub fn start(&self) {
        let planner = self.clone();
        self.inner
            .tracker
            .spawn(async move { planner.runner().await });
    }

    /// 佇列有新的 run。
    pub fn kick(&self) {
        self.inner.notify.notify_one();
    }

    /// 中斷這個 Goal 正在跑的 run（之後跑新排的那個）。
    pub fn steer(&self, goal: GoalId) {
        self.interrupt(goal, StopReason::Steer);
        self.kick();
    }

    /// Goal 結束了：中斷正在跑的 run。
    pub fn close_goal(&self, goal: GoalId) {
        self.interrupt(goal, StopReason::GoalClosed);
    }

    fn interrupt(&self, goal: GoalId, reason: StopReason) {
        if let Some(a) = self.inner.active.lock().unwrap().get_mut(&goal) {
            a.reason = Some(reason);
            a.cancel.cancel();
        }
    }

    /// 中斷所有 run 並等它們收尾（最多 `timeout`）；被中斷的 run 回到佇列。
    pub async fn shutdown(&self, timeout: Duration) {
        self.inner.shutdown.cancel();
        self.inner.tracker.close();
        if tokio::time::timeout(timeout, self.inner.tracker.wait())
            .await
            .is_err()
        {
            tracing::warn!("Planner run 沒有在時限內結束");
        }
    }

    async fn runner(&self) {
        loop {
            while !self.inner.shutdown.is_cancelled()
                && self.inner.active.lock().unwrap().len() < MAX_PARALLEL
            {
                match self.claim().await {
                    Ok(Some(job)) => {
                        let planner = self.clone();
                        self.inner
                            .tracker
                            .spawn(async move { planner.execute(job).await });
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::error!(error = %e, "無法從佇列取出 Planner run");
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

    /// 取出下一個可以跑的 run：`queued`，而且同一個 Goal 沒有 run 在跑。所屬 Goal 已經結束的
    /// 直接標成取消；重試次數用完的標成失敗。
    ///
    /// lease 過期的 `running` 不在這裡重新取走：資料目錄有檔案鎖，執行期間看到的 `running`
    /// 一定是這個行程自己的（可能只是心跳延遲，例如電腦睡眠），重新取走會讓同一個 Goal 同時跑
    /// 兩個 Planner。當掉留下的由啟動時的 [`requeue_stale`] 放回佇列；lease 留給多行程部署。
    async fn claim(&self) -> Result<Option<Job>, Error> {
        let inner = &self.inner;
        loop {
            let mut txn = inner.hub.begin().await?;
            let now = Timestamp::now().0;
            let row = sqlx::query(
                "SELECT r.id, r.project_id, r.goal_id, r.trigger, r.attempts FROM agent_runs r \
                 WHERE r.agent = 'planner' AND r.goal_id IS NOT NULL AND r.state = 'queued' \
                 AND NOT EXISTS (SELECT 1 FROM agent_runs x WHERE x.goal_id = r.goal_id \
                     AND x.agent = 'planner' AND x.state = 'running') \
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
            let goal: GoalId = parse_id(row.try_get("goal_id")?)?;
            let attempts: i64 = row.try_get("attempts")?;
            let trigger: String = row.try_get("trigger")?;
            let list: TriggerList = serde_json::from_str(&trigger).unwrap_or_default();

            let active = tasks::goal(txn.conn(), goal)
                .await?
                .is_some_and(|g| g.state.is_active());
            if !active || attempts >= MAX_ATTEMPTS {
                let (state, error) = if active {
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
                if active {
                    note_gave_up(&mut txn, &[goal]).await?;
                }
                txn.commit().await?;
                continue;
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
            // 在日誌鎖內登記，commit 之後的 steer／close_goal 一定找得到它。
            let cancel = inner.shutdown.child_token();
            txn.commit_then(|| {
                inner.active.lock().unwrap().insert(
                    goal,
                    Active {
                        run,
                        cancel: cancel.clone(),
                        reason: None,
                    },
                );
            })
            .await?;
            return Ok(Some(Job {
                run,
                project,
                goal,
                triggers: list.triggers,
                cancel,
            }));
        }
    }

    #[tracing::instrument(
        name = "agent.run",
        skip_all,
        fields(agent = "planner", project = %job.project, run = %job.run, goal = %job.goal, model = tracing::field::Empty)
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
                        tracing::warn!(%run, error = %e, "無法延長 Planner run 的 lease");
                    }
                }
            })
        };

        let result = self.run(&job).await;
        beat.cancel();
        let _ = heartbeat.await;

        let reason = {
            let mut active = inner.active.lock().unwrap();
            let reason = active.get(&job.goal).and_then(|a| a.reason);
            if active.get(&job.goal).is_some_and(|a| a.run == job.run) {
                active.remove(&job.goal);
            }
            reason
        };
        if let Err(e) = self.finish(&job, result, reason).await {
            tracing::error!(run = %job.run, error = %e, "無法記錄 Planner run 的結束，改用最少的收尾");
            // 列一直停在 running 的話，這個 Goal 之後的 run 都取不走（直到重啟）。
            if let Err(e) = mark_failed(&inner.store, job.run).await {
                tracing::error!(run = %job.run, error = %e, "無法把 Planner run 標成失敗");
            }
        }
        // 同一個 Goal 可能有排隊中的 run 在等這個結束。
        self.kick();
    }

    /// 跑一次 Planner。回傳要顯示在狀態面板的備註。
    async fn run(&self, job: &Job) -> Result<RunEnd, Error> {
        let inner = &self.inner;
        let reader = format!("planner:{}", job.goal);
        let Some(project) = inner.store.project(job.project).await? else {
            return Ok(RunEnd::Skipped);
        };

        // 組 context，記下讀到的版本，狀態面板顯示「規劃中」。
        let message = {
            let mut txn = inner.hub.begin().await?;
            let Some(goal) = tasks::goal(txn.conn(), job.goal).await? else {
                return Ok(RunEnd::Skipped);
            };
            if !goal.state.is_active() {
                return Ok(RunEnd::Skipped);
            }
            let message = render_trigger(txn.conn(), &goal, &job.triggers).await?;
            for kind in [ArtifactKind::Brief, ArtifactKind::Ledger] {
                if let Some(a) = artifacts::for_goal(txn.conn(), goal.id, kind).await? {
                    artifacts::mark_read(txn.conn(), a.id, &reader, a.rev).await?;
                }
            }
            tasks::set_planner_note(txn.conn(), goal.id, "Planner 規劃中…").await?;
            tasks::push_goal(&mut txn, goal.id).await?;
            txn.commit().await?;
            message
        };

        let route = match inner.router.resolve() {
            Ok(r) => r,
            Err(e) => return Ok(RunEnd::Llm(e)),
        };
        tracing::Span::current().record("model", tracing::field::display(&route.model));
        let (head, tail) = {
            let mut conn = inner.store.acquire().await?;
            crate::user_md::compose(&mut conn, project.id).await?
        };
        let mut instructions = format!("{head}{}", inner.profile.role_prompt());
        instructions.push_str(&format!("\n\n這個 Goal 屬於 project「{}」。", project.name));
        if !project.instructions.trim().is_empty() {
            instructions.push_str("\n\n# Project 指示\n\n");
            instructions.push_str(project.instructions.trim());
        }
        instructions.push_str(&tail);
        let budget = crate::budget::for_run(
            &inner.store,
            &inner.hub,
            &inner.clock,
            job.run,
            job.project,
            Some(job.goal),
            None,
            route.meta("builtin"),
        )
        .await?;
        let ctx = Arc::new(ToolCtx {
            hub: inner.hub.clone(),
            store: inner.store.clone(),
            planner: self.clone(),
            project: job.project,
            scope: Scope::Goal(job.goal),
            actor: Principal::agent(&inner.profile.name),
            reader,
            artifact_kinds: inner.profile.artifact_kinds.clone(),
            run_id: job.run,
            cards: Arc::new(Mutex::new(Vec::<Card>::new())),
            finished: Default::default(),
            budget,
            milestones: Default::default(),
            goals_created: Default::default(),
            queued: None,
            clock: inner.clock.clone(),
            worker: inner.worker.clone(),
            work: None,
            code: None,
            stop: Arc::new(tokio::sync::Notify::new()),
            cancel: job.cancel.clone(),
        });
        let gate = ctx.gate();
        let config = RunConfig {
            provider: route.provider,
            model: route.model,
            instructions: Some(instructions),
            tools: tools::build(&inner.profile, ctx),
            hook: Arc::new(gate),
            max_turns: inner.profile.max_turns,
            session_id: Some(format!("goal:{}", job.goal)),
        };
        // Planner 的輸出不串流給 UI；最後的文字當作狀態面板的備註。
        let (events, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);
        let history = vec![Item::User { text: message }];
        match agent::run(&config, history, events, job.cancel.clone()).await {
            Ok(r) => {
                let _ = r.usage;
                let text = r
                    .new_items
                    .iter()
                    .rev()
                    .find_map(|i| match i {
                        Item::Assistant { text } if !text.trim().is_empty() => Some(text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                Ok(RunEnd::Done {
                    text,
                    max_turns: r.outcome == agent::RunOutcome::MaxTurns,
                })
            }
            Err(e) => Ok(RunEnd::Llm(e)),
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
        let conn = txn.conn();
        let stop: Option<String> =
            sqlx::query_scalar("SELECT stop_reason FROM agent_runs WHERE id = ?")
                .bind(job.run.to_string())
                .fetch_optional(&mut *conn)
                .await?
                .flatten();
        // (run 的狀態, 錯誤, 新的 Planner 備註)
        let (state, error, note): (&str, Option<String>, Option<String>) = match result {
            Ok(RunEnd::Skipped) => ("cancelled", None, None),
            Ok(RunEnd::Done { text, max_turns }) => {
                let mut note = if text.trim().is_empty() {
                    "Planner 完成了這一輪".to_string()
                } else {
                    tasks::truncate(&text, MAX_NOTE_CHARS)
                };
                if max_turns {
                    note.push_str("（已達單次工具輪數上限）");
                }
                match stop.as_deref() {
                    Some("budget") => {
                        note = "Goal 的預算用完了，等你決定要不要追加預算（Inbox）".into();
                    }
                    Some("breaker") => {
                        note = "這個 project 的預算熔斷中，暫停所有 run".into();
                    }
                    _ => {}
                }
                ("done", None, Some(note))
            }
            Ok(RunEnd::Llm(LlmError::Cancelled)) if shutdown && reason.is_none() => {
                // 服務關閉：放回佇列，下次啟動再跑；這次不算一次重試。
                sqlx::query(
                    "UPDATE agent_runs SET state = 'queued', lease_until = NULL, \
                     attempts = CASE WHEN attempts > 0 THEN attempts - 1 ELSE 0 END WHERE id = ?",
                )
                .bind(job.run.to_string())
                .execute(&mut *conn)
                .await?;
                return txn.commit().await;
            }
            Ok(RunEnd::Llm(LlmError::Cancelled)) => {
                let note = match reason {
                    Some(StopReason::Steer) => Some("方向改了，Planner 重新規劃中…".to_string()),
                    _ => None,
                };
                ("cancelled", None, note)
            }
            Ok(RunEnd::Llm(e)) => (
                "failed",
                Some(e.to_string()),
                Some(format!("Planner 沒有完成：{}", explain(&e))),
            ),
            Err(e) => {
                tracing::error!(run = %job.run, error = %e, "Planner run 失敗");
                (
                    "failed",
                    Some(e.to_string()),
                    Some("Planner 沒有完成：伺服器內部錯誤".to_string()),
                )
            }
        };
        sqlx::query(
            "UPDATE agent_runs SET state = ?, error = ?, finished_at = ?, lease_until = NULL WHERE id = ?",
        )
        .bind(state)
        .bind(error)
        .bind(Timestamp::now().0)
        .bind(job.run.to_string())
        .execute(&mut *conn)
        .await?;
        // 被 steer 中斷的 run：它的觸發併進新排的 run，讓下一輪知道完整的原因。
        if reason == Some(StopReason::Steer) && state == "cancelled" {
            for t in &job.triggers {
                enqueue(conn, job.project, job.goal, t).await?;
            }
        }
        // Goal 已經結束（例如這一輪呼叫了 complete_goal）就不蓋掉狀態面板。
        if let Some(note) = note
            && tasks::goal(conn, job.goal)
                .await?
                .is_some_and(|g| g.state.is_active())
        {
            tasks::set_planner_note(conn, job.goal, &note).await?;
            tasks::push_goal(&mut txn, job.goal).await?;
        }
        txn.commit().await?;
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

enum RunEnd {
    /// project 或 Goal 已經不在（或已結束），沒有跑。
    Skipped,
    Done {
        text: String,
        max_turns: bool,
    },
    Llm(LlmError),
}

/// `finish` 失敗時的最後手段：只把 run 標成失敗，不記事件。
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

/// Planner 的觸發訊息：為什麼被叫醒、Goal、Brief、Ledger 與任務樹。
pub async fn render_trigger(
    conn: &mut SqliteConnection,
    goal: &GoalRow,
    triggers: &[PlannerTrigger],
) -> Result<String, Error> {
    let mut out = String::from("# 為什麼叫醒你\n\n");
    if triggers.is_empty() {
        out.push_str("- 繼續規劃這個 Goal。\n");
    }
    for t in triggers {
        out.push_str(&format!("- {}\n", t.describe()));
    }
    out.push_str(&format!(
        "\n# Goal\n\n- id：{}\n- 標題：{}\n- 狀態：{}\n",
        goal.id,
        tasks::one_line(&goal.title, 200),
        goal.state.label()
    ));
    for kind in [ArtifactKind::Brief, ArtifactKind::Ledger] {
        out.push_str(&format!("\n# {}\n\n", kind.label()));
        match artifacts::for_goal(conn, goal.id, kind).await? {
            Some(a) => {
                out.push_str(&format!("artifact_id：{}，rev：{}\n\n", a.id, a.rev));
                out.push_str("<artifact>\n");
                out.push_str(a.body.trim());
                out.push_str("\n</artifact>\n");
            }
            None => out.push_str("（還沒有）\n"),
        }
    }
    out.push_str("\n# 任務樹\n\n");
    let tasks = tasks::tasks(conn, goal.id).await?;
    if tasks.is_empty() {
        out.push_str("（還沒有任務）\n");
    }
    // 依 parent 縮排；找不到 parent 的當作頂層。
    fn walk(
        out: &mut String,
        tasks: &[tasks::TaskRow],
        parent: Option<pots_proto::TaskId>,
        depth: usize,
    ) {
        for t in tasks.iter().filter(|t| t.parent == parent) {
            out.push_str(&format!(
                "{}- [{}] {}（task_id：{}）",
                "  ".repeat(depth),
                t.state.name(),
                tasks::one_line(&t.title, 200),
                t.id
            ));
            if !t.summary.is_empty() {
                out.push_str(&format!("：{}", tasks::one_line(&t.summary, 2000)));
            }
            out.push('\n');
            walk(out, tasks, Some(t.id), depth + 1);
        }
    }
    let ids: Vec<_> = tasks.iter().map(|t| t.id).collect();
    walk(&mut out, &tasks, None, 0);
    for t in tasks
        .iter()
        .filter(|t| t.parent.is_some_and(|p| !ids.contains(&p)))
    {
        out.push_str(&format!(
            "- [{}] {}（task_id：{}）\n",
            t.state.name(),
            tasks::one_line(&t.title, 200),
            t.id
        ));
    }
    let posts = crate::team::unread(
        conn,
        goal.id,
        &format!("planner:{}", goal.id),
        &Principal::agent("planner"),
        None,
    )
    .await?;
    if !posts.is_empty() {
        out.push_str("\n# Team 的新貼文\n\n");
        out.push_str(&crate::team::render(conn, &posts).await?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use pots_proto::GoalState;

    use super::*;
    use crate::store::ProjectRow;

    async fn state_of(store: &Store, goal: GoalId) -> Vec<(String, i64, String)> {
        let mut conn = store.acquire().await.unwrap();
        sqlx::query("SELECT state, attempts, trigger FROM agent_runs WHERE goal_id = ? ORDER BY started_at, id")
            .bind(goal.to_string())
            .fetch_all(&mut *conn)
            .await
            .unwrap()
            .iter()
            .map(|r| (r.get("state"), r.get("attempts"), r.get("trigger")))
            .collect()
    }

    /// 同一個 Goal 排隊中的觸發會合併；當掉留下的 running 放回佇列，重試用完的標成失敗。
    #[tokio::test]
    async fn queue_merges_and_recovers() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open_sqlite(&tmp.path().join("pots.db"))
            .await
            .unwrap();
        let project = ProjectId::new();
        let goals = [GoalId::new(), GoalId::new()];
        let mut tx = store.begin().await.unwrap();
        Store::insert_project(
            &mut tx,
            &ProjectRow {
                id: project,
                name: "p".into(),
                instructions: String::new(),
                agent_name: "Pots".into(),
                created_at: Timestamp::now(),
            },
        )
        .await
        .unwrap();
        for g in goals {
            let row = GoalRow {
                id: g,
                project,
                title: "g".into(),
                state: GoalState::Active,
                planner_note: String::new(),
                created_at: Timestamp::now(),
                archived_at: None,
            };
            tasks::insert_goal(&mut tx, &row, &Principal::Human)
                .await
                .unwrap();
            enqueue(
                &mut tx,
                project,
                g,
                &PlannerTrigger::GoalCreated { by: "你".into() },
            )
            .await
            .unwrap();
        }
        enqueue(
            &mut tx,
            project,
            goals[0],
            &PlannerTrigger::BriefEdited { rev: 2 },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        let runs = state_of(&store, goals[0]).await;
        assert_eq!(runs.len(), 1);
        let list: TriggerList = serde_json::from_str(&runs[0].2).unwrap();
        assert_eq!(list.triggers.len(), 2);

        // 模擬當掉：第一個 Goal 的 run 跑到一半（第一次），第二個已經重試到上限。
        let mut tx = store.begin().await.unwrap();
        for (g, attempts) in [(goals[0], 1), (goals[1], MAX_ATTEMPTS)] {
            sqlx::query("UPDATE agent_runs SET state = 'running', attempts = ?, lease_until = 0 WHERE goal_id = ?")
                .bind(attempts)
                .bind(g.to_string())
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        let (requeued, gave_up) = requeue_stale(&mut tx).await.unwrap();
        assert_eq!(requeued, 1);
        assert_eq!(gave_up, [goals[1]]);
        tx.commit().await.unwrap();
        assert_eq!(state_of(&store, goals[0]).await[0].0, "queued");
        assert_eq!(state_of(&store, goals[1]).await[0].0, "failed");

        // Goal 結束時清掉排隊中的 run。
        let mut tx = store.begin().await.unwrap();
        drop_queued(&mut tx, goals[0]).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(state_of(&store, goals[0]).await[0].0, "cancelled");
    }
}
