//! 階層預算、熔斷與 run trace（proposal/security-and-operations.md#預算、#可觀測性）。
//!
//! 記帳在 [`Hook::after_turn`](pots_llm::agent::Hook) 同步發生：`Gate` 每輪模型呼叫結束後
//! 呼叫 [`Budget::record_turn`]，所以下一輪的預算檢查看得到剛剛用掉的 token。trace 與用量
//! 寫進 `agent_runs` 與 `run_trace`，跑完就留在資料庫，run 失敗或取消也留得住。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use pots_llm::Reasoning;
use pots_llm::provider::{ToolCall, Usage};
use pots_proto::{GoalId, ProjectId, TaskId, Timestamp};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::approvals::{self, Action, NewApproval};
use crate::config::Clock;
use crate::store::Store;
use crate::tasks;
use crate::tasks::Principal;
use crate::{Error, Hub};

// 預設不限：新 Goal 的 `budget_tokens` 是 NULL、project 沒設 `hourly_budget_tokens` 就不熔斷。
// 先只記用量（成本報表／用量頁）觀察一陣子，再決定要不要設預設預算。
/// agent 自己要求預算時的下限（沒有上限）。
pub const MIN_GOAL_BUDGET_TOKENS: u64 = 20_000;
/// 追加預算時預設加多少：原本預算的一半，至少 10 萬。
const MIN_BUDGET_STEP_TOKENS: u64 = 100_000;
/// trace 裡工具參數與結果最多留幾個字。
const TRACE_ARGS_CHARS: usize = 1200;
const TRACE_OUTPUT_CHARS: usize = 600;
/// trace 裡模型在工具呼叫之間說的話最多留幾個字。
const TRACE_TEXT_CHARS: usize = 2000;

/// 一輪開始前停下來的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnStop {
    /// 這個 Goal 的預算用完了。
    Budget,
    /// project 的每小時上限熔斷中。
    Breaker,
}

impl TurnStop {
    fn stop_reason(self) -> &'static str {
        match self {
            Self::Budget => "budget",
            Self::Breaker => "breaker",
        }
    }
}

/// 記進 `agent_runs` 的模型、provider 與 runtime 維度（成本報表用）。
#[derive(Clone, Debug)]
pub struct RunMeta {
    pub model: String,
    /// 顯示名稱，例如「ChatGPT」「OpenAI」「Claude Code」。
    pub provider: String,
    /// `subscription`、`api` 或 `cli`。
    pub billing: String,
    /// models.dev 的 provider 鍵（單價表用）。
    pub pricing_provider: String,
    /// `builtin` 或 `claude_code`。
    pub runtime: String,
    /// 角色設定的 thinking；`None` 是模型預設（Claude Code 一律 `None`）。記在 trace 的
    /// `model` 步驟，「為什麼」面板看得到。
    pub reasoning: Option<Reasoning>,
}

impl RunMeta {
    /// Claude Code 由 CLI 回報費用，沒有 provider 的 token 用量。
    pub fn claude_code(model: &str) -> Self {
        Self {
            model: if model.trim().is_empty() {
                "claude-code".into()
            } else {
                model.to_string()
            },
            provider: "Claude Code".into(),
            billing: "cli".into(),
            pricing_provider: "anthropic".into(),
            runtime: "claude_code".into(),
            reasoning: None,
        }
    }
}

/// 一次 run 的預算與 trace。`limit` 是這個 run 開始時剩下的 token（`None` 表示不限）。
pub struct Budget {
    store: Store,
    hub: Arc<Hub>,
    clock: Clock,
    run: Uuid,
    project: ProjectId,
    goal: Option<GoalId>,
    task: Option<TaskId>,
    model: String,
    reasoning: Option<Reasoning>,
    exhausted: AtomicBool,
    traced: AtomicU64,
}

impl std::fmt::Debug for Budget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Budget").field("run", &self.run).finish()
    }
}

