//! 平台工具（`proposal/agent-collaboration.md#唯讀基礎工具`、`#profile-一覽`）。
//!
//! 每個 run 依 profile 組出工具：唯讀基礎集（`get_status`、`list_goals`、`read_artifact`）
//! 一律都有，其他只給 profile 授權的。模型拿不到的工具就呼叫不到（loop 對不認得的名稱直接
//! 回錯誤），所以 [`Gate`] 不需要再擋授權；範圍（只能動自己的 project、Planner 只能動自己的
//! Goal、只能寫 profile 允許的 artifact 種類）由各工具檢查，錯誤以文字回給模型。
//!
//! 狀態變更都在 [`LogTxn`] 裡做，和事件同一個 transaction；需要叫醒或打斷 Planner 的，
//! 在 commit 之後（`commit_then`）通知它。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use pots_llm::ToolSpec;
use pots_llm::agent::{Hook, Tool, ToolError};
use pots_proto::{
    ArtifactId, ArtifactKind, Card, GoalId, GoalState, ItemId, ProjectId, ServerEvent,
    StreamItemKind, StreamItemView, TaskId, Timestamp,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::Error;
use crate::artifacts::{self, ArtifactRow, Edit, WriteOutcome};
use crate::automation::{self, ListenerDbState, ScheduleOwner, ScheduleSpec};
use crate::config::Clock;
use crate::hub::{Hub, LogTxn};
use crate::planner::{self, Planner, PlannerTrigger};
use crate::profiles::{AgentProfile, ToolName};
use crate::runs::format_utc;
use crate::store::{ItemStatus, Store};
use crate::tasks::{self, Cause, GoalRow, Principal, TaskRow, TaskState};
use crate::watcher::{self, WaitCond};
use crate::worker::code::CodeAccess;
use crate::worker::{self, WorkCtx, Worker, WorkerTrigger};

mod manage;

/// 里程碑最多幾個字。
const MAX_MILESTONE_CHARS: usize = 500;
/// 理由、摘要最多幾個字。
const MAX_REASON_CHARS: usize = 1000;
/// 任務 summary 最多幾個字。
const MAX_SUMMARY_CHARS: usize = 2000;
/// 一次 run 最多貼幾張里程碑（避免洗版使用者的對話流）。
const MAX_MILESTONES_PER_RUN: usize = 3;
/// 一次 run 最多開幾個 Goal。
const MAX_GOALS_PER_RUN: usize = 3;
/// 一個 project 同時最多幾個進行中的 Goal。
const MAX_ACTIVE_GOALS: usize = 20;

/// 工具作用的範圍。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// 對話 agent：整個 project。
    Project,
    /// Planner：project 裡的一個 Goal。
    Goal(GoalId),
    /// worker：Goal 裡的一個任務。
    Task { goal: GoalId, task: TaskId },
    /// 總管：整個實例（所有 project）。
    Global,
}

impl Scope {
    /// 這個範圍所屬的 Goal（對話 agent 是 `None`）。
    pub fn goal(self) -> Option<GoalId> {
        match self {
            Self::Project | Self::Global => None,
            Self::Goal(g) | Self::Task { goal: g, .. } => Some(g),
        }
    }

    pub fn task(self) -> Option<TaskId> {
        match self {
            Self::Task { task, .. } => Some(task),
            _ => None,
        }
    }
}

/// 一個 run 的工具共用的 context。
pub struct ToolCtx {
    pub hub: Arc<Hub>,
    pub store: Store,
    pub planner: Planner,
    pub project: ProjectId,
    pub scope: Scope,
    pub actor: Principal,
    /// `artifact_reads` 的讀者，例如 `chat:<project>`。
    pub reader: String,
    /// `write_artifact` 可以寫的種類。
    pub artifact_kinds: Vec<ArtifactKind>,
    pub run_id: Uuid,
    /// 工具產生、要附在這次回覆下面的卡片（對話 agent 用）。
    pub cards: Arc<Mutex<Vec<Card>>>,
    /// 有沒有使用者訊息在排隊（對話 agent 的 `get_status` 用）。
    pub queued: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    /// 這個 run 該結束了（例如 `complete_goal` 成功）；[`Gate`] 在下一輪開始前停下。
    pub finished: Arc<AtomicBool>,
    /// 這個 run 的預算、用量與 trace。
    pub budget: Arc<crate::budget::Budget>,
    /// 這個 run 已經貼了幾張里程碑、開了幾個 Goal。
    pub milestones: AtomicUsize,
    pub goals_created: AtomicUsize,
    /// 排程、等待與 listener 用的時鐘。
    pub clock: Clock,
    pub worker: Worker,
    /// worker 的工作位置（`open_pr`、`edit_file`、`run` 用）。
    pub work: Option<Arc<WorkCtx>>,
    /// 看得到的程式碼；有的話自動加上程式碼的唯讀工具。
    pub code: Option<Arc<dyn CodeAccess>>,
    /// `finished` 變成 `true` 時通知（Claude Code adapter 據此中斷 CLI）。
    pub stop: Arc<Notify>,
    /// 這個 run 的取消（subagent 跟著停）。
    pub cancel: CancellationToken,
}

impl ToolCtx {
    /// 這個 run 的 [`Gate`]。
    pub fn gate(&self) -> Gate {
        Gate {
            finished: self.finished.clone(),
            budget: self.budget.clone(),
        }
    }

    /// 這個 run 該結束了（結束類的工具成功）。
    pub fn finish(&self) {
        self.finished.store(true, Ordering::SeqCst);
        self.stop.notify_waiters();
        self.stop.notify_one();
    }
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    GetStatus,
    GetSourceStatus,
    ListGoals,
    ReadArtifact,
    Granted(ToolName),
}

struct PlatformTool {
    kind: Kind,
    ctx: Arc<ToolCtx>,
}

/// 依 profile 組出這個 run 的工具。
pub fn build(profile: &AgentProfile, ctx: Arc<ToolCtx>) -> Vec<Arc<dyn Tool>> {
    let mut out: Vec<Arc<dyn Tool>> = Vec::new();
    for kind in [
        Kind::GetStatus,
        Kind::GetSourceStatus,
        Kind::ListGoals,
        Kind::ReadArtifact,
    ] {
        out.push(Arc::new(PlatformTool {
            kind,
            ctx: ctx.clone(),
        }));
    }
    if ctx.code.is_some() {
        out.extend(worker::tools::code_tools(ctx.clone()));
    }
    for t in &profile.tools {
        match worker::tools::granted(*t, ctx.clone()) {
            Some(tool) => out.push(tool),
            None => out.push(Arc::new(PlatformTool {
                kind: Kind::Granted(*t),
                ctx: ctx.clone(),
            })),
        }
    }
    out
}

/// 平台的 gate：每次工具呼叫前後把關（`proposal/agent-collaboration.md#一個-loop`）。
///
/// 授權由「只把 profile 的工具交給模型」保證。這裡處理「成功後要結束這個 run」的工具
/// （`complete_goal`）：工具成功時設 `finished`，下一輪開始前停下。不在 `before_tool` 回
/// `AllowThenStop`，否則參數錯誤時模型也沒機會重試。預算、熔斷與 trace 在 [`crate::budget`]。
pub struct Gate {
    finished: Arc<AtomicBool>,
    budget: Arc<crate::budget::Budget>,
}

#[async_trait::async_trait]
impl Hook for Gate {
    async fn before_turn(&self, _turn: usize) -> bool {
        if self.finished.load(Ordering::SeqCst) {
            return false;
        }
        // 預算用完或熔斷：乾淨停下，完成路徑依 stop_reason 改成申請或維持暫停。
        self.budget.check_turn().await.is_none()
    }

    async fn after_turn(
        &self,
        turn: usize,
        usage: pots_llm::provider::Usage,
        items: &[pots_llm::provider::Item],
    ) {
        self.budget
            .record_turn(turn, usage, narration(items).as_deref())
            .await;
    }

    async fn after_tool(&self, call: &pots_llm::provider::ToolCall, output: &str) {
        // 工具結果的字串開頭是「錯誤：」時視為失敗（`pots_llm` 的介面只有文字）。
        let ok = !output.starts_with("錯誤：");
        self.budget.record_tool(call, output, ok).await;
    }
}