impl Budget {
    /// 讀 Goal 的預算與已用，算出這個 run 還剩多少。
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        store: Store,
        hub: Arc<Hub>,
        clock: Clock,
        run: Uuid,
        project: ProjectId,
        goal: Option<GoalId>,
        task: Option<TaskId>,
        meta: RunMeta,
    ) -> Result<Arc<Self>, Error> {
        // 「為什麼」面板要知道這個 run 用的模型；成本報表要 provider 與 runtime。
        Store::set_run_meta(&store, run, &meta).await?;
        Ok(Arc::new(Self {
            store,
            hub,
            clock,
            run,
            project,
            goal,
            task,
            model: meta.model,
            reasoning: meta.reasoning,
            exhausted: AtomicBool::new(false),
            traced: AtomicU64::new(0),
        }))
    }

    pub fn exhausted(&self) -> bool {
        self.exhausted.load(Ordering::Relaxed)
    }

    /// 想知道下一個 turn 能不能開始；不能時順便記下原因。
    pub async fn check_turn(&self) -> Option<TurnStop> {
        if self.exhausted() {
            return Some(TurnStop::Budget);
        }
        // 每輪都讀 Goal 的總用量，不是 run 開始時的快照：同一個 Goal 的 Planner、worker
        // 與 subagent 各自開 run，只比單一 run 的用量會超過預算。
        if let Some(goal) = self.goal
            && let Ok(Some((Some(budget), used, _))) = self.store.goal_budget(goal).await
            && used >= budget
        {
            self.exhausted.store(true, Ordering::SeqCst);
            self.note_stop(TurnStop::Budget).await;
            return Some(TurnStop::Budget);
        }
        match self.store.project_breaker_until(self.project).await {
            Ok(Some(_)) => {
                self.note_stop(TurnStop::Breaker).await;
                return Some(TurnStop::Breaker);
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, "讀取熔斷狀態失敗"),
        }
        // 沒設每小時上限的 project 不熔斷（只記用量）。
        let limit = match self.store.project_hourly_budget(self.project).await {
            Ok(Some(limit)) => limit,
            Ok(None) => return None,
            Err(e) => {
                tracing::warn!(error = %e, "讀取每小時預算失敗");
                return None;
            }
        };
        let since = Timestamp(self.clock.now().0.saturating_sub(3600));
        match self.store.project_usage_since(self.project, since).await {
            Ok(used) if used >= limit => {
                if let Err(e) = self.trip(used, limit).await {
                    tracing::error!(error = %e, "無法觸發預算熔斷");
                }
                self.note_stop(TurnStop::Breaker).await;
                Some(TurnStop::Breaker)
            }
            Ok(_) => None,
            Err(e) => {
                tracing::warn!(error = %e, "計算每小時用量失敗");
                None
            }
        }
    }

    /// 一小時內用超過上限：暫停這個 project 的 run，建立一筆「恢復」核准。
    async fn trip(&self, used: u64, limit: u64) -> Result<(), Error> {
        let now = self.clock.now();
        let mut txn = self.hub.begin().await?;
        let marked = sqlx::query(
            "UPDATE projects SET breaker_until = ? WHERE id = ? AND breaker_until IS NULL",
        )
        .bind(now.0)
        .bind(self.project.to_string())
        .execute(txn.conn())
        .await?
        .rows_affected();
        if marked == 0 {
            return Ok(());
        }
        // 排隊中的 run 留在佇列（claim 會跳過熔斷中的 project）；正在跑的在下一輪前停下，
        // 核准恢復後佇列與 Planner 會接著做。
        approvals::insert(
            &mut txn,
            NewApproval {
                project: self.project,
                goal: None,
                task: None,
                action: Action::ResumeProject {
                    project: self.project,
                },
                title: "專案預算熔斷".into(),
                reason: format!(
                    "這個 project 在過去一小時用了 {used} tokens（上限 {limit}），已暫停所有 run。\
                     核准後恢復；拒絕或逾時則維持暫停。"
                ),
                by: Principal::agent("budget"),
                notify: true,
            },
            now,
        )
        .await?;
        crate::tools::post_card(
            &mut txn,
            self.project,
            pots_proto::Card::Notice {
                title: "預算熔斷".into(),
                body: format!(
                    "過去一小時用了 {used} tokens，超過上限 {limit}。所有 run 已暫停，\
                     在 Inbox 核准「專案預算熔斷」後恢復。"
                ),
                actions: Vec::new(),
            },
            None,
        )
        .await?;
        txn.commit().await?;
        Ok(())
    }

    /// 一輪模型呼叫結束：用量記進 `agent_runs`、Goal／任務與 trace。`narration` 是這一輪接著工具呼叫的
    /// 文字，另外記成一筆 `text` trace，「為什麼」面板跟工具依序列出。
    pub async fn record_turn(&self, turn: usize, usage: Usage, narration: Option<&str>) {
        let total = usage.input_tokens.saturating_add(usage.output_tokens);
        let now = self.clock.now();
        let mut tx = match self.store.begin().await {
            Ok(tx) => tx,
            Err(e) => {
                tracing::error!(error = %e, "無法記錄模型用量");
                return;
            }
        };
        if let Err(e) =
            Store::add_run_usage(&mut tx, self.run, usage.input_tokens, usage.output_tokens).await
        {
            tracing::error!(error = %e, "無法記錄模型用量");
        }
        if let Some(goal) = self.goal
            && let Err(e) = tasks::add_spent(&mut tx, goal, total).await
        {
            tracing::error!(error = %e, "無法累計 Goal 用量");
        }
        if let Some(task) = self.task
            && let Err(e) = tasks::add_task_spent(&mut tx, task, total).await
        {
            tracing::error!(error = %e, "無法累計任務用量");
        }
        let seq = self.traced.fetch_add(1, Ordering::SeqCst);
        let mut detail = json!({
            "turn": turn,
            "input_tokens": usage.input_tokens,
            "output_tokens": usage.output_tokens,
            "seq": seq,
        });
        if let Some(reasoning) = self.reasoning {
            detail["thinking"] = json!(reasoning);
        }
        if let Err(e) = Store::append_trace(
            &mut tx,
            self.run,
            "model",
            &self.model,
            &detail,
            true,
            total,
            now,
        )
        .await
        {
            tracing::error!(error = %e, "無法記錄模型 trace");
        }
        if let Some(text) = narration {
            let seq = self.traced.fetch_add(1, Ordering::SeqCst);
            if let Err(e) = Store::append_trace(
                &mut tx,
                self.run,
                "text",
                &self.model,
                &json!({
                    "turn": turn,
                    "text": redact_text(text, TRACE_TEXT_CHARS),
                    "seq": seq,
                }),
                true,
                0,
                now,
            )
            .await
            {
                tracing::error!(error = %e, "無法記錄模型的文字 trace");
            }
        }
        if let Err(e) = tx.commit().await {
            tracing::error!(error = %e, "無法寫入模型用量");
        }
        // 這一輪剛好把預算用完（最後一輪不會再有 `before_turn`）：一樣記下停止原因，
        // 完成路徑才會申請追加。
        if let Some(goal) = self.goal
            && let Ok(Some((Some(budget), used, _))) = self.store.goal_budget(goal).await
            && used >= budget
        {
            self.exhausted.store(true, Ordering::SeqCst);
            self.note_stop(TurnStop::Budget).await;
        }
    }

    /// 一次工具呼叫的 trace；參數與結果遮蔽、截短後才寫入。
    pub async fn record_tool(&self, call: &ToolCall, output: &str, ok: bool) {
        let now = self.clock.now();
        let mut tx = match self.store.begin().await {
            Ok(tx) => tx,
            Err(e) => {
                tracing::error!(error = %e, "無法記錄工具 trace");
                return;
            }
        };
        let seq = self.traced.fetch_add(1, Ordering::SeqCst);
        if let Err(e) = Store::append_trace(
            &mut tx,
            self.run,
            "tool",
            &call.name,
            &json!({
                "call_id": call.call_id,
                "arguments": redact_text(&call.arguments, TRACE_ARGS_CHARS),
                "output": redact_text(output, TRACE_OUTPUT_CHARS),
                "seq": seq,
            }),
            ok,
            0,
            now,
        )
        .await
        {
            tracing::error!(error = %e, "無法記錄工具 trace");
        }
        if let Err(e) = tx.commit().await {
            tracing::error!(error = %e, "無法寫入工具 trace");
        }
    }

    /// 記下這個 run 為什麼停下，完成路徑據此改成申請預算或維持暫停。
    pub async fn note_stop(&self, stop: TurnStop) {
        let mut tx = match self.store.begin().await {
            Ok(tx) => tx,
            Err(e) => {
                tracing::error!(error = %e, "無法記錄停止原因");
                return;
            }
        };
        if let Err(e) = Store::set_run_stop_reason(&mut tx, self.run, stop.stop_reason()).await {
            tracing::error!(error = %e, "無法記錄停止原因");
        }
        if let Err(e) = tx.commit().await {
            tracing::error!(error = %e, "無法寫入停止原因");
        }
    }
}

fn truncate(text: &str, max: usize) -> String {
    let mut out = String::new();
    for (count, ch) in text.chars().enumerate() {
        if count >= max {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

/// trace 的文字先遮蔽再截短。JSON 會逐欄處理；其他文字用 `key=value` 與
/// `Bearer …` 的形式遮蔽，處理不了的部分照原樣截短（至少不會多留）。
fn redact_text(text: &str, max: usize) -> String {
    // 很大的輸出先截短再遮蔽（解析整份 JSON 沒有意義，也吃記憶體）。
    let text = if text.len() > max.saturating_mul(4) {
        truncate(text, max.saturating_mul(4))
    } else {
        text.to_string()
    };
    if let Ok(mut value) = serde_json::from_str::<Value>(&text) {
        redact(&mut value);
        return truncate(&value.to_string(), max);
    }
    let mut out = String::new();
    for (i, line) in text.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&redact_line(line));
    }
    truncate(&out, max)
}

/// 一行文字裡的 `key=value`、`key: value` 與 `Bearer …`（trace 只給人看，不保留原空白）。
fn redact_line(line: &str) -> String {
    let words: Vec<&str> = line.split_whitespace().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < words.len() {
        let word = words[i];
        if word.eq_ignore_ascii_case("bearer") {
            out.push_str("Bearer （已遮蔽）");
            i = words.len();
        } else if let Some((head, _)) = word.split_once('=').or_else(|| word.split_once(':'))
            && sensitive(head)
        {
            // 值可能含空白（`password=my secret`）：後面整段都不留。
            out.push_str(&format!("{head}=（已遮蔽）"));
            i = words.len();
        } else if looks_like_secret(word) {
            out.push_str("（已遮蔽）");
            i += 1;
        } else {
            out.push_str(word);
            i += 1;
        }
        if i < words.len() {
            out.push(' ');
        }
    }
    out
}

/// 常見的憑證前綴：即使沒有欄位名稱也遮蔽。
fn looks_like_secret(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    lower.starts_with("sk-")
        || lower.starts_with("ghp_")
        || lower.starts_with("github_pat_")
        || lower.starts_with("xox")
        || lower.starts_with("akia")
}

fn sensitive(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    lower.contains("key")
        || lower.contains("token")
        || lower.contains("secret")
        || lower.contains("password")
        || lower.contains("authorization")
        || lower.contains("cookie")
}

fn redact(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                let lower = key.to_ascii_lowercase();
                if lower.contains("key")
                    || lower.contains("token")
                    || lower.contains("secret")
                    || lower.contains("password")
                    || lower.contains("authorization")
                    || lower.contains("cookie")
                {
                    *v = Value::String("（已遮蔽）".into());
                } else {
                    redact(v);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact),
        _ => {}
    }
}