/// 一輪裡接著工具呼叫的文字（「我先查一下……」）。沒有工具呼叫的那一輪是最後的回答，不算。
pub(crate) fn narration(items: &[pots_llm::provider::Item]) -> Option<String> {
    use pots_llm::provider::Item;
    if !items.iter().any(|i| matches!(i, Item::ToolCall(_))) {
        return None;
    }
    let text = items
        .iter()
        .filter_map(|i| match i {
            Item::Assistant { text } if !text.trim().is_empty() => Some(text.trim()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    (!text.is_empty()).then_some(text)
}

pub(crate) fn internal(e: Error) -> ToolError {
    tracing::error!(error = %e, "工具執行失敗");
    ToolError("伺服器內部錯誤，請稍後再試".into())
}

pub(crate) fn parse<T: DeserializeOwned>(arguments: &str) -> Result<T, ToolError> {
    let text = if arguments.trim().is_empty() {
        "{}"
    } else {
        arguments
    };
    serde_json::from_str(text).map_err(|e| ToolError(format!("參數錯誤：{e}")))
}

pub(crate) fn bad(msg: impl Into<String>) -> ToolError {
    ToolError(msg.into())
}

pub(crate) fn parse_id<T: std::str::FromStr>(what: &str, s: &str) -> Result<T, ToolError> {
    s.trim()
        .parse()
        .map_err(|_| bad(format!("{what} 格式不對：{s}")))
}

pub(crate) fn object(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn edits_schema() -> Value {
    json!({
        "type": "array",
        "description": "依序套用的編輯。以 `## ` 標題區段為單位：replace_section 取代既有區段的內文，\
            append_section 在最後新增區段，replace_all 整份取代（只能在新建時用）。",
        "items": {
            "type": "object",
            "properties": {
                "op": {"type": "string", "enum": ["replace_section", "append_section", "replace_all"]},
                "heading": {"type": "string", "description": "區段標題，不含 `## `；replace_all 不用"},
                "body_md": {"type": "string", "description": "區段內文（不含標題行，不能再有 `## ` 標題）；replace_all 時是整份 Markdown"},
            },
            "required": ["op", "body_md"],
            "additionalProperties": false,
        },
    })
}

fn kind_name(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::Spec => "Spec",
        ArtifactKind::Brief => "Brief",
        ArtifactKind::Ledger => "Ledger",
        ArtifactKind::Report => "Report",
        ArtifactKind::ProjectNotes => "ProjectNotes",
    }
}

fn goal_state_name(state: &GoalState) -> &'static str {
    match state {
        GoalState::Draft => "draft",
        GoalState::Active => "active",
        GoalState::Done { .. } => "done",
        GoalState::Cancelled { .. } => "cancelled",
    }
}

fn artifact_ref(a: &Option<ArtifactRow>) -> Value {
    match a {
        Some(a) => json!({"artifact_id": a.id.to_string(), "rev": a.rev}),
        None => Value::Null,
    }
}

pub(crate) fn task_json(t: &TaskRow) -> Value {
    let detail = match &t.state {
        TaskState::Done { outcome } => Some(outcome.clone()),
        TaskState::Failed { error } => Some(error.clone()),
        TaskState::NeedsHuman { reason } => Some(reason.clone()),
        _ => None,
    };
    json!({
        "id": t.id.to_string(),
        "title": t.title,
        "summary": t.summary,
        "state": t.state.name(),
        "detail": detail,
        "parent_id": t.parent.map(|p| p.to_string()),
        "repo": t.repo,
        "branch": t.branch,
        "pr": t.pr,
        "progress": (!t.progress.is_empty()).then_some(&t.progress),
    })
}

fn conflict_json(current_rev: u32, changes: String, human: bool) -> String {
    json!({
        "conflict": {
            "current_rev": current_rev,
            "changed_by_human": human,
            "changes": changes,
            "hint": "base_rev 不是目前的版本：先吸收這些修改（必要時 read_artifact），再用 current_rev 重送。",
        }
    })
    .to_string()
}

/// 在 project 的對話流貼一張卡片。
pub async fn post_card(
    txn: &mut LogTxn<'_>,
    project: ProjectId,
    card: Card,
    run_id: Option<Uuid>,
) -> Result<(), Error> {
    let item = StreamItemView {
        id: ItemId::new(),
        at: Timestamp::now(),
        kind: StreamItemKind::Card(card),
    };
    Store::insert_item(txn.conn(), project, &item, ItemStatus::Done, run_id).await?;
    txn.push(|seq| ServerEvent::Item {
        seq,
        scope: crate::store::stream_scope(project),
        item,
    })
    .await
}

#[async_trait::async_trait]
impl Tool for PlatformTool {
    fn spec(&self) -> ToolSpec {
        let (name, description, parameters) = match self.kind {
            Kind::GetSourceStatus => (
                "get_source_status",
                "查詢外部來源（GitHub、RSS／Atom、狀態頁、HTTP 探測、通用 webhook）的健康狀態\
                 與最近一次輪詢的進度。回應慢或收不到事件時先看這裡。",
                object(json!({}), &[]),
            ),
            Kind::GetStatus => (
                "get_status",
                match self.ctx.scope {
                    Scope::Project => {
                        "查詢這個 project 的即時狀態：名稱、現在時間（UTC）、是否有使用者訊息在排隊、\
                        進行中的 Goal 與它們的任務樹、Brief／Ledger 的版本，以及 artifact 清單。\
                        關於 Goal 與任務的問題一律以這裡為準。"
                    }
                    Scope::Goal(_) => {
                        "查詢你負責的 Goal 的即時狀態：Goal 狀態、任務樹、Brief／Ledger 的版本與現在時間（UTC）。"
                    }
                    Scope::Task { .. } => {
                        "查詢你的任務與所屬 Goal 的即時狀態：任務狀態、分支、PR、Goal 的任務樹與現在時間（UTC）。"
                    }
                    Scope::Global => {
                        "查詢整個實例的狀態：所有 project 與各自的進行中 Goal、任務數、等你處理的事情與現在時間（UTC）。"
                    }
                },
                object(json!({}), &[]),
            ),
            Kind::ListGoals => (
                "list_goals",
                "列出這個 project 的 Goal（預設只有進行中的）與各自的任務數。",
                object(
                    json!({"include_closed": {"type": "boolean", "description": "也列出已完成與已取消的 Goal"}}),
                    &[],
                ),
            ),
            Kind::ReadArtifact => (
                "read_artifact",
                "讀取一份 artifact 的全文與目前版本（rev）。如果使用者在你上次讀取之後改過，會附上那些修改的 diff。\
                寫入或修改前一定要先讀。",
                object(json!({"artifact_id": {"type": "string"}}), &["artifact_id"]),
            ),
            Kind::Granted(ToolName::CreateGoal) => (
                "create_goal",
                "開一個 Goal，把討論收斂的工作交給 Planner。brief_md 是交接用的 Brief：目標、約束、\
                已決定事項與未決問題，用 `## ` 分段。Planner 不讀對話，只讀 Brief。\
                對話 agent 開的 Goal 先是草稿：使用者看過 Brief（可能直接改、或和你再討論）按「開始」才交給 Planner。",
                object(
                    json!({
                        "title": {"type": "string", "description": "簡短的 Goal 標題"},
                        "brief_md": {"type": "string", "description": "Brief 的 Markdown 全文"},
                        "budget_tokens": {"type": "integer", "description": "這個 Goal 的預算（token）；不填就不限，只記用量。指定時至少 2 萬"},
                        "project": {"type": "string", "description": "總管用：project 名稱或 ID；其他 agent 不用給"},
                    }),
                    &["title", "brief_md"],
                ),
            ),
            Kind::Granted(ToolName::SteerGoal) => (
                "steer_goal",
                "使用者改變方向時修改 Goal 的 Brief 並叫醒 Planner 重新規劃（Planner 正在跑會被中斷）。\
                edits 改 Brief 的區段，要帶 base_rev（先 read_artifact）；只想提醒 Planner 時可以只給 reason。\
                草稿 Goal 也用這個改 Brief（要帶 edits），不會叫醒 Planner。",
                object(
                    json!({
                        "goal_id": {"type": "string"},
                        "reason": {"type": "string", "description": "為什麼改方向，會轉告 Planner"},
                        "base_rev": {"type": "integer", "description": "Brief 目前的版本；有 edits 時必填"},
                        "edits": edits_schema(),
                    }),
                    &["goal_id", "reason"],
                ),
            ),
            Kind::Granted(ToolName::CancelGoal) => (
                "cancel_goal",
                "取消一個進行中或草稿的 Goal：還沒結束的任務一起取消，Planner 停止。",
                object(
                    json!({"goal_id": {"type": "string"}, "reason": {"type": "string"}}),
                    &["goal_id", "reason"],
                ),
            ),
            Kind::Granted(ToolName::ArchiveGoal) => (
                "archive_goal",
                "封存一個已結束的 Goal（已完成或已取消）：從 project 列表與快照移除，歷史與成本報表仍保留。",
                object(json!({"goal_id": {"type": "string"}}), &["goal_id"]),
            ),
            Kind::Granted(ToolName::WriteArtifact) => {
                let kinds: Vec<&str> = self
                    .ctx
                    .artifact_kinds
                    .iter()
                    .map(|k| kind_name(*k))
                    .collect();
                (
                    "write_artifact",
                    "新建或修改 artifact。修改：給 artifact_id 與 base_rev（read_artifact 拿到的 rev）；\
                    期間有人改過會回 conflict，吸收後用 current_rev 重送。新建：不給 artifact_id，給 kind 與 title，\
                    edits 用 replace_all 或 append_section。",
                    object(
                        json!({
                            "artifact_id": {"type": "string", "description": "要修改的 artifact；新建時不給"},
                            "base_rev": {"type": "integer"},
                            "kind": {"type": "string", "enum": kinds, "description": "新建時的種類"},
                            "title": {"type": "string", "description": "新建時的標題"},
                            "goal_id": {"type": "string", "description": "新建時掛在哪個 Goal；不給就掛在 project"},
                            "edits": edits_schema(),
                        }),
                        &["edits"],
                    ),
                )
            }
            Kind::Granted(ToolName::CreateTask) => (
                "create_task",
                "在你負責的 Goal 建立一個任務，馬上派給一個 worker（改 code 的任務在自己的分支上改、開 PR、等 CI；\
                評估／調查類任務寫 Report artifact，不開 PR）。summary 寫清楚要做什麼、產出是 PR 還是 Report、完成條件。",
                object(
                    json!({
                        "title": {"type": "string"},
                        "summary": {"type": "string"},
                        "parent_id": {"type": "string", "description": "上層任務（拆子任務時）"},
                        "repo": {"type": "string", "description": "在哪個 repo 做（owner/repo）；project 只監看一個 repo 時可以不給"},
                    }),
                    &["title", "summary"],
                ),
            ),
            Kind::Granted(ToolName::CancelTask) => (
                "cancel_task",
                "取消你負責的 Goal 裡一個還沒結束的任務。",
                object(
                    json!({"task_id": {"type": "string"}, "reason": {"type": "string"}}),
                    &["task_id", "reason"],
                ),
            ),
            Kind::Granted(ToolName::PostMilestone) => (
                "post_milestone",
                "在使用者的對話流貼一張里程碑卡片，回報值得讓使用者知道的進展（一句話）。",
                object(json!({"text": {"type": "string"}}), &["text"]),
            ),
            Kind::Granted(ToolName::WaitFor) => (
                "wait_for",
                "掛起任務直到條件成立（CI 結束、PR 被 review 或 merge、資源上的事件、子任務結束、指定時間），\
                逾時也會喚醒。等待期間不消耗 token；呼叫成功後這一輪就結束。",
                object(
                    json!({
                        "task_id": {"type": "string", "description": "要等待的任務；worker 不用給（就是自己的任務）"},
                        "on": {
                            "type": "object",
                            "description": "ci_finished：repo、sha（完整的 40 個字元）；change_reviewed／change_merged：change（owner/repo#42）；\
                                resource：key（change:owner/repo#42、issue:owner/repo#42 或 commit:owner/repo@<sha>）、kinds；\
                                subtasks：tasks；at：at（RFC 3339）",
                            "properties": {
                                "kind": {"type": "string", "enum": ["ci_finished", "change_reviewed", "change_merged", "resource", "subtasks", "at"]},
                                "repo": {"type": "string"},
                                "sha": {"type": "string"},
                                "change": {"type": "string"},
                                "key": {"type": "string"},
                                "kinds": {"type": "array", "items": {"type": "string"}},
                                "tasks": {"type": "array", "items": {"type": "string"}},
                                "at": {"type": "string"},
                            },
                            "required": ["kind"],
                            "additionalProperties": false,
                        },
                        "timeout_minutes": {"type": "integer", "description": "最多等多久（1 到 10080）"},
                    }),
                    match self.ctx.scope {
                        Scope::Task { .. } => &["on", "timeout_minutes"][..],
                        _ => &["task_id", "on", "timeout_minutes"][..],
                    },
                ),
            ),
            Kind::Granted(ToolName::Schedule) => (
                "schedule",
                match self.ctx.scope {
                    Scope::Project => {
                        "替自己設定時間醒來：時間到了會在這個 project 的對話裡帶著記憶繼續，prompt 是醒來時要做的事。\
                        單次用 once（at 或 in_minutes），週期用 every（五個欄位的 cron，本機時區，最短間隔 10 分鐘，預設 30 天後過期）。\
                        使用者會在對話裡看到一張卡片，可以在自動化頁停用或改時間。"
                    }
                    Scope::Goal(_) | Scope::Task { .. } => {
                        "替你負責的 Goal 設定時間醒來：時間到了 Planner 會被叫醒，prompt 是醒來時要做的事。\
                        單次用 once（at 或 in_minutes），週期用 every（五個欄位的 cron，本機時區，最短間隔 10 分鐘，預設 30 天後過期）。"
                    }
                    Scope::Global => {
                        "替總管設定時間醒來：時間到了會回到這條總管對話，prompt 是醒來時要做的事（例如跨 project 摘要）。\
                        單次用 once（at 或 in_minutes），週期用 every（五個欄位的 cron，本機時區，最短間隔 10 分鐘）。"
                    }
                },
                object(
                    json!({
                        "kind": {"type": "string", "enum": ["once", "every"]},
                        "at": {"type": "string", "description": "once：RFC 3339 時間（帶時區）"},
                        "in_minutes": {"type": "integer", "description": "once：幾分鐘後"},
                        "cron": {"type": "string", "description": "every：例如 `0 8 * * *` 是每天 08:00"},
                        "until": {"type": "string", "description": "every：到什麼時候為止（RFC 3339）"},
                        "prompt": {"type": "string"},
                    }),
                    &["kind", "prompt"],
                ),
            ),
            Kind::Granted(ToolName::CreateListener) => (
                "create_listener",
                "提案一個 listener：符合條件的外部事件（例如 issue 加上 agent label、main 的 CI 失敗）自動開 Goal 或通知對話。\
                使用者會看到一張核准卡片，核准後才生效。可能來自任何人的事件（issue 開啟、留言、review）要自動開工，\
                必須加上白名單條件（label 或 author）。",
                object(
                    json!({
                        "kinds": {"type": "array", "items": {"type": "string", "enum": pots_proto::EVENT_KINDS.iter().map(|(k, _)| *k).collect::<Vec<_>>()}},
                        "resource": {"type": "string", "description": "資源的 glob，例如 `issue:github.com/me/app#*`；不給就是 project 監看的所有 repo"},
                        "fields": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "field": {"type": "string", "enum": ["label", "author", "branch", "conclusion", "state", "repo"]},
                                    "any_of": {"type": "array", "items": {"type": "string"}},
                                },
                                "required": ["field", "any_of"],
                                "additionalProperties": false,
                            },
                        },
                        "action": {"type": "string", "enum": ["create_goal", "quick_fix", "notify_chat"]},
                        "template": {"type": "string", "description": "create_goal／quick_fix：Goal brief 的模板（事件內容會自動附上）"},
                        "max_per_hour": {"type": "integer", "description": "每小時最多觸發幾次（預設 5）"},
                    }),
                    &["kinds", "action"],
                ),
            ),
            Kind::Granted(ToolName::PauseAutomations) => (
                "pause_automations",
                "暫停排程與 listener；暫停期間錯過的觸發不補跑。可以給 until 自動恢復。",
                object(
                    json!({
                        "project_id": {"type": "string", "description": "不給就是這個 project"},
                        "until": {"type": "string", "description": "RFC 3339 時間"},
                    }),
                    &[],
                ),
            ),
            Kind::Granted(ToolName::ResumeAutomations) => (
                "resume_automations",
                "提前恢復被暫停的排程與 listener。",
                object(
                    json!({"project_id": {"type": "string", "description": "不給就是這個 project"}}),
                    &[],
                ),
            ),
            Kind::Granted(ToolName::CompleteGoal) => (
                "complete_goal",
                "Goal 的目標都已達成時呼叫：Goal 標成完成，還沒結束的任務一起取消，對話流貼一張完成卡片。\
                呼叫後這一輪就結束。",
                object(
                    json!({"summary": {"type": "string", "description": "完成了什麼"}}),
                    &["summary"],
                ),
            ),
            Kind::Granted(ToolName::RequestApproval) => (
                "request_approval",
                match self.ctx.scope {
                    Scope::Task { .. } => {
                        "申請 merge 你的任務的 PR（CI 通過後）：使用者在核准卡片決定，核准後由平台 merge（只 merge \
                        現在的 head；之後再 push 就要重新申請），任務自動完成；拒絕時你會被叫醒並看到理由。\
                        工作資料夾是本機資料夾時沒有 PR 與 CI：open_pr 之後直接申請，核准後平台把分支套用進資料夾。\
                        呼叫成功後這一輪就結束。"
                    }
                    _ => {
                        "替你負責的 Goal 裡已經結束或需要決定、有 PR 的任務申請 merge：使用者在核准卡片決定，\
                        核准後由平台 merge，任務標成完成；拒絕時那個任務的 worker 會被叫醒並看到理由。"
                    }
                },
                object(
                    json!({
                        "action": {"type": "string", "enum": ["merge_pr"], "description": "要核准的動作；目前只有 merge_pr"},
                        "task_id": {"type": "string", "description": "要 merge 的任務；worker 不用給（就是自己的任務）"},
                        "method": {"type": "string", "enum": ["squash", "merge", "rebase"], "description": "不給就用 project 的設定（預設 squash）"},
                        "reason": {"type": "string", "description": "給使用者看的說明：改了什麼、怎麼驗證的"},
                    }),
                    match self.ctx.scope {
                        Scope::Task { .. } => &["action", "reason"][..],
                        _ => &["action", "task_id", "reason"][..],
                    },
                ),
            ),
            Kind::Granted(ToolName::ProposeUserMdEdit) => (
                "propose_user_md_edit",
                "提案修改 USER.md（跨 project 的使用者偏好：commit 風格、語言、程式碼習慣、溝通方式）。\
                使用者會看到一張 diff 卡片，核准後才寫入。edits 以 `## ` 區段為單位套用在目前的 USER.md 上。",
                object(
                    json!({"edits": edits_schema(), "reason": {"type": "string", "description": "為什麼值得記住"}}),
                    &["edits", "reason"],
                ),
            ),
            Kind::Granted(ToolName::ProposeProjectNote) => (
                "propose_project_note",
                "提案修改這個 project 的 Project Notes（跨 Goal 的專案知識：怎麼跑測試、踩過的坑、慣例）。\
                使用者會看到一張 diff 卡片，核准後才寫入。edits 以 `## ` 區段為單位套用在目前的 Project Notes 上。",
                object(
                    json!({"edits": edits_schema(), "reason": {"type": "string", "description": "為什麼值得記住"}}),
                    &["edits", "reason"],
                ),
            ),
            Kind::Granted(ToolName::AskUser) => (
                "ask_user",
                match self.ctx.scope {
                    Scope::Task { .. } => {
                        "問使用者一個只有他能回答的問題（例如要用哪個環境、偏好哪種做法），使用者會在 Inbox 與對話裡\
                         看到一張問題卡片，選一個選項或自己寫答案，也可以略過。設計決定先問 Planner（post_to_team）。\
                         呼叫後任務轉成等回答、這一輪就結束；回答、略過或逾時後你會被叫醒並看到結果。"
                    }
                    Scope::Goal(_) => {
                        "問使用者一個 Brief 沒寫清楚、只有他能決定的問題，使用者會看到一張問題卡片，選一個選項或自己寫答案，\
                         也可以略過。不會結束這一輪：可以先做不依賴答案的事；回答、略過或逾時後會用新的觸發叫醒你。\
                         同一個 Goal 一次只能有一個問題在等回答。"
                    }
                    _ => {
                        "發一張問題卡片給使用者，讓他從明確的選項裡選（也可以自己寫答案或略過）。卡片附在這次回覆下面，\
                         回覆裡一句話帶過就好。使用者回答後你會收到一則「問題卡片的回答」。開放式的追問直接在回覆裡問。"
                    }
                },
                object(
                    json!({
                        "question": {"type": "string", "description": "要問的問題，寫清楚背景，讓使用者不看其他東西也能回答"},
                        "options": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "建議的答案（最多 6 個、各一行）；不給就只能自由回答",
                        },
                        "allow_other": {"type": "boolean", "description": "可以不選選項、自己寫答案（預設 true）"},
                        "reason": {"type": "string", "description": "為什麼需要使用者回答（選填，顯示在卡片上）"},
                    }),
                    &["question"],
                ),
            ),
            Kind::Granted(ToolName::SetWorkdir) => (
                "set_workdir",
                "設定這個 project 的工作資料夾（程式碼在哪、worker 在哪裡改）。使用者說「我們用這個 repo」\
                 或「在這個資料夾工作」時用，不要自己猜路徑。GitHub repo 要 owner/repo，本機要是 git repo \
                 的絕對路徑。回傳設定後的說明與限制。",
                object(
                    json!({
                        "kind": {"type": "string", "enum": ["github", "local"]},
                        "repo": {"type": "string", "description": "kind=github：owner/repo 或 github.com/owner/repo"},
                        "path": {"type": "string", "description": "kind=local：本機 git repo 的絕對路徑，例如 /Users/me/code/app"},
                    }),
                    &["kind"],
                ),
            ),
            Kind::Granted(t) => match manage::spec(t) {
                Some(spec) => spec,
                None => unreachable!("{} 由 worker::tools 提供", t.as_str()),
            },
        };
        ToolSpec {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }

    /// 在另一個 task 裡執行：run 被取消時 loop 會丟掉這個 future，但工具的寫入（`LogTxn` 的
    /// commit 與事件序號）不能停在一半，否則記憶體裡的序號會落後資料庫。
    async fn call(&self, arguments: &str) -> Result<String, ToolError> {
        let tool = PlatformTool {
            kind: self.kind,
            ctx: self.ctx.clone(),
        };
        let arguments = arguments.to_string();
        tokio::spawn(async move { tool.dispatch(&arguments).await })
            .await
            .unwrap_or_else(|e| {
                tracing::error!(error = %e, "工具的 task 異常結束");
                Err(ToolError("伺服器內部錯誤，請稍後再試".into()))
            })
    }
}