/// 熔斷被拒絕或逾時：維持暫停，一小時後再問一次（不要讓 project 永遠卡住）。
pub async fn cooldown_breaker(
    conn: &mut sqlx::SqliteConnection,
    project: ProjectId,
    now: Timestamp,
) -> Result<(), Error> {
    sqlx::query("UPDATE projects SET breaker_until = ? WHERE id = ? AND breaker_until IS NOT NULL")
        .bind(now.0 + 3600)
        .bind(project.to_string())
        .execute(conn)
        .await?;
    Ok(())
}

/// 冷卻結束、還沒有待決核准的熔斷專案再建一筆 `ResumeProject`（sweeper 每分鐘呼叫）。
pub async fn rearm_breakers(txn: &mut crate::hub::LogTxn<'_>, now: Timestamp) -> Result<(), Error> {
    let rows = sqlx::query(
        "SELECT id FROM projects WHERE breaker_until IS NOT NULL AND breaker_until <= ?",
    )
    .bind(now.0)
    .fetch_all(txn.conn())
    .await?;
    for row in rows {
        use sqlx::Row;
        let project: ProjectId = crate::store::parse_id(row.try_get("id")?)?;
        let pending: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM approvals WHERE project_id = ? AND state = 'pending' \
             AND action LIKE '%resume_project%'",
        )
        .bind(project.to_string())
        .fetch_one(txn.conn())
        .await?;
        // 更新時間，下一分鐘不要重複問。
        sqlx::query("UPDATE projects SET breaker_until = ? WHERE id = ?")
            .bind(now.0 + 3600)
            .bind(project.to_string())
            .execute(txn.conn())
            .await?;
        if pending > 0 {
            continue;
        }
        approvals::insert(
            txn,
            NewApproval {
                project,
                goal: None,
                task: None,
                action: Action::ResumeProject { project },
                title: "專案預算熔斷".into(),
                reason: "這個 project 的用量仍然超過一小時上限，已暫停所有 run。\
                         核准後恢復；拒絕或逾時則維持暫停。"
                    .into(),
                by: Principal::agent("budget"),
                notify: true,
            },
            now,
        )
        .await?;
    }
    Ok(())
}

/// 幫 Goal 申請追加預算；已經有一筆等決定、或你拒絕過就不再申請。
/// 必須在 `finish` 的 transaction commit 之後呼叫（它自己開 transaction）。
pub async fn request_increase(
    store: &Store,
    hub: &Arc<Hub>,
    clock: &Clock,
    project: ProjectId,
    goal: GoalId,
) -> Result<bool, Error> {
    // 不限預算的 Goal 不會用完，也就不用申請。
    let Some((Some(budget), _, denied)) = store.goal_budget(goal).await? else {
        return Ok(false);
    };
    if denied {
        return Ok(false);
    }
    let now = clock.now();
    let mut txn = hub.begin().await?;
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM approvals WHERE goal_id = ? AND state = 'pending' \
         AND action LIKE '%increase_budget%'",
    )
    .bind(goal.to_string())
    .fetch_one(txn.conn())
    .await?;
    if pending > 0 {
        return Ok(false);
    }
    let tokens = (budget / 2).max(MIN_BUDGET_STEP_TOKENS);
    approvals::insert(
        &mut txn,
        NewApproval {
            project,
            goal: Some(goal),
            task: None,
            action: Action::IncreaseBudget { goal, tokens },
            title: "Goal 預算用完".into(),
            reason: format!(
                "這個 Goal 的預算（{budget} tokens）用完了。核准後追加 {tokens} tokens 並繼續；\
                 拒絕或逾時就不再自動追加。"
            ),
            by: Principal::agent("budget"),
            notify: true,
        },
        now,
    )
    .await?;
    txn.commit().await?;
    Ok(true)
}

/// 建立一個 run（chat）的預算；沒有 Goal 時只記用量，不限制。
#[allow(clippy::too_many_arguments)]
pub async fn for_run(
    store: &Store,
    hub: &Arc<Hub>,
    clock: &Clock,
    run: Uuid,
    project: ProjectId,
    goal: Option<GoalId>,
    task: Option<TaskId>,
    meta: RunMeta,
) -> Result<Arc<Budget>, Error> {
    Budget::new(
        store.clone(),
        hub.clone(),
        clock.clone(),
        run,
        project,
        goal,
        task,
        meta,
    )
    .await
}