impl PlatformTool {
    /// 設定 project 的工作資料夾（對話 agent 的 `set_workdir`）。和人類的 `SetProjectWorkdir`
    /// 走同一份驗證；本機資料夾另外確認現在不在 detached HEAD。
    async fn set_workdir(&self, args: SetWorkdirArgs) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        if ctx.scope != Scope::Project {
            return Err(bad("只有 project 的對話 agent 能設定工作資料夾"));
        }
        let dir = match args.kind.as_str() {
            "github" => pots_proto::WorkDir::Github {
                repo: args.repo.unwrap_or_default(),
            },
            "local" => pots_proto::WorkDir::Local {
                path: args.path.unwrap_or_default(),
            },
            _ => return Err(bad("kind 要是 github 或 local")),
        };
        let dir = crate::workdir::normalize(dir, ctx.worker.data_dir()).map_err(bad)?;
        let mut warning = None;
        if let pots_proto::WorkDir::Local { path } = &dir {
            let status = crate::workdir::status(std::path::Path::new(path))
                .await
                .map_err(bad)?;
            if status.dirty {
                warning =
                    Some("這個資料夾現在有未 commit 的修改：核准套用前要先 commit 或 stash。");
            }
        }
        let mut txn = ctx.hub.begin().await.map_err(internal)?;
        Store::set_workdir(txn.conn(), ctx.project, Some(&dir))
            .await
            .map_err(internal)?;
        let event = dir.clone();
        txn.push(|seq| ServerEvent::ProjectWorkdir {
            seq,
            project: ctx.project,
            workdir: Some(event),
        })
        .await
        .map_err(internal)?;
        txn.commit().await.map_err(internal)?;
        let mut out = format!(
            "工作資料夾已設成 {}。{}",
            dir.label(),
            crate::workdir::describe(Some(&dir))
        );
        if let Some(warning) = warning {
            out.push(' ');
            out.push_str(warning);
        }
        Ok(out)
    }

    async fn dispatch(&self, arguments: &str) -> Result<String, ToolError> {
        match self.kind {
            Kind::GetStatus => self.get_status().await,
            Kind::GetSourceStatus => self.get_source_status().await,
            Kind::ListGoals => self.list_goals(parse(arguments)?).await,
            Kind::ReadArtifact => self.read_artifact(parse(arguments)?).await,
            Kind::Granted(ToolName::CreateGoal) => self.create_goal(parse(arguments)?).await,
            Kind::Granted(ToolName::SteerGoal) => self.steer_goal(parse(arguments)?).await,
            Kind::Granted(ToolName::CancelGoal) => self.cancel_goal(parse(arguments)?).await,
            Kind::Granted(ToolName::ArchiveGoal) => self.archive_goal(parse(arguments)?).await,
            Kind::Granted(ToolName::WriteArtifact) => self.write_artifact(parse(arguments)?).await,
            Kind::Granted(ToolName::CreateTask) => self.create_task(parse(arguments)?).await,
            Kind::Granted(ToolName::CancelTask) => self.cancel_task(parse(arguments)?).await,
            Kind::Granted(ToolName::PostMilestone) => self.post_milestone(parse(arguments)?).await,
            Kind::Granted(ToolName::CompleteGoal) => self.complete_goal(parse(arguments)?).await,
            Kind::Granted(ToolName::WaitFor) => self.wait_for(parse(arguments)?).await,
            Kind::Granted(ToolName::Schedule) => self.schedule(parse(arguments)?).await,
            Kind::Granted(ToolName::CreateListener) => {
                self.create_listener(parse(arguments)?).await
            }
            Kind::Granted(ToolName::PauseAutomations) => self.pause(parse(arguments)?).await,
            Kind::Granted(ToolName::ResumeAutomations) => self.resume(parse(arguments)?).await,
            Kind::Granted(ToolName::RequestApproval) => {
                let args: RequestApprovalArgs = parse(arguments)?;
                if args.action != "merge_pr" {
                    return Err(bad("action 目前只能是 merge_pr"));
                }
                crate::approvals::request_merge(
                    &self.ctx,
                    args.task_id.as_deref(),
                    args.method.as_deref(),
                    &args.reason,
                )
                .await
            }
            Kind::Granted(ToolName::SetWorkdir) => self.set_workdir(parse(arguments)?).await,
            Kind::Granted(ToolName::AskUser) => {
                let args: AskUserArgs = parse(arguments)?;
                crate::approvals::ask(
                    &self.ctx,
                    &args.question,
                    &args.options,
                    args.allow_other.unwrap_or(true),
                    args.reason.as_deref(),
                )
                .await
            }
            Kind::Granted(t @ (ToolName::ProposeUserMdEdit | ToolName::ProposeProjectNote)) => {
                let args: ProposeArgs = parse(arguments)?;
                crate::approvals::propose_edit(
                    &self.ctx,
                    t == ToolName::ProposeUserMdEdit,
                    &args.edits,
                    &args.reason,
                )
                .await
            }
            Kind::Granted(ToolName::ListAutomations) => {
                self.list_automations(parse(arguments)?).await
            }
            Kind::Granted(ToolName::UpdateSchedule) => {
                self.update_schedule(parse(arguments)?).await
            }
            Kind::Granted(ToolName::DeleteSchedule) => {
                self.delete_schedule(parse(arguments)?).await
            }
            Kind::Granted(ToolName::UpdateListener) => {
                self.update_listener(parse(arguments)?).await
            }
            Kind::Granted(ToolName::DeleteListener) => {
                self.delete_listener(parse(arguments)?).await
            }
            Kind::Granted(ToolName::ArtifactHistory) => {
                self.artifact_history(parse(arguments)?).await
            }
            Kind::Granted(ToolName::RestoreArtifact) => {
                self.restore_artifact(parse(arguments)?).await
            }
            Kind::Granted(ToolName::ArchiveArtifact) => {
                self.archive_artifact(parse(arguments)?).await
            }
            Kind::Granted(t) => Err(bad(format!("{} 由 worker::tools 提供", t.as_str()))),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestApprovalArgs {
    action: String,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    method: Option<String>,
    reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AskUserArgs {
    question: String,
    #[serde(default)]
    options: Vec<String>,
    #[serde(default)]
    allow_other: Option<bool>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetWorkdirArgs {
    kind: String,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    path: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposeArgs {
    edits: Vec<Edit>,
    reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListGoalsArgs {
    #[serde(default)]
    include_closed: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArtifactArgs {
    artifact_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateGoalArgs {
    title: String,
    brief_md: String,
    #[serde(default)]
    budget_tokens: Option<u64>,
    /// 總管用：在哪個 project 開 Goal（名稱或 ID）。
    #[serde(default)]
    project: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SteerGoalArgs {
    goal_id: String,
    reason: String,
    #[serde(default)]
    base_rev: Option<u32>,
    #[serde(default)]
    edits: Vec<Edit>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelGoalArgs {
    goal_id: String,
    reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveGoalArgs {
    goal_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArtifactArgs {
    #[serde(default)]
    artifact_id: Option<String>,
    #[serde(default)]
    base_rev: Option<u32>,
    #[serde(default)]
    kind: Option<ArtifactKind>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    goal_id: Option<String>,
    edits: Vec<Edit>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateTaskArgs {
    title: String,
    summary: String,
    #[serde(default)]
    parent_id: Option<String>,
    #[serde(default)]
    repo: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelTaskArgs {
    task_id: String,
    reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MilestoneArgs {
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompleteGoalArgs {
    summary: String,
}

impl PlatformTool {
    fn goal_scope(&self) -> Result<GoalId, ToolError> {
        self.ctx
            .scope
            .goal()
            .ok_or_else(|| bad("這個工具只能在 Goal 的範圍內使用"))
    }

    /// 這個 project 的 Goal；Planner 只能指定自己的 Goal。
    async fn own_goal(
        &self,
        conn: &mut sqlx::SqliteConnection,
        id: GoalId,
    ) -> Result<GoalRow, ToolError> {
        if let Some(own) = self.ctx.scope.goal()
            && own != id
        {
            return Err(bad("你只能操作自己負責的 Goal"));
        }
        match tasks::goal(conn, id).await.map_err(internal)? {
            Some(g) if self.ctx.scope == Scope::Global || g.project == self.ctx.project => Ok(g),
            _ => Err(bad(format!("這個 project 沒有 Goal {id}"))),
        }
    }

    async fn active_goal(
        &self,
        conn: &mut sqlx::SqliteConnection,
        id: GoalId,
    ) -> Result<GoalRow, ToolError> {
        let g = self.own_goal(conn, id).await?;
        if !g.state.is_active() {
            return Err(bad(format!("Goal「{}」已經{}", g.title, g.state.label())));
        }
        Ok(g)
    }

    /// 自己的、還沒結束的 Goal（草稿或進行中）。
    async fn unfinished_goal(
        &self,
        conn: &mut sqlx::SqliteConnection,
        id: GoalId,
    ) -> Result<GoalRow, ToolError> {
        let g = self.own_goal(conn, id).await?;
        if !g.state.is_open() {
            return Err(bad(format!("Goal「{}」已經{}", g.title, g.state.label())));
        }
        Ok(g)
    }

    async fn get_source_status(&self) -> Result<String, ToolError> {
        let mut tx = self.ctx.store.read().await.map_err(internal)?;
        let rows = crate::watcher::source::rows(&mut tx)
            .await
            .map_err(internal)?;
        let mut out = Vec::new();
        // GitHub 的設定不在 `source::rows`（它是特例），另外列出。
        let github = crate::watcher::github_config(&mut tx)
            .await
            .map_err(internal)?;
        out.push(json!({
            "id": "github:personal",
            "name": "GitHub",
            "kind": "GitHub",
            "url": github.api_base,
            "state": { "login": github.login },
        }));
        for row in rows {
            let health = crate::watcher::source::health(&mut tx, &row.id)
                .await
                .map_err(internal)?;
            let state = match row.kind {
                pots_proto::SourceKind::Rss => crate::watcher::feed::describe(&row, &row.cursor),
                pots_proto::SourceKind::Probe => crate::watcher::probe::describe(&row, &row.cursor),
                pots_proto::SourceKind::StatusPage => {
                    crate::watcher::statuspage::describe(&row, &row.cursor)
                }
                pots_proto::SourceKind::Webhook => json!({
                    "path": crate::watcher::source::webhook_path(&row.id),
                    "kinds": row.config.observed_kinds,
                }),
            };
            out.push(json!({
                "id": row.id,
                "name": row.name,
                "kind": row.kind.label(),
                "url": row.config.url,
                "enabled": row.enabled,
                "health": match (&health.error, health.since) {
                    (Some(message), since) => json!({
                        "state": "error",
                        "message": message,
                        "since": since.map(|t| t.0),
                    }),
                    (None, _) => json!({
                        "state": "ok",
                        "last_ok": health.last_ok.map(|t| t.0),
                    }),
                },
                "state": state,
            }));
        }
        Ok(json!({ "sources": out }).to_string())
    }

    async fn get_status(&self) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        let mut tx = ctx.store.read().await.map_err(internal)?;
        let now = format_utc(Timestamp::now());
        match ctx.scope {
            Scope::Goal(goal) | Scope::Task { goal, .. } => {
                let g = self.own_goal(&mut tx, goal).await?;
                let tasks = tasks::tasks(&mut tx, goal).await.map_err(internal)?;
                let mine = ctx
                    .scope
                    .task()
                    .and_then(|id| tasks.iter().find(|t| t.id == id))
                    .map(task_json);
                let brief = artifacts::for_goal(&mut tx, goal, ArtifactKind::Brief)
                    .await
                    .map_err(internal)?;
                let ledger = artifacts::for_goal(&mut tx, goal, ArtifactKind::Ledger)
                    .await
                    .map_err(internal)?;
                Ok(json!({
                    "now": now,
                    "your_task": mine,
                    "goal": {
                        "id": g.id.to_string(),
                        "title": g.title,
                        "state": goal_state_name(&g.state),
                        "brief": artifact_ref(&brief),
                        "ledger": artifact_ref(&ledger),
                    },
                    "tasks": tasks.iter().map(task_json).collect::<Vec<_>>(),
                })
                .to_string())
            }
            Scope::Global => {
                let projects = ctx.store.list_projects().await.map_err(internal)?;
                let mut out = Vec::new();
                for p in projects {
                    let goals = tasks::goals(&mut tx, p.id).await.map_err(internal)?;
                    let mut active = Vec::new();
                    for g in goals.iter().filter(|g| g.state.is_open()) {
                        let tasks = tasks::tasks(&mut tx, g.id).await.map_err(internal)?;
                        active.push(json!({
                            "id": g.id.to_string(),
                            "title": g.title,
                            "state": goal_state_name(&g.state),
                            "planner_note": g.planner_note,
                            "tasks": {
                                "total": tasks.len(),
                                "open": tasks.iter().filter(|t| t.state.is_open()).count(),
                                "needs_human": tasks.iter().filter(|t| matches!(t.state, TaskState::NeedsHuman { .. })).count(),
                            },
                        }));
                    }
                    out.push(json!({
                        "project": p.name,
                        "project_id": p.id.to_string(),
                        "active_goals": active,
                    }));
                }
                Ok(json!({
                    "now": now,
                    "projects": out,
                    "note": "核准留在各 project 的 Inbox；你只透過任務樹派工。",
                })
                .to_string())
            }
            Scope::Project => {
                let project = ctx
                    .store
                    .project(ctx.project)
                    .await
                    .map_err(internal)?
                    .ok_or_else(|| bad("找不到這個 project"))?;
                let messages = ctx
                    .store
                    .count_messages(ctx.project)
                    .await
                    .map_err(internal)?;
                let goals = tasks::goals(&mut tx, ctx.project).await.map_err(internal)?;
                let mut active = Vec::new();
                let mut closed = 0;
                for g in &goals {
                    if !g.state.is_open() {
                        closed += 1;
                        continue;
                    }
                    let tasks = tasks::tasks(&mut tx, g.id).await.map_err(internal)?;
                    let brief = artifacts::for_goal(&mut tx, g.id, ArtifactKind::Brief)
                        .await
                        .map_err(internal)?;
                    let ledger = artifacts::for_goal(&mut tx, g.id, ArtifactKind::Ledger)
                        .await
                        .map_err(internal)?;
                    active.push(json!({
                        "id": g.id.to_string(),
                        "title": g.title,
                        "state": goal_state_name(&g.state),
                        "planner_note": g.planner_note,
                        "brief": artifact_ref(&brief),
                        "ledger": artifact_ref(&ledger),
                        "tasks": tasks.iter().map(task_json).collect::<Vec<_>>(),
                    }));
                }
                let arts = artifacts::list(&mut tx, ctx.project)
                    .await
                    .map_err(internal)?;
                Ok(json!({
                    "project": project.name,
                    "agent": project.agent_name,
                    "created_at": format_utc(project.created_at),
                    "messages": messages,
                    "now": now,
                    "queued": ctx.queued.as_ref().is_some_and(|q| q()),
                    "active_goals": active,
                    "closed_goals": closed,
                    "artifacts": arts.iter().map(|a| json!({
                        "id": a.id.to_string(),
                        "kind": kind_name(a.kind),
                        "title": a.title,
                        "rev": a.rev,
                        "goal_id": a.goal.map(|g| g.to_string()),
                    })).collect::<Vec<_>>(),
                })
                .to_string())
            }
        }
    }

    async fn list_goals(&self, args: ListGoalsArgs) -> Result<String, ToolError> {
        let mut tx = self.ctx.store.read().await.map_err(internal)?;
        let projects = if self.ctx.scope == Scope::Global {
            self.ctx.store.list_projects().await.map_err(internal)?
        } else {
            Vec::new()
        };
        let mut goals = Vec::new();
        if projects.is_empty() {
            goals = tasks::goals(&mut tx, self.ctx.project)
                .await
                .map_err(internal)?;
        } else {
            for p in &projects {
                goals.extend(tasks::goals(&mut tx, p.id).await.map_err(internal)?);
            }
        }
        let mut out = Vec::new();
        for g in goals {
            if !args.include_closed && !g.state.is_open() {
                continue;
            }
            // Planner 與 worker 只看得到自己的 Goal。
            if let Some(own) = self.ctx.scope.goal()
                && own != g.id
            {
                continue;
            }
            let tasks = tasks::tasks(&mut tx, g.id).await.map_err(internal)?;
            let outcome = match &g.state {
                GoalState::Draft | GoalState::Active => None,
                GoalState::Done { summary } => Some(summary.clone()),
                GoalState::Cancelled { reason } => Some(reason.clone()),
            };
            let project = projects
                .iter()
                .find(|p| p.id == g.project)
                .map(|p| p.name.clone());
            out.push(json!({
                "id": g.id.to_string(),
                "title": g.title,
                "state": goal_state_name(&g.state),
                "outcome": outcome,
                "project": project,
                "created_at": format_utc(g.created_at),
                "tasks": {
                    "total": tasks.len(),
                    "open": tasks.iter().filter(|t| t.state.is_open()).count(),
                },
            }));
        }
        Ok(json!({ "goals": out }).to_string())
    }

    async fn read_artifact(&self, args: ReadArtifactArgs) -> Result<String, ToolError> {
        let id: ArtifactId = parse_id("artifact_id", &args.artifact_id)?;
        let mut tx = self.ctx.store.begin().await.map_err(internal)?;
        let a = match artifacts::get(&mut tx, id).await.map_err(internal)? {
            Some(a) if self.ctx.scope == Scope::Global || a.project == self.ctx.project => a,
            _ => return Err(bad(format!("這個 project 沒有 artifact {id}"))),
        };
        // Planner 讀得到自己 Goal 的與掛在 project 上的 artifact，讀不到別的 Goal 的。
        if let Some(own) = self.ctx.scope.goal()
            && a.goal.is_some_and(|g| g != own)
        {
            return Err(bad("你只能讀自己負責的 Goal 與 project 層級的 artifact"));
        }
        let human = artifacts::read_for_agent(&mut tx, &a, &self.ctx.reader)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(|e| internal(e.into()))?;
        let mut out = format!(
            "artifact {} · {} ·「{}」· rev {}",
            a.id,
            kind_name(a.kind),
            a.title,
            a.rev
        );
        if let Some(g) = a.goal {
            out.push_str(&format!("（掛在 Goal {g}）"));
        }
        out.push_str("\n\n");
        out.push_str(&a.body);
        if let Some(diff) = human {
            out.push_str(
                "\n---\n使用者在你上次讀取之後改了這些（請以使用者的修改為準）：\n```diff\n",
            );
            out.push_str(&diff);
            out.push_str("```\n");
        }
        Ok(out)
    }

    async fn create_goal(&self, args: CreateGoalArgs) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        let title =
            tasks::check_line("title ", &args.title, tasks::MAX_TITLE_CHARS).map_err(bad)?;
        if args.brief_md.trim().is_empty() {
            return Err(bad("brief_md 不能是空的"));
        }
        if ctx.goals_created.load(Ordering::SeqCst) >= MAX_GOALS_PER_RUN {
            return Err(bad(format!("一次回覆最多開 {MAX_GOALS_PER_RUN} 個 Goal")));
        }
        let target = match ctx.scope {
            Scope::Global => {
                let selector = args
                    .project
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| bad("總管開 Goal 要指定 project（名稱或 ID）"))?;
                ctx.store
                    .find_project(selector)
                    .await
                    .map_err(internal)?
                    .ok_or_else(|| bad(format!("找不到 project「{selector}」")))?
                    .id
            }
            _ => {
                if args.project.is_some() {
                    return Err(bad("只有總管可以替別的 project 開 Goal"));
                }
                ctx.project
            }
        };
        // 對話 agent 開的 Goal 先是草稿，等你看過 Brief 再開始；總管開的直接交給 Planner。
        let opening = if ctx.scope == Scope::Global {
            Opening::Start
        } else {
            Opening::Draft
        };
        let result: Result<Result<(GoalId, ArtifactId), String>, Error> = async {
            let mut txn = ctx.hub.begin().await?;
            let opened = open_goal_with_brief(
                &mut txn,
                target,
                &title,
                &args.brief_md,
                &ctx.actor,
                &ctx.actor.label(),
                Some(&ctx.reader),
                opening,
            )
            .await?;
            if let Ok((goal, _)) = &opened {
                if let Some(tokens) = args.budget_tokens {
                    tasks::set_goal_budget(txn.conn(), *goal, tokens).await?;
                }
                txn.commit_then(|| {
                    if opening == Opening::Start {
                        ctx.planner.kick();
                    }
                })
                .await?;
            }
            Ok(opened)
        }
        .await;
        let (goal, brief) = result.map_err(internal)?.map_err(bad)?;
        ctx.goals_created.fetch_add(1, Ordering::SeqCst);
        ctx.cards.lock().unwrap().push(Card::GoalCreated { goal });
        let note = if opening == Opening::Draft {
            "Goal 已建立成草稿：Planner 還沒開工。使用者會在對話裡的 Goal 卡片看 Brief、直接修改或和你討論，\
             按「開始」才交給 Planner。請他看一下 Brief；要依討論修改時先 read_artifact（會看到他改過的地方），\
             再用 steer_goal 帶 edits 改 Brief（草稿時不會叫醒 Planner）。你不能替他開始。"
        } else {
            "Goal 已建立，Planner 會在背景拆任務；使用者會在對話裡看到 Goal 卡片。"
        };
        Ok(json!({
            "goal_id": goal.to_string(),
            "state": if opening == Opening::Draft { "draft" } else { "active" },
            "brief": {"artifact_id": brief.to_string(), "rev": 1},
            "note": note,
        })
        .to_string())
    }

    async fn steer_goal(&self, args: SteerGoalArgs) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        let goal: GoalId = parse_id("goal_id", &args.goal_id)?;
        let reason =
            tasks::check_text("reason ", &args.reason, MAX_REASON_CHARS, true).map_err(bad)?;
        let mut txn = ctx.hub.begin().await.map_err(internal)?;
        let g = self.unfinished_goal(txn.conn(), goal).await?;
        let draft = g.state.is_draft();
        if draft && args.edits.is_empty() {
            return Err(bad(
                "這個 Goal 還是草稿，沒有 Planner 可以改方向：要改就帶 edits 改 Brief",
            ));
        }
        if tasks::goal_kind(txn.conn(), g.id).await.map_err(internal)? == "quick" {
            return Err(bad(
                "這是快速修改，沒有 Planner 可以改方向：要改就用 cancel_goal 取消它，再開一個新的",
            ));
        }
        let mut rev = None;
        if !args.edits.is_empty() {
            let base_rev = args
                .base_rev
                .ok_or_else(|| bad("有 edits 時要帶 base_rev（先 read_artifact 讀 Brief）"))?;
            let brief = artifacts::for_goal(txn.conn(), g.id, ArtifactKind::Brief)
                .await
                .map_err(internal)?
                .ok_or_else(|| bad("這個 Goal 沒有 Brief"))?;
            let outcome = artifacts::agent_write(
                txn.conn(),
                &brief,
                base_rev,
                &args.edits,
                &ctx.actor,
                &ctx.reader,
            )
            .await
            .map_err(internal)?
            .map_err(bad)?;
            match outcome {
                WriteOutcome::Written(new) => {
                    artifacts::push_changed(&mut txn, &new)
                        .await
                        .map_err(internal)?;
                    rev = Some(new.rev);
                }
                WriteOutcome::Conflict {
                    current_rev,
                    changes,
                    human,
                } => return Ok(conflict_json(current_rev, changes, human)),
            }
        }
        if draft {
            txn.commit().await.map_err(internal)?;
            return Ok(json!({
                "goal": g.id.to_string(),
                "brief_rev": rev,
                "note": "已更新草稿的 Brief；Planner 要等使用者按「開始」才開工。",
            })
            .to_string());
        }
        planner::enqueue(
            txn.conn(),
            ctx.project,
            g.id,
            &PlannerTrigger::Steer {
                by: ctx.actor.label(),
                reason,
            },
        )
        .await
        .map_err(internal)?;
        txn.commit_then(|| ctx.planner.steer(g.id))
            .await
            .map_err(internal)?;
        Ok(json!({
            "steered": g.id.to_string(),
            "brief_rev": rev,
            "note": "已叫醒 Planner 重新規劃。",
        })
        .to_string())
    }

    async fn cancel_goal(&self, args: CancelGoalArgs) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        let goal: GoalId = parse_id("goal_id", &args.goal_id)?;
        let reason =
            tasks::check_text("reason ", &args.reason, MAX_REASON_CHARS, true).map_err(bad)?;
        let mut txn = ctx.hub.begin().await.map_err(internal)?;
        let g = self.unfinished_goal(txn.conn(), goal).await?;
        let cancelled = close_goal(
            &mut txn,
            &g,
            GoalState::Cancelled { reason },
            &ctx.actor,
            "Goal 已取消",
        )
        .await
        .map_err(internal)?;
        txn.commit_then(|| {
            ctx.planner.close_goal(g.id);
            ctx.worker.close_goal(g.id);
        })
        .await
        .map_err(internal)?;
        Ok(json!({"cancelled": g.id.to_string(), "tasks_cancelled": cancelled}).to_string())
    }

    async fn archive_goal(&self, args: ArchiveGoalArgs) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        if !matches!(ctx.scope, Scope::Project | Scope::Global) {
            return Err(bad("只有對話 agent 或總管可以封存 Goal"));
        }
        let goal: GoalId = parse_id("goal_id", &args.goal_id)?;
        let mut txn = ctx.hub.begin().await.map_err(internal)?;
        let g = tasks::goal(txn.conn(), goal)
            .await
            .map_err(internal)?
            .ok_or_else(|| bad(format!("找不到 Goal {goal}")))?;
        if ctx.scope != Scope::Global && g.project != ctx.project {
            return Err(bad("只能封存這個 project 的 Goal"));
        }
        if g.state.is_open() {
            return Err(bad("只能封存已結束的 Goal；先完成或取消它"));
        }
        if g.archived_at.is_some() {
            return Ok(json!({
                "archived": goal.to_string(),
                "note": "這個 Goal 已經封存了。",
            })
            .to_string());
        }
        tasks::set_goal_archived(txn.conn(), goal, ctx.clock.now())
            .await
            .map_err(internal)?;
        txn.push(|seq| ServerEvent::GoalArchived {
            seq,
            project: g.project,
            goal,
        })
        .await
        .map_err(internal)?;
        txn.commit().await.map_err(internal)?;
        Ok(json!({"archived": goal.to_string()}).to_string())
    }

    async fn write_artifact(&self, args: WriteArtifactArgs) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        let mut txn = ctx.hub.begin().await.map_err(internal)?;
        let mut created = false;
        let written = match &args.artifact_id {
            Some(id) => {
                let id: ArtifactId = parse_id("artifact_id", id)?;
                let a = match artifacts::get(txn.conn(), id).await.map_err(internal)? {
                    Some(a) if a.project == ctx.project => a,
                    _ => return Err(bad(format!("這個 project 沒有 artifact {id}"))),
                };
                if !ctx.artifact_kinds.contains(&a.kind) {
                    return Err(bad(format!("你不能修改 {}", kind_name(a.kind))));
                }
                if let Some(own) = ctx.scope.goal()
                    && a.goal != Some(own)
                {
                    return Err(bad("你只能修改自己負責的 Goal 的 artifact"));
                }
                let base_rev = args
                    .base_rev
                    .ok_or_else(|| bad("修改時要帶 base_rev（先 read_artifact）"))?;
                let outcome = artifacts::agent_write(
                    txn.conn(),
                    &a,
                    base_rev,
                    &args.edits,
                    &ctx.actor,
                    &ctx.reader,
                )
                .await
                .map_err(internal)?
                .map_err(bad)?;
                match outcome {
                    WriteOutcome::Written(new) => new,
                    WriteOutcome::Conflict {
                        current_rev,
                        changes,
                        human,
                    } => return Ok(conflict_json(current_rev, changes, human)),
                }
            }
            None => {
                let kind = args.kind.ok_or_else(|| bad("新建時要給 kind"))?;
                if !ctx.artifact_kinds.contains(&kind) {
                    return Err(bad(format!("你不能建立 {}", kind_name(kind))));
                }
                let goal = match (ctx.scope, &args.goal_id) {
                    (Scope::Goal(own) | Scope::Task { goal: own, .. }, _) => Some(own),
                    (Scope::Project, Some(g)) => {
                        let g: GoalId = parse_id("goal_id", g)?;
                        Some(self.own_goal(txn.conn(), g).await?.id)
                    }
                    (Scope::Project, None) => None,
                    (Scope::Global, _) => {
                        return Err(bad("總管不寫 artifact；派工後由該 project 的 agent 寫"));
                    }
                };
                if kind == ArtifactKind::Ledger {
                    let Some(goal) = goal else {
                        return Err(bad("Ledger 一定要掛在 Goal 上"));
                    };
                    if let Some(existing) = artifacts::for_goal(txn.conn(), goal, kind)
                        .await
                        .map_err(internal)?
                    {
                        return Err(bad(format!(
                            "這個 Goal 已經有 Ledger（{}，rev {}），請用 artifact_id 修改",
                            existing.id, existing.rev
                        )));
                    }
                }
                let title = match &args.title {
                    Some(t) => {
                        tasks::check_line("title ", t, artifacts::MAX_TITLE_CHARS).map_err(bad)?
                    }
                    None if kind == ArtifactKind::Ledger => "Ledger".into(),
                    None => return Err(bad("新建時要給 title")),
                };
                let body = artifacts::apply_edits("", &args.edits, true).map_err(bad)?;
                let row = ArtifactRow {
                    id: ArtifactId::new(),
                    project: ctx.project,
                    goal,
                    kind,
                    title,
                    rev: 1,
                    body,
                    human_edits: Vec::new(),
                };
                artifacts::insert(txn.conn(), &row, &ctx.actor)
                    .await
                    .map_err(internal)?;
                artifacts::mark_read(txn.conn(), row.id, &ctx.reader, row.rev)
                    .await
                    .map_err(internal)?;
                created = true;
                row
            }
        };
        artifacts::push_changed(&mut txn, &written)
            .await
            .map_err(internal)?;
        txn.commit().await.map_err(internal)?;
        // commit 成功後才附卡片。
        if created && ctx.scope == Scope::Project {
            ctx.cards.lock().unwrap().push(Card::Artifact {
                id: written.id,
                kind: written.kind,
                title: written.title.clone(),
                note: format!("{} 已建立 · v1", kind_name(written.kind)),
            });
        }
        Ok(
            json!({"written": {"artifact_id": written.id.to_string(), "rev": written.rev}})
                .to_string(),
        )
    }

    async fn create_task(&self, args: CreateTaskArgs) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        let goal = self.goal_scope()?;
        let title =
            tasks::check_line("title ", &args.title, tasks::MAX_TITLE_CHARS).map_err(bad)?;
        let summary =
            tasks::check_text("summary ", &args.summary, MAX_SUMMARY_CHARS, false).map_err(bad)?;
        let mut txn = ctx.hub.begin().await.map_err(internal)?;
        let g = self.active_goal(txn.conn(), goal).await?;
        let existing = tasks::tasks(txn.conn(), g.id).await.map_err(internal)?;
        let open = existing.iter().filter(|t| t.state.is_open()).count();
        if open >= tasks::MAX_OPEN_TASKS_PER_GOAL {
            return Err(bad(format!(
                "一個 Goal 同時最多 {} 個未結束的任務",
                tasks::MAX_OPEN_TASKS_PER_GOAL
            )));
        }
        if existing.len() >= tasks::MAX_TASKS_PER_GOAL {
            return Err(bad(format!(
                "一個 Goal 最多 {} 個任務（含已取消的）",
                tasks::MAX_TASKS_PER_GOAL
            )));
        }
        let parent = match &args.parent_id {
            Some(p) => {
                let p: TaskId = parse_id("parent_id", p)?;
                if !existing.iter().any(|t| t.id == p) {
                    return Err(bad(format!("這個 Goal 沒有任務 {p}")));
                }
                Some(p)
            }
            None => None,
        };
        // project 還沒有工作資料夾：任務只記在任務樹上，不派給 worker（沒有指定 repo 時不算錯）。
        let picked = worker::pick_workdir(txn.conn(), ctx.project, args.repo.as_deref())
            .await
            .map_err(internal)?;
        let mut no_workdir = None;
        let repo = match picked {
            Ok(key) => Some(key),
            Err(e) if args.repo.as_deref().is_none_or(|r| r.trim().is_empty()) => {
                no_workdir = Some(e);
                None
            }
            Err(e) => return Err(bad(e)),
        };
        let mut task = TaskRow::new(g.id, title, summary);
        task.parent = parent;
        task.repo = repo.clone();
        if repo.is_some() {
            task.assignee = "worker".into();
        }
        let cause = Cause {
            by: ctx.actor.clone(),
            reason: String::new(),
        };
        let result: Result<(), Error> = async {
            tasks::insert_task(txn.conn(), &task, &cause).await?;
            if repo.is_some() {
                worker::enqueue(
                    txn.conn(),
                    ctx.project,
                    g.id,
                    task.id,
                    &WorkerTrigger::Assigned,
                )
                .await?;
            }
            tasks::push_goal(&mut txn, g.id).await?;
            txn.commit_then(|| ctx.worker.kick()).await
        }
        .await;
        result.map_err(internal)?;
        let note = if repo.is_some() {
            "已派給 worker"
        } else {
            no_workdir
                .as_deref()
                .unwrap_or("這個 project 還沒有工作資料夾")
        };
        Ok(json!({"task_id": task.id.to_string(), "repo": repo, "note": note}).to_string())
    }

    async fn cancel_task(&self, args: CancelTaskArgs) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        let goal = self.goal_scope()?;
        let id: TaskId = parse_id("task_id", &args.task_id)?;
        let reason =
            tasks::check_text("reason ", &args.reason, MAX_REASON_CHARS, true).map_err(bad)?;
        let mut txn = ctx.hub.begin().await.map_err(internal)?;
        let task = match tasks::task(txn.conn(), id).await.map_err(internal)? {
            Some(t) if t.goal == goal => t,
            _ => return Err(bad(format!("你負責的 Goal 沒有任務 {id}"))),
        };
        if !task.state.is_open() {
            return Err(bad(format!("任務「{}」已經結束", task.title)));
        }
        let result: Result<(), Error> = async {
            let cause = Cause {
                by: ctx.actor.clone(),
                reason,
            };
            let to = TaskState::Cancelled {
                by: ctx.actor.clone(),
            };
            tasks::set_task_state(txn.conn(), &task, &to, &cause).await?;
            tasks::push_goal(&mut txn, goal).await?;
            txn.commit_then(|| ctx.worker.interrupt(id)).await
        }
        .await;
        result.map_err(internal)?;
        Ok(json!({"cancelled": id.to_string()}).to_string())
    }

    async fn post_milestone(&self, args: MilestoneArgs) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        let goal = self.goal_scope()?;
        let text =
            tasks::check_text("text ", &args.text, MAX_MILESTONE_CHARS, true).map_err(bad)?;
        if ctx.milestones.load(Ordering::SeqCst) >= MAX_MILESTONES_PER_RUN {
            return Err(bad(format!(
                "一輪最多貼 {MAX_MILESTONES_PER_RUN} 張里程碑；把進展合併成一句"
            )));
        }
        let mut txn = ctx.hub.begin().await.map_err(internal)?;
        self.active_goal(txn.conn(), goal).await?;
        let result: Result<(), Error> = async {
            post_card(
                &mut txn,
                ctx.project,
                Card::Milestone { goal, text },
                Some(ctx.run_id),
            )
            .await?;
            txn.commit().await
        }
        .await;
        result.map_err(internal)?;
        ctx.milestones.fetch_add(1, Ordering::SeqCst);
        Ok("已貼到使用者的對話流".into())
    }

    async fn complete_goal(&self, args: CompleteGoalArgs) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        let goal = self.goal_scope()?;
        let summary =
            tasks::check_text("summary ", &args.summary, MAX_REASON_CHARS, true).map_err(bad)?;
        let mut txn = ctx.hub.begin().await.map_err(internal)?;
        let g = self.active_goal(txn.conn(), goal).await?;
        let result: Result<(), Error> = async {
            close_goal(
                &mut txn,
                &g,
                GoalState::Done {
                    summary: summary.clone(),
                },
                &ctx.actor,
                "Goal 已完成",
            )
            .await?;
            post_card(
                &mut txn,
                ctx.project,
                Card::Milestone {
                    goal,
                    text: format!("Goal 完成：{summary}"),
                },
                Some(ctx.run_id),
            )
            .await?;
            txn.commit().await
        }
        .await;
        result.map_err(internal)?;
        ctx.worker.close_goal(goal);
        // Goal 結束了，這個 run 不該再做事：Gate 在下一輪開始前停下。
        ctx.finish();
        Ok("Goal 已標成完成，這一輪到此結束".into())
    }
}

/// 結束一個 Goal：改狀態、取消還沒結束的任務、清掉排隊中的 Planner run、記 `GoalChanged`。
/// 正在跑的 Planner 由呼叫端在 commit 之後中斷（[`Planner::close_goal`]）。回傳取消了幾個任務。
pub async fn close_goal(
    txn: &mut LogTxn<'_>,
    g: &GoalRow,
    state: GoalState,
    by: &Principal,
    reason: &str,
) -> Result<usize, Error> {
    tasks::set_goal_state(txn.conn(), g.id, &state).await?;
    let cancelled = tasks::cancel_open_tasks(
        txn.conn(),
        g.id,
        &Cause {
            by: by.clone(),
            reason: reason.into(),
        },
    )
    .await?;
    planner::drop_queued(txn.conn(), g.id).await?;
    // 排隊中的 worker run 也不用跑了（正在跑的由呼叫端在 commit 之後中斷：`Worker::close_goal`）。
    sqlx::query(
        "UPDATE agent_runs SET state = 'cancelled', finished_at = ? \
         WHERE agent = 'worker' AND goal_id = ? AND state = 'queued'",
    )
    .bind(Timestamp::now().0)
    .bind(g.id.to_string())
    .execute(txn.conn())
    .await?;
    if automation::finish_goal_schedules(txn.conn(), g.id, Timestamp::now()).await? > 0 {
        automation::push(txn, Timestamp::now()).await?;
    }
    tasks::push_goal(txn, g.id).await?;
    Ok(cancelled)
}

/// 草稿 Goal 在狀態面板上的說明。
pub const DRAFT_NOTE: &str = "草稿：看過 Brief 後按「開始」交給 Planner";

/// 開始一個草稿 Goal：改成進行中、排 Planner（拿到的是當下的 Brief）、記 `GoalChanged`。
/// 呼叫端在 commit 之後叫醒 Planner（[`Planner::kick`]）。
pub async fn start_goal(txn: &mut LogTxn<'_>, g: &GoalRow) -> Result<(), Error> {
    tasks::set_goal_state(txn.conn(), g.id, &GoalState::Active).await?;
    tasks::set_planner_note(txn.conn(), g.id, "等待 Planner…").await?;
    planner::enqueue(txn.conn(), g.project, g.id, &PlannerTrigger::Started).await?;
    tasks::push_goal(txn, g.id).await
}

/// 開一個 Goal：寫入 Goal 與 Brief、排 Planner、記 `ArtifactChanged`。呼叫端在 commit 之後
/// 叫醒 Planner（[`Planner::kick`]）。進行中的 Goal 太多或內容不合法時回說明。
/// `by_label` 是告訴 Planner 誰開的，`reader` 是已經讀過這份 Brief 的 agent。
pub async fn open_goal(
    txn: &mut LogTxn<'_>,
    project: ProjectId,
    title: &str,
    brief_md: &str,
    by: &Principal,
    by_label: &str,
    reader: Option<&str>,
) -> Result<Result<GoalId, String>, Error> {
    Ok(open_goal_with_brief(
        txn,
        project,
        title,
        brief_md,
        by,
        by_label,
        reader,
        Opening::Start,
    )
    .await?
    .map(|(g, _)| g))
}

/// 開 Goal 之後怎麼走。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Opening {
    /// 直接交給 Planner（listener、排程、總管）。
    Start,
    /// 草稿：不排 Planner，等你看過 Brief 按「開始」（對話 agent 開的 Goal）。
    Draft,
    /// 快速修改：種類是 `quick`，不排 Planner（呼叫端自己建任務）。
    Quick,
}

/// 開 Goal 與 Brief，見 [`Opening`]。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn open_goal_with_brief(
    txn: &mut LogTxn<'_>,
    project: ProjectId,
    title: &str,
    brief_md: &str,
    by: &Principal,
    by_label: &str,
    reader: Option<&str>,
    opening: Opening,
) -> Result<Result<(GoalId, ArtifactId), String>, Error> {
    let title = match tasks::check_line("title ", title, tasks::MAX_TITLE_CHARS) {
        Ok(t) => t,
        Err(e) => return Ok(Err(e)),
    };
    if brief_md.trim().is_empty() {
        return Ok(Err("brief_md 不能是空的".into()));
    }
    let body = if brief_md.trim_start().starts_with("# ") {
        brief_md.to_string()
    } else {
        format!("# {title}\n\n{}", brief_md.trim())
    };
    let body = match artifacts::apply_edits("", &[Edit::ReplaceAll { body_md: body }], true) {
        Ok(b) => b,
        Err(e) => return Ok(Err(e)),
    };
    let active = tasks::goals(txn.conn(), project)
        .await?
        .iter()
        .filter(|g| g.state.is_open())
        .count();
    if active >= MAX_ACTIVE_GOALS {
        return Ok(Err(format!(
            "這個 project 已經有 {active} 個進行中或草稿的 Goal，先完成、取消或捨棄一些"
        )));
    }
    let draft = opening == Opening::Draft;
    let goal = GoalRow {
        id: GoalId::new(),
        project,
        title: title.clone(),
        state: if draft {
            GoalState::Draft
        } else {
            GoalState::Active
        },
        planner_note: if draft {
            DRAFT_NOTE.into()
        } else {
            "等待 Planner…".into()
        },
        created_at: Timestamp::now(),
        archived_at: None,
    };
    let brief = ArtifactRow {
        id: ArtifactId::new(),
        project,
        goal: Some(goal.id),
        kind: ArtifactKind::Brief,
        title,
        rev: 1,
        body,
        human_edits: Vec::new(),
    };
    tasks::insert_goal(txn.conn(), &goal, by).await?;
    artifacts::insert(txn.conn(), &brief, by).await?;
    if let Some(reader) = reader {
        artifacts::mark_read(txn.conn(), brief.id, reader, brief.rev).await?;
    }
    match opening {
        Opening::Quick => tasks::set_goal_kind(txn.conn(), goal.id, "quick").await?,
        Opening::Draft => {}
        Opening::Start => {
            planner::enqueue(
                txn.conn(),
                project,
                goal.id,
                &PlannerTrigger::GoalCreated {
                    by: by_label.to_string(),
                },
            )
            .await?
        }
    }
    artifacts::push_changed(txn, &brief).await?;
    Ok(Ok((goal.id, brief.id)))
}

/// RFC 3339 時間。
fn parse_time_arg(what: &str, s: &str) -> Result<Timestamp, ToolError> {
    chrono::DateTime::parse_from_rfc3339(s.trim())
        .map(|t| Timestamp(t.timestamp()))
        .map_err(|_| {
            bad(format!(
                "{what} 要是 RFC 3339 時間（例如 2026-11-08T09:00:00+08:00）：{s}"
            ))
        })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitOnArgs {
    kind: String,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    sha: Option<String>,
    #[serde(default)]
    change: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    kinds: Vec<String>,
    #[serde(default)]
    tasks: Vec<String>,
    #[serde(default)]
    at: Option<String>,
}

impl WaitOnArgs {
    fn into_cond(self) -> Result<WaitCond, ToolError> {
        let need = |v: Option<String>, name: &str| {
            v.filter(|s| !s.trim().is_empty())
                .ok_or_else(|| bad(format!("on 要給 {name}")))
        };
        let cond = match self.kind.as_str() {
            "ci_finished" => WaitCond::CiFinished {
                repo: need(self.repo, "repo")?,
                sha: need(self.sha, "sha")?,
            },
            "change_reviewed" => WaitCond::ChangeReviewed {
                change: need(self.change, "change")?,
            },
            "change_merged" => WaitCond::ChangeMerged {
                change: need(self.change, "change")?,
            },
            "resource" => WaitCond::Resource {
                key: need(self.key, "key")?,
                kinds: self.kinds,
            },
            "subtasks" => WaitCond::Subtasks {
                tasks: self
                    .tasks
                    .iter()
                    .map(|t| parse_id("tasks", t))
                    .collect::<Result<_, _>>()?,
            },
            "at" => WaitCond::At {
                at: parse_time_arg("on.at", &need(self.at, "at")?)?,
            },
            k => return Err(bad(format!("不認得的等待種類：{k}"))),
        };
        cond.normalized().map_err(bad)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitForArgs {
    /// worker 可以不給（就是自己的任務）。
    #[serde(default)]
    task_id: Option<String>,
    on: WaitOnArgs,
    timeout_minutes: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScheduleArgs {
    kind: String,
    #[serde(default)]
    at: Option<String>,
    #[serde(default)]
    in_minutes: Option<i64>,
    #[serde(default)]
    cron: Option<String>,
    #[serde(default)]
    until: Option<String>,
    prompt: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListenerArgs {
    kinds: Vec<String>,
    #[serde(default)]
    resource: Option<String>,
    #[serde(default)]
    fields: Vec<pots_proto::FieldMatch>,
    action: String,
    #[serde(default)]
    template: Option<String>,
    #[serde(default)]
    max_per_hour: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PauseArgs {
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    until: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResumeArgs {
    #[serde(default)]
    project_id: Option<String>,
}

/// 排程卡片與自動化卡片。
fn notice(title: &str, body: String) -> Card {
    Card::Notice {
        title: title.into(),
        body,
        actions: vec![pots_proto::NoticeAction::ViewAutomations],
    }
}

impl PlatformTool {
    async fn wait_for(&self, args: WaitForArgs) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        // worker 的任務已經結束或在等待了；Planner 可以在同一輪讓好幾個任務等待。
        if ctx.scope.task().is_some() && ctx.finished.load(Ordering::SeqCst) {
            return Err(bad("這一輪已經結束，沒有執行"));
        }
        let id: TaskId = match (
            args.task_id.as_deref().filter(|s| !s.trim().is_empty()),
            ctx.scope.task(),
        ) {
            (Some(s), _) => parse_id("task_id", s)?,
            (None, Some(own)) => own,
            (None, None) => return Err(bad("要給 task_id")),
        };
        if !(1..=10_080).contains(&args.timeout_minutes) {
            return Err(bad("timeout_minutes 要在 1 到 10080 之間"));
        }
        let on = args.on.into_cond()?;
        let now = ctx.clock.now();
        let deadline = Timestamp(now.0 + args.timeout_minutes * 60);
        let mut txn = ctx.hub.begin().await.map_err(internal)?;
        let task = match tasks::task(txn.conn(), id).await.map_err(internal)? {
            Some(t) => t,
            None => return Err(bad(format!("沒有任務 {id}"))),
        };
        self.own_goal(txn.conn(), task.goal).await?;
        // worker 只能讓自己的任務等待（正在跑）；Planner 只能讓還沒開始的任務等待。
        let allowed = match ctx.scope.task() {
            Some(own) if own != task.id => {
                return Err(bad("你只能讓自己的任務等待"));
            }
            Some(_) => task.state == TaskState::Running,
            None => task.state == TaskState::Planned,
        };
        if !allowed {
            return Err(bad(format!(
                "任務「{}」現在是 {}，不能等待",
                task.title,
                task.state.name()
            )));
        }
        if let WaitCond::Subtasks { tasks: subs } = &on {
            for sub in subs {
                match tasks::task(txn.conn(), *sub).await.map_err(internal)? {
                    Some(s) if s.goal == task.goal && s.id != task.id => {}
                    _ => return Err(bad(format!("同一個 Goal 裡沒有任務 {sub}"))),
                }
            }
        }
        // 只能等這個 project 監看的 repo（token 看得到的其他 repo 不行）。
        if let Some(repo) = on.repo() {
            let repos = automation::project_repos(txn.conn(), ctx.project)
                .await
                .map_err(internal)?;
            if !repos.contains(&repo) {
                return Err(bad(format!("這個 project 沒有監看 {repo}，不能等它的事件")));
            }
        }
        let desc = on.describe();
        let result: Result<(), Error> = async {
            watcher::suspend_task(&mut txn, &task, ctx.project, on, deadline, &ctx.actor, now)
                .await?;
            txn.commit().await
        }
        .await;
        result.map_err(internal)?;
        // 等待後這一輪就結束（等同 AllowThenStop，但參數錯誤時模型還能重試）。
        ctx.finish();
        Ok(format!(
            "suspended：{desc}，最多等到 {}",
            format_utc(deadline)
        ))
    }

    async fn schedule(&self, args: ScheduleArgs) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        let now = ctx.clock.now();
        let spec = match args.kind.as_str() {
            "once" => {
                let at = match (&args.at, args.in_minutes) {
                    (Some(at), None) => parse_time_arg("at", at)?,
                    (None, Some(m)) if (1..=366 * 24 * 60).contains(&m) => {
                        Timestamp(now.0 + m * 60)
                    }
                    _ => return Err(bad("once 要給 at 或 in_minutes（擇一）")),
                };
                ScheduleSpec::Once { at }
            }
            "every" => ScheduleSpec::Every {
                cron: args
                    .cron
                    .clone()
                    .filter(|c| !c.trim().is_empty())
                    .ok_or_else(|| bad("every 要給 cron"))?,
                until: args
                    .until
                    .as_deref()
                    .map(|u| parse_time_arg("until", u))
                    .transpose()?,
            },
            k => return Err(bad(format!("kind 要是 once 或 every：{k}"))),
        };
        let owner = match ctx.scope {
            Scope::Project => ScheduleOwner::Chat(ctx.project),
            Scope::Goal(goal) => ScheduleOwner::Planner {
                project: ctx.project,
                goal,
            },
            Scope::Task { .. } => return Err(bad("worker 不能設排程；要等時間用 wait_for")),
            Scope::Global => ScheduleOwner::Global,
        };
        let mut txn = ctx.hub.begin().await.map_err(internal)?;
        if let Scope::Goal(goal) = ctx.scope {
            self.active_goal(txn.conn(), goal).await?;
        }
        let result: Result<Result<automation::ScheduleRow, String>, Error> = async {
            let row = match automation::insert_schedule(
                txn.conn(),
                owner,
                spec,
                &args.prompt,
                &ctx.actor,
                now,
            )
            .await?
            {
                Ok(r) => r,
                Err(e) => return Ok(Err(e)),
            };
            let card = notice(
                "已建立排程",
                format!(
                    "{}：{}（下一次 {}）",
                    row.spec.cadence(),
                    tasks::one_line(&row.prompt, 200),
                    automation::local(row.next_at)
                ),
            );
            match ctx.scope {
                Scope::Project => {
                    automation::push(&mut txn, now).await?;
                    txn.commit().await?;
                    // 對話 agent 的卡片附在這則回覆下面（commit 成功後才加）。
                    ctx.cards.lock().unwrap().push(card);
                }
                Scope::Goal(_) | Scope::Task { .. } | Scope::Global => {
                    post_card(&mut txn, ctx.project, card, Some(ctx.run_id)).await?;
                    automation::push(&mut txn, now).await?;
                    txn.commit().await?;
                }
            }
            Ok(Ok(row))
        }
        .await;
        let row = result.map_err(internal)?.map_err(bad)?;
        Ok(json!({
            "schedule_id": row.id.to_string(),
            "next_at": format_utc(row.next_at),
            "expires_at": row.expires_at.map(format_utc),
        })
        .to_string())
    }

    async fn create_listener(&self, args: ListenerArgs) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        if !matches!(ctx.scope, Scope::Project) {
            return Err(bad("只有對話 agent 能提案 listener"));
        }
        let action = match (args.action.as_str(), args.template) {
            ("create_goal", Some(template)) => {
                pots_proto::ListenerActionSpec::CreateGoal { template }
            }
            ("quick_fix", Some(template)) => pots_proto::ListenerActionSpec::QuickFix { template },
            ("notify_chat", _) => pots_proto::ListenerActionSpec::NotifyChat,
            ("create_goal" | "quick_fix", None) => {
                return Err(bad("create_goal／quick_fix 要給 template"));
            }
            (a, _) => return Err(bad(format!("不認得的動作：{a}"))),
        };
        let spec = automation::check_spec(pots_proto::ListenerSpec {
            project: ctx.project,
            kinds: args.kinds,
            resource: args.resource,
            fields: args.fields,
            action,
            max_per_hour: args
                .max_per_hour
                .unwrap_or(automation::DEFAULT_MAX_PER_HOUR),
            budget_per_trigger_cents: None,
        })
        .map_err(bad)?;
        let now = ctx.clock.now();
        let result: Result<
            Result<(pots_proto::ListenerId, pots_proto::ApprovalId), String>,
            Error,
        > = async {
            let mut txn = ctx.hub.begin().await?;
            let id = match automation::save_listener(
                txn.conn(),
                None,
                &spec,
                ListenerDbState::Proposed,
                &ctx.actor,
                now,
            )
            .await?
            {
                Ok(id) => id,
                Err(e) => return Ok(Err(e)),
            };
            let approval =
                crate::approvals::propose_listener(&mut txn, ctx, id, &spec, now).await?;
            automation::push(&mut txn, now).await?;
            txn.commit().await?;
            Ok(Ok((id, approval)))
        }
        .await;
        let (id, approval) = result.map_err(internal)?.map_err(bad)?;
        ctx.cards
            .lock()
            .unwrap()
            .push(Card::Approval { id: approval });
        Ok(json!({
            "listener_id": id.to_string(),
            "approval_id": approval.to_string(),
            "state": "pending_approval",
            "note": "已提案，使用者會看到一張核准卡片，核准後才生效。",
        })
        .to_string())
    }

    /// 對話 agent 只能暫停自己的 project；總管可以指定任一 project（不給就是全部）。
    fn pause_scope(&self, project_id: Option<&str>) -> Result<Option<ProjectId>, ToolError> {
        match self.ctx.scope {
            Scope::Global => match project_id {
                Some(p) => Ok(Some(parse_id("project_id", p)?)),
                None => Ok(None),
            },
            _ => {
                let project = match project_id {
                    Some(p) => parse_id("project_id", p)?,
                    None => self.ctx.project,
                };
                if project != self.ctx.project {
                    return Err(bad("你只能暫停自己的 project"));
                }
                Ok(Some(project))
            }
        }
    }

    async fn pause(&self, args: PauseArgs) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        let project = self.pause_scope(args.project_id.as_deref())?;
        let now = ctx.clock.now();
        let until = args
            .until
            .as_deref()
            .map(|u| parse_time_arg("until", u))
            .transpose()?;
        if until.is_some_and(|u| u.0 <= now.0) {
            return Err(bad("until 已經過了"));
        }
        let result: Result<Result<(), String>, Error> = async {
            let mut txn = ctx.hub.begin().await?;
            // 別人（例如使用者）設的暫停不能被蓋掉（否則可以換成很短的期限再恢復）。
            if let Some(p) = automation::pauses(txn.conn())
                .await?
                .into_iter()
                .find(|p| p.project == project)
                && p.by != ctx.actor
            {
                return Ok(Err(format!(
                    "{}已經被{}暫停了",
                    project.map(|_| "這個 project").unwrap_or("所有 project"),
                    p.by.label()
                )));
            }
            automation::pause(txn.conn(), project, until, &ctx.actor, now).await?;
            automation::push(&mut txn, now).await?;
            txn.commit().await?;
            Ok(Ok(()))
        }
        .await;
        result.map_err(internal)?.map_err(bad)?;
        let card = Card::Notice {
            title: "已暫停排程與 listener".into(),
            body: match until {
                Some(u) => format!(
                    "{} 自動恢復；暫停期間錯過的觸發不補跑。",
                    automation::local(u)
                ),
                None => "要手動恢復；暫停期間錯過的觸發不補跑。".into(),
            },
            actions: vec![
                pots_proto::NoticeAction::ViewAutomations,
                pots_proto::NoticeAction::ResumeAutomations,
            ],
        };
        if ctx.scope == Scope::Global {
            let mut txn = ctx.hub.begin().await.map_err(internal)?;
            post_card(&mut txn, ctx.project, card, Some(ctx.run_id))
                .await
                .map_err(internal)?;
            txn.commit().await.map_err(internal)?;
        } else {
            ctx.cards.lock().unwrap().push(card);
        }
        Ok(json!({
            "paused": project.map(|p| p.to_string()).unwrap_or_else(|| "all".into()),
            "until": until.map(format_utc),
        })
        .to_string())
    }

    async fn resume(&self, args: ResumeArgs) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        let project = self.pause_scope(args.project_id.as_deref())?;
        let now = ctx.clock.now();
        let result: Result<Result<(), String>, Error> = async {
            let mut txn = ctx.hub.begin().await?;
            let pause = automation::pauses(txn.conn())
                .await?
                .into_iter()
                .find(|p| p.project == project);
            match pause {
                None => return Ok(Err("這個 project 沒有被暫停".into())),
                // 使用者自己暫停的只有使用者能恢復。
                Some(p) if p.by != ctx.actor => {
                    return Ok(Err(format!("這是{}暫停的，請使用者自己恢復", p.by.label())));
                }
                Some(_) => {}
            }
            automation::resume(txn.conn(), project).await?;
            automation::push(&mut txn, now).await?;
            txn.commit().await?;
            Ok(Ok(()))
        }
        .await;
        result.map_err(internal)?.map_err(bad)?;
        Ok(json!({
            "resumed": project.map(|p| p.to_string()).unwrap_or_else(|| "all".into()),
        })
        .to_string())
    }
}