/// 記錄一筆「你不在期間」摘要需要的里程碑數量（給 hub 的快照與 MarkRead 用）。
pub async fn milestones_since(
    conn: &mut sqlx::SqliteConnection,
    project: ProjectId,
    since: Timestamp,
) -> Result<Milestones, Error> {
    let goals: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM goals WHERE project_id = ? AND created_at >= ?")
            .bind(project.to_string())
            .bind(since.0)
            .fetch_one(&mut *conn)
            .await?;
    let done: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM goals WHERE project_id = ? AND state = 'done' AND updated_at >= ?",
    )
    .bind(project.to_string())
    .bind(since.0)
    .fetch_one(&mut *conn)
    .await?;
    let waiting: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM tasks t JOIN goals g ON g.id = t.goal_id \
         WHERE g.project_id = ? AND t.updated_at >= ? AND t.state LIKE '%\"NeedsHuman\"%'",
    )
    .bind(project.to_string())
    .bind(since.0)
    .fetch_optional(&mut *conn)
    .await?
    .unwrap_or(0);
    let approvals: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM approvals WHERE project_id = ? AND state = 'pending' AND created_at >= ?",
    )
    .bind(project.to_string())
    .bind(since.0)
    .fetch_one(&mut *conn)
    .await?;
    let tasks_done: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM tasks t JOIN goals g ON g.id = t.goal_id \
         WHERE g.project_id = ? AND t.state LIKE '%\"Done\"%' AND t.updated_at >= ?",
    )
    .bind(project.to_string())
    .bind(since.0)
    .fetch_one(&mut *conn)
    .await?;
    Ok(Milestones {
        goals,
        done,
        tasks_done,
        waiting: waiting.max(0),
        approvals,
    })
}

/// 「你不在期間」摘要的原料。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Milestones {
    pub goals: i64,
    pub done: i64,
    pub tasks_done: i64,
    pub waiting: i64,
    pub approvals: i64,
}

impl Milestones {
    pub fn any(&self) -> bool {
        self.goals > 0
            || self.done > 0
            || self.tasks_done > 0
            || self.waiting > 0
            || self.approvals > 0
    }

    /// 平台產生的摘要（不依賴模型，也不含他人寫的內容）。
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.done > 0 {
            parts.push(format!("{} 個 Goal 完成", self.done));
        }
        if self.tasks_done > 0 {
            parts.push(format!("{} 個任務完成", self.tasks_done));
        }
        if self.goals > self.done {
            parts.push(format!("{} 個新 Goal", self.goals - self.done));
        }
        if self.approvals > 0 {
            parts.push(format!("{} 筆核准等你決定", self.approvals));
        }
        if self.waiting > 0 {
            parts.push(format!("{} 個任務需要你處理", self.waiting));
        }
        if parts.is_empty() {
            "這段期間沒有新的里程碑。".into()
        } else {
            format!("你不在的期間：{}。", parts.join("、"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_text_redacts_secrets() {
        let json = r#"{"api_key":"sk-1","path":"/x","nested":{"token":"abc"}}"#;
        let out = redact_text(json, 1000);
        assert!(!out.contains("sk-1"), "{out}");
        assert!(!out.contains("abc"), "{out}");
        assert!(out.contains("/x"), "{out}");

        let env = "OPENAI_API_KEY=sk-2\nAuthorization: Bearer abcdef\nplain=ok";
        let out = redact_text(env, 1000);
        assert!(!out.contains("sk-2"), "{out}");
        assert!(!out.contains("abcdef"), "{out}");
        assert!(out.contains("plain=ok"), "{out}");
    }

    #[test]
    fn milestones_summary_counts_and_reads() {
        let m = Milestones {
            goals: 3,
            done: 1,
            tasks_done: 2,
            waiting: 1,
            approvals: 1,
        };
        assert!(m.any());
        let text = m.summary();
        assert!(text.contains("1 個 Goal 完成"), "{text}");
        assert!(text.contains("2 個任務完成"), "{text}");
        assert!(text.contains("2 個新 Goal"), "{text}");
        assert!(text.contains("1 筆核准"), "{text}");
        assert!(text.contains("1 個任務需要你處理"), "{text}");
        assert!(!Milestones::default().any());
    }
}
