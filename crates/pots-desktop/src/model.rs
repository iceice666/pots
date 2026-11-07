//! 客戶端的 view model。
//!
//! 型別依 `proposal/architecture.md` 的核心資料模型與 `proposal/ux-and-frontend.md`
//! 的即時協定命名，是伺服器推給 UI 的「檢視」而不是資料庫列。現在由 `mock`
//! 模組填入假資料；之後接 WebSocket 時，`ServerEvent` 直接套用到 [`Store`]。
//!
//! ID 在正式環境是 UUIDv7；這裡先用 `u64` newtype，序列化格式之後再對齊。

use serde::{Deserialize, Serialize};

macro_rules! id_type {
    ($($name:ident),* $(,)?) => {$(
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub struct $name(pub u64);
    )*};
}

id_type!(
    InstanceId, ProjectId, ItemId, GoalId, TaskId, ApprovalId, ArtifactId, ScheduleId, ListenerId,
);

/// `$2`、`$1.40` 這類金額顯示。
pub fn format_dollars(cents: u32) -> String {
    if cents.is_multiple_of(100) {
        format!("${}", cents / 100)
    } else {
        format!("${}.{:02}", cents / 100, cents % 100)
    }
}

/// Unix 秒。假資料把它當作本地時間，顯示時不做時區換算。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Timestamp(pub i64);

impl Timestamp {
    pub fn hh_mm(self) -> String {
        let secs = self.0.rem_euclid(86_400);
        format!("{:02}:{:02}", secs / 3600, (secs % 3600) / 60)
    }

    /// 「2 分鐘前」這類相對時間；超過一天改顯示時刻。
    pub fn relative_to(self, now: Timestamp) -> String {
        let delta = (now.0 - self.0).max(0);
        match delta {
            0..60 => "剛剛".into(),
            60..3600 => format!("{} 分鐘前", delta / 60),
            3600..86_400 => format!("{} 小時前", delta / 3600),
            _ => self.hh_mm(),
        }
    }
}

// ---------------------------------------------------------------------------
// 實例與 project

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstanceKind {
    /// 桌面版：app 內嵌服務，經 loopback 連線。
    Desktop,
    /// 多設備版：自架的 `pots-server`。
    MultiDevice,
}

impl InstanceKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Desktop => "桌面版",
            Self::MultiDevice => "多設備版",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionState {
    Connected,
    Reconnecting,
    Offline,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstanceView {
    pub id: InstanceId,
    pub name: String,
    pub kind: InstanceKind,
    pub host: String,
    pub connection: ConnectionState,
    /// 每條連線各自的事件序號，重連時帶 `Resume { last_seq }`。
    pub last_seq: u64,
    pub projects: Vec<ProjectView>,
    pub overseer: OverseerView,
    pub schedules: Vec<ScheduleView>,
    pub listeners: Vec<ListenerView>,
    /// 批次暫停（`pause_automations`）；`None` 表示正常運作。
    pub automation_pause: Option<AutomationPause>,
}

impl InstanceView {
    /// `project` 為 `None` 時是全部；指定 project 時只看掛在它上面的排程。
    pub fn schedules_for(&self, project: Option<ProjectId>) -> impl Iterator<Item = &ScheduleView> {
        self.schedules
            .iter()
            .filter(move |s| project.is_none_or(|p| s.project == Some(p)))
    }

    /// 指定 project 時，也包含套用到所有 project 的 listener。
    pub fn listeners_for(&self, project: Option<ProjectId>) -> impl Iterator<Item = &ListenerView> {
        self.listeners
            .iter()
            .filter(move |l| project.is_none_or(|p| l.project.is_none() || l.project == Some(p)))
    }

    /// 頻道「自動化」分頁上的數字。
    pub fn automation_count(&self, project: ProjectId) -> usize {
        self.schedules_for(Some(project)).count() + self.listeners_for(Some(project)).count()
    }

    pub fn project_name(&self, project: ProjectId) -> Option<&str> {
        self.projects
            .iter()
            .find(|p| p.id == project)
            .map(|p| p.name.as_str())
    }
}

/// 對話頂端顯示的 agent 狀態。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentState {
    Idle,
    Thinking,
    Planning,
    WaitingCi,
    WaitingApproval,
}

impl AgentState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "閒置",
            Self::Thinking => "思考中",
            Self::Planning => "規劃中",
            Self::WaitingCi => "等待 CI",
            Self::WaitingApproval => "等你核准",
        }
    }

    /// 本輪還在跑：此時送出的訊息會排隊，頂端顯示「停止」。
    /// 等你核准時 agent 沒有在執行，不算忙。
    pub fn is_busy(self) -> bool {
        matches!(self, Self::Thinking | Self::Planning | Self::WaitingCi)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Budget {
    pub spent_cents: u32,
    pub limit_cents: u32,
}

impl Budget {
    pub fn label(&self) -> String {
        format!(
            "本週預算 ${}.{:02} / ${}",
            self.spent_cents / 100,
            self.spent_cents % 100,
            self.limit_cents / 100
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectView {
    pub id: ProjectId,
    pub name: String,
    pub agent_name: String,
    pub agent_state: AgentState,
    pub budget: Budget,
    pub unread: u32,
    pub items: Vec<StreamItemView>,
    pub goals: Vec<GoalView>,
    pub artifacts: Vec<ArtifactView>,
}

// ---------------------------------------------------------------------------
// 對話流

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MsgStatus {
    Streaming,
    Done,
    Failed,
    /// 客戶端狀態：已送出、排在本輪結束後處理。
    Queued,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SendMode {
    /// 等本輪結束再處理。
    #[default]
    Queue,
    /// 發出 steer 事件，中斷目前的執行並重新規劃。
    Steer,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Author {
    Human,
    /// project 對話 agent、總管或 worker；`label` 是顯示名稱。
    Agent {
        label: String,
        role: AgentRole,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentRole {
    Project,
    Overseer,
    Worker,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolSummary {
    pub text: String,
    pub details: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Msg {
    pub author: Author,
    pub text: String,
    pub status: MsgStatus,
    pub tools: Option<ToolSummary>,
    /// 訊息下方的內嵌卡片（例如 agent 回覆附帶的 Goal 或核准卡）。
    pub cards: Vec<Card>,
    pub steer: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactKind {
    Spec,
    Brief,
    Ledger,
    Report,
    ProjectNotes,
}

impl ArtifactKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Spec => "Spec",
            Self::Brief => "Brief",
            Self::Ledger => "Ledger",
            Self::Report => "Report",
            Self::ProjectNotes => "Project Notes",
        }
    }

    /// 列表與面板上的小標籤。
    pub fn tag(self) -> &'static str {
        match self {
            Self::Spec => "SPEC",
            Self::Brief => "BRIEF",
            Self::Ledger => "LEDGER",
            Self::Report => "REPORT",
            Self::ProjectNotes => "NOTES",
        }
    }
}

/// 掛在 project 或 Goal 上的 Markdown 文件，你與 agent 都能編輯
/// （proposal/artifacts.md）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArtifactView {
    pub id: ArtifactId,
    pub kind: ArtifactKind,
    pub title: String,
    /// 列表第二行，例如「Goal 交接 · v1」；agent 正在寫入時改顯示寫入狀態。
    pub note: String,
    pub rev: u32,
    /// 掛在哪個 Goal；`None` 表示掛在 project。
    pub goal: Option<GoalId>,
    /// Markdown 全文。
    pub body: String,
    /// 你改過的區段標題與時間。
    pub human_edits: Vec<(String, Timestamp)>,
    /// agent 正在寫入的區段標題；寫入期間這段在客戶端唯讀。
    pub agent_writing: Option<String>,
    /// 等你核准的修改提案數；Project Notes 的 agent 修改只能是提案。
    pub proposals: u32,
}

impl ArtifactView {
    pub fn list_note(&self) -> String {
        if self.agent_writing.is_some() {
            format!("v{} · agent 正在更新", self.rev)
        } else {
            self.note.clone()
        }
    }

    pub fn edited_at(&self, heading: &str) -> Option<Timestamp> {
        self.human_edits
            .iter()
            .find(|(h, _)| h == heading)
            .map(|(_, at)| *at)
    }
}

/// Artifact 的一個 `## ` 標題區段。agent 以區段為單位寫入，客戶端也逐段編輯，
/// 這樣 agent 正在寫的那段可以單獨鎖住。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactSection {
    /// `None` 是第一個 `##` 之前的部分（文件標題與前言）。
    pub heading: Option<String>,
    /// 這段的 Markdown，含標題行。
    pub source: String,
}

/// 依 `## ` 標題切段；程式碼區塊裡的 `##` 不算。
pub fn split_sections(markdown: &str) -> Vec<ArtifactSection> {
    fn flush(out: &mut Vec<ArtifactSection>, heading: Option<String>, buf: &str) {
        let source = buf.trim();
        if heading.is_some() || !source.is_empty() {
            out.push(ArtifactSection {
                heading,
                source: source.to_string(),
            });
        }
    }

    let mut out = Vec::new();
    let mut heading = None;
    let mut buf = String::new();
    let mut in_fence = false;
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
        }
        if !in_fence && let Some(h) = section_heading(line) {
            flush(&mut out, heading.take(), &buf);
            buf.clear();
            heading = Some(h);
        }
        buf.push_str(line);
        buf.push('\n');
    }
    flush(&mut out, heading, &buf);
    out
}

/// 一行（或一段的第一行）若是 `## ` 標題，回傳標題文字。
pub fn section_heading(source: &str) -> Option<String> {
    let first = source.lines().next()?;
    first
        .strip_prefix("## ")
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
}

/// 把各段的 Markdown 接回全文。
pub fn join_sections<'a>(sources: impl IntoIterator<Item = &'a str>) -> String {
    let parts: Vec<&str> = sources
        .into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let mut out = parts.join("\n\n");
    out.push('\n');
    out
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Card {
    GoalCreated {
        goal: GoalId,
    },
    /// 總管在別的 project 開的 Goal：只顯示摘要，不展開任務樹。
    RemoteGoal {
        project: String,
        title: String,
        note: String,
    },
    Approval {
        id: ApprovalId,
    },
    Artifact {
        id: ArtifactId,
        kind: ArtifactKind,
        title: String,
        note: String,
    },
    CatchUp {
        summary: String,
    },
    /// 總管的早安摘要等結構化回報。
    Digest {
        title: String,
        rows: Vec<(String, String)>,
        footer: String,
    },
    /// 已執行的動作回報，例如「已暫停 3 個排程」。
    Notice {
        title: String,
        body: String,
        actions: Vec<NoticeAction>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoticeAction {
    ViewAutomations,
    ResumeAutomations,
}

impl NoticeAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::ViewAutomations => "查看清單",
            Self::ResumeAutomations => "立即恢復",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum StreamItemKind {
    Human(Msg),
    Agent(Msg),
    Card(Card),
    /// 只給人看的事件（例如「PR #42 已開」），不進 OM。
    Event {
        text: String,
    },
    /// 日期分隔線，僅供顯示。
    DayDivider(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StreamItemView {
    pub id: ItemId,
    pub at: Timestamp,
    pub kind: StreamItemKind,
}

// ---------------------------------------------------------------------------
// 任務樹

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStateView {
    Planned,
    Running,
    Waiting { on: String },
    NeedsHuman { reason: String },
    Done { outcome: String },
    Failed { attempts: u8, error: String },
    Cancelled,
}

impl TaskStateView {
    pub fn bucket(&self) -> TaskBucket {
        match self {
            Self::Running => TaskBucket::Running,
            Self::Waiting { .. } | Self::NeedsHuman { .. } | Self::Planned => TaskBucket::Waiting,
            Self::Done { .. } => TaskBucket::Done,
            Self::Failed { .. } | Self::Cancelled => TaskBucket::Stopped,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum TaskBucket {
    Running,
    Waiting,
    Done,
    Stopped,
}

impl TaskBucket {
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "進行中",
            Self::Waiting => "等待中",
            Self::Done => "完成",
            Self::Stopped => "已停止",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskStep {
    pub text: String,
    /// `None` 表示進行中的步驟。
    pub at: Option<Timestamp>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskView {
    pub id: TaskId,
    pub goal: GoalId,
    pub title: String,
    /// 例如「worker · Claude Code」「reviewer subagent」。
    pub assignee: String,
    pub summary: String,
    pub state: TaskStateView,
    pub branch: Option<String>,
    pub elapsed_min: u32,
    pub cost_cents: u32,
    pub steps: Vec<TaskStep>,
    pub thread: Vec<StreamItemView>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TeamPostKind {
    Finding,
    Question,
    Decision,
    Blocker,
    Handoff,
}

impl TeamPostKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Finding => "發現",
            Self::Question => "問題",
            Self::Decision => "決定",
            Self::Blocker => "阻礙",
            Self::Handoff => "交接",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TeamPost {
    pub kind: TeamPostKind,
    pub from: String,
    pub at: Timestamp,
    pub body: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GoalView {
    pub id: GoalId,
    pub title: String,
    pub planner_note: String,
    pub budget_used_pct: u8,
    pub tasks: Vec<TaskView>,
    pub team: Vec<TeamPost>,
    pub brief: String,
}

impl GoalView {
    pub fn count(&self, bucket: TaskBucket) -> usize {
        self.tasks
            .iter()
            .filter(|t| t.state.bucket() == bucket)
            .count()
    }

    /// 等待 CI 的任務另外算，Goal 卡片上與「進行中」分開顯示。
    pub fn waiting_ci(&self) -> usize {
        self.tasks
            .iter()
            .filter(|t| matches!(&t.state, TaskStateView::Waiting { on } if on.contains("CI")))
            .count()
    }
}

// ---------------------------------------------------------------------------
// 自動化：排程與 listener（proposal/events-and-automation.md）

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScheduleCreator {
    Chat,
    Planner,
    Overseer,
}

impl ScheduleCreator {
    pub fn label(self) -> &'static str {
        match self {
            Self::Chat => "對話 agent",
            Self::Planner => "Planner",
            Self::Overseer => "總管",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TriggerStatus {
    Done,
    Running,
    Failed,
    /// NotifyChat 一類只通知、不開工的觸發。
    Notified,
}

impl TriggerStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Done => "完成",
            Self::Running => "進行中",
            Self::Failed => "失敗",
            Self::Notified => "已通知",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TriggerRecord {
    /// 例如「#118 → 支援 refresh token」。
    pub title: String,
    /// 例如「11/5 · 花費 $1.40」。
    pub meta: String,
    pub status: TriggerStatus,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScheduleView {
    pub id: ScheduleId,
    /// `None` 是總管的全域排程。
    pub project: Option<ProjectId>,
    /// 醒來時要做什麼。
    pub prompt: String,
    /// 標題下的小字，例如「30 天後過期」「單次」。
    pub note: String,
    pub creator: ScheduleCreator,
    /// 例如「每天 08:00」「單次」。
    pub cadence: String,
    pub next: String,
    /// 批次暫停期間，恢復後第一次觸發的時間；`None` 表示不受這次暫停影響。
    pub next_after_pause: Option<String>,
    pub enabled: bool,
    pub history: Vec<TriggerRecord>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrustLevel {
    Owned,
    Members,
    Public,
}

impl TrustLevel {
    pub fn label(self) -> &'static str {
        match self {
            Self::Owned => "Owned",
            Self::Members => "Members",
            Self::Public => "Public",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ListenerState {
    Active,
    Disabled,
    /// agent 用 `create_listener` 提案，等 Inbox 裡這筆核准。
    PendingApproval(ApprovalId),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ListenerView {
    pub id: ListenerId,
    /// `None` 表示套用到這個實例的所有 project。
    pub project: Option<ProjectId>,
    /// 條件的白話描述，例如「issue 加上 agent label」。
    pub condition: String,
    /// 動作的白話描述，例如「開 Goal」。
    pub action: String,
    pub source: String,
    /// 來源的帳號或說明，例如「個人帳號」。
    pub source_detail: String,
    pub trust: TrustLevel,
    pub resource: String,
    pub event: String,
    pub matcher: String,
    pub action_detail: String,
    /// 開 Goal 時 Brief 用的模板。
    pub template: Option<String>,
    pub budget_per_trigger_cents: Option<u32>,
    pub max_per_hour: Option<u32>,
    pub triggers_this_week: u32,
    pub state: ListenerState,
    pub history: Vec<TriggerRecord>,
}

impl ListenerView {
    pub fn title(&self) -> String {
        format!("{} → {}", self.condition, self.action)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AutomationPause {
    /// 自動恢復的時間，例如「週一 09:00」。
    pub until: String,
    /// 表格裡的短寫，例如「週一」。
    pub until_short: String,
    /// 例如「由總管在 09:20 依你的要求暫停」。
    pub by: String,
}

// ---------------------------------------------------------------------------
// 核准與 Inbox

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InboxKind {
    Approval,
    NeedsYou,
    Mention,
}

impl InboxKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Approval => "核准",
            Self::NeedsYou => "需要你處理",
            Self::Mention => "找你",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    Pending,
    Approved,
    Rejected,
    /// 非核准類項目被處理掉（例如「再試一次」）。
    Resolved(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InboxAction {
    Approve,
    Reject,
    ViewDiff,
    EditConditions,
    Retry,
    CancelTask,
    StartLogin,
    OpenThread,
    OpenOverseer,
}

impl InboxAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Approve => "核准",
            Self::Reject => "拒絕",
            Self::ViewDiff => "看 diff",
            Self::EditConditions => "修改條件",
            Self::Retry => "再試一次",
            Self::CancelTask => "取消任務",
            Self::StartLogin => "開始登入",
            Self::OpenThread => "在頻道中查看",
            Self::OpenOverseer => "開啟總管",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InboxItem {
    pub id: ApprovalId,
    pub instance: InstanceId,
    /// `None` 表示來自總管。
    pub project: Option<ProjectId>,
    pub context: String,
    pub kind: InboxKind,
    pub title: String,
    pub detail: String,
    pub facts: Vec<String>,
    pub actions: Vec<InboxAction>,
    pub at: Timestamp,
    /// 核准卡片在對話中的有效期限說明，例如「72 小時內有效」。
    pub expiry: Option<String>,
    pub decision: Decision,
}

impl InboxItem {
    pub fn is_open(&self) -> bool {
        self.decision == Decision::Pending
    }
}

// ---------------------------------------------------------------------------
// 總管

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectOverview {
    pub project: String,
    pub state: AgentState,
    pub note: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Priority {
    High,
    Medium,
    Low,
}

impl Priority {
    pub fn label(self) -> &'static str {
        match self {
            Self::High => "高",
            Self::Medium => "中",
            Self::Low => "低",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Observation {
    pub priority: Priority,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OverseerView {
    pub state: AgentState,
    pub items: Vec<StreamItemView>,
    pub overview: Vec<ProjectOverview>,
    pub memory: Vec<Observation>,
    pub memory_total: u32,
}

// ---------------------------------------------------------------------------
// 即時協定（proposal/ux-and-frontend.md#即時協定）

/// 伺服器推送的事件；序號每個實例各自獨立。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum ServerEvent {
    Item {
        seq: u64,
        item: StreamItemView,
    },
    /// agent 串流；不寫入事件日誌，只寫最後的 `ItemDone`。
    Delta {
        seq: u64,
        item_id: ItemId,
        text: String,
    },
    ItemDone {
        seq: u64,
        item_id: ItemId,
        status: MsgStatus,
    },
    AgentState {
        seq: u64,
        project: ProjectId,
        state: AgentState,
    },
    TaskChanged {
        seq: u64,
        task: TaskView,
    },
    Inbox {
        seq: u64,
        pending: u32,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum ClientCommand {
    Resume {
        last_seq: u64,
    },
    Send {
        project: ProjectId,
        text: String,
        mode: SendMode,
    },
    Decide {
        approval: ApprovalId,
        approve: bool,
        note: Option<String>,
    },
    Cancel {
        task: TaskId,
    },
}

// ---------------------------------------------------------------------------
// 客戶端整體狀態

/// 所有已連線實例的資料，加上合併後的 Inbox。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Store {
    pub now: Timestamp,
    pub instances: Vec<InstanceView>,
    pub inbox: Vec<InboxItem>,
    pub next_id: u64,
    /// 尚未送出的指令；接上 WebSocket 前只是記錄，方便除錯與測試。
    pub outbox: Vec<(InstanceId, ClientCommand)>,
}

impl Store {
    pub fn instance(&self, id: InstanceId) -> Option<&InstanceView> {
        self.instances.iter().find(|i| i.id == id)
    }

    pub fn instance_mut(&mut self, id: InstanceId) -> Option<&mut InstanceView> {
        self.instances.iter_mut().find(|i| i.id == id)
    }

    pub fn project(&self, instance: InstanceId, project: ProjectId) -> Option<&ProjectView> {
        self.instance(instance)?
            .projects
            .iter()
            .find(|p| p.id == project)
    }

    pub fn project_mut(
        &mut self,
        instance: InstanceId,
        project: ProjectId,
    ) -> Option<&mut ProjectView> {
        self.instance_mut(instance)?
            .projects
            .iter_mut()
            .find(|p| p.id == project)
    }

    pub fn goal(&self, instance: InstanceId, goal: GoalId) -> Option<&GoalView> {
        self.instance(instance)?
            .projects
            .iter()
            .flat_map(|p| p.goals.iter())
            .find(|g| g.id == goal)
    }

    pub fn task(&self, instance: InstanceId, task: TaskId) -> Option<(&GoalView, &TaskView)> {
        self.instance(instance)?
            .projects
            .iter()
            .flat_map(|p| p.goals.iter())
            .find_map(|g| g.tasks.iter().find(|t| t.id == task).map(|t| (g, t)))
    }

    pub fn task_mut(&mut self, instance: InstanceId, task: TaskId) -> Option<&mut TaskView> {
        self.instance_mut(instance)?
            .projects
            .iter_mut()
            .flat_map(|p| p.goals.iter_mut())
            .flat_map(|g| g.tasks.iter_mut())
            .find(|t| t.id == task)
    }

    pub fn approval(&self, id: ApprovalId) -> Option<&InboxItem> {
        self.inbox.iter().find(|i| i.id == id)
    }

    /// Artifact 與它所屬的 project。
    pub fn artifact(
        &self,
        instance: InstanceId,
        id: ArtifactId,
    ) -> Option<(&ProjectView, &ArtifactView)> {
        self.instance(instance)?
            .projects
            .iter()
            .find_map(|p| p.artifacts.iter().find(|a| a.id == id).map(|a| (p, a)))
    }

    /// 你在編輯器裡改了 artifact：更新全文，並標示改過的區段。
    /// 寫回服務走 HTTP API（尚未定義），這裡只改本地狀態。
    pub fn edit_artifact(
        &mut self,
        instance: InstanceId,
        id: ArtifactId,
        body: String,
        section: Option<String>,
    ) {
        let now = self.now;
        let Some(a) = self.instance_mut(instance).and_then(|i| {
            i.projects
                .iter_mut()
                .flat_map(|p| p.artifacts.iter_mut())
                .find(|a| a.id == id)
        }) else {
            return;
        };
        a.body = body;
        if let Some(heading) = section {
            match a.human_edits.iter_mut().find(|(h, _)| *h == heading) {
                Some(edit) => edit.1 = now,
                None => a.human_edits.push((heading, now)),
            }
        }
    }

    pub fn schedule(&self, instance: InstanceId, id: ScheduleId) -> Option<&ScheduleView> {
        self.instance(instance)?
            .schedules
            .iter()
            .find(|s| s.id == id)
    }

    pub fn listener(&self, instance: InstanceId, id: ListenerId) -> Option<&ListenerView> {
        self.instance(instance)?
            .listeners
            .iter()
            .find(|l| l.id == id)
    }

    /// 提前結束批次暫停（`resume_automations`）。
    pub fn resume_automations(&mut self, instance: InstanceId) {
        if let Some(i) = self.instance_mut(instance) {
            i.automation_pause = None;
        }
    }

    pub fn set_schedule_enabled(&mut self, instance: InstanceId, id: ScheduleId, enabled: bool) {
        if let Some(s) = self
            .instance_mut(instance)
            .and_then(|i| i.schedules.iter_mut().find(|s| s.id == id))
        {
            s.enabled = enabled;
        }
    }

    /// 啟用或停用 listener；等待核准中的不受影響。
    pub fn set_listener_enabled(&mut self, instance: InstanceId, id: ListenerId, enabled: bool) {
        if let Some(l) = self
            .instance_mut(instance)
            .and_then(|i| i.listeners.iter_mut().find(|l| l.id == id))
            && !matches!(l.state, ListenerState::PendingApproval(_))
        {
            l.state = if enabled {
                ListenerState::Active
            } else {
                ListenerState::Disabled
            };
        }
    }

    pub fn delete_schedule(&mut self, instance: InstanceId, id: ScheduleId) {
        if let Some(i) = self.instance_mut(instance) {
            i.schedules.retain(|s| s.id != id);
        }
    }

    pub fn delete_listener(&mut self, instance: InstanceId, id: ListenerId) {
        if let Some(i) = self.instance_mut(instance) {
            i.listeners.retain(|l| l.id != id);
        }
    }

    /// `None` 代表所有實例。
    pub fn pending(&self, instance: Option<InstanceId>) -> usize {
        self.inbox
            .iter()
            .filter(|i| i.is_open() && instance.is_none_or(|id| i.instance == id))
            .count()
    }

    /// 實例直列徽章：只算需要你決定或處理的項目，不含「找你」的訊息。
    pub fn badge_count(&self, instance: Option<InstanceId>) -> usize {
        self.inbox
            .iter()
            .filter(|i| {
                i.is_open()
                    && i.kind != InboxKind::Mention
                    && instance.is_none_or(|id| i.instance == id)
            })
            .count()
    }

    pub fn pending_for_project(&self, instance: InstanceId, project: ProjectId) -> usize {
        self.inbox
            .iter()
            .filter(|i| i.is_open() && i.instance == instance && i.project == Some(project))
            .count()
    }

    pub fn project_needs_you(&self, instance: InstanceId, project: ProjectId) -> bool {
        self.inbox.iter().any(|i| {
            i.is_open()
                && i.instance == instance
                && i.project == Some(project)
                && i.kind == InboxKind::NeedsYou
        })
    }

    fn alloc_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// 在 project 對話流尾端加入使用者訊息，並記下要送出的 `Send` 指令。
    pub fn send_to_project(
        &mut self,
        instance: InstanceId,
        project: ProjectId,
        text: String,
        mode: SendMode,
    ) {
        let id = ItemId(self.alloc_id());
        let at = self.now;
        let Some(p) = self.project_mut(instance, project) else {
            return;
        };
        let busy = p.agent_state.is_busy();
        p.items.push(StreamItemView {
            id,
            at,
            kind: StreamItemKind::Human(Msg {
                author: Author::Human,
                text: text.clone(),
                status: if busy && mode == SendMode::Queue {
                    MsgStatus::Queued
                } else {
                    MsgStatus::Done
                },
                tools: None,
                cards: vec![],
                steer: mode == SendMode::Steer,
            }),
        });
        if mode == SendMode::Steer {
            p.agent_state = AgentState::Thinking;
        }
        self.outbox.push((
            instance,
            ClientCommand::Send {
                project,
                text,
                mode,
            },
        ));
    }

    pub fn send_to_overseer(&mut self, instance: InstanceId, text: String) {
        let id = ItemId(self.alloc_id());
        let at = self.now;
        if let Some(inst) = self.instance_mut(instance) {
            inst.overseer.items.push(StreamItemView {
                id,
                at,
                kind: StreamItemKind::Human(Msg {
                    author: Author::Human,
                    text,
                    status: MsgStatus::Done,
                    tools: None,
                    cards: vec![],
                    steer: false,
                }),
            });
        }
    }

    pub fn send_to_task(&mut self, instance: InstanceId, task: TaskId, text: String) {
        let id = ItemId(self.alloc_id());
        let at = self.now;
        if let Some(t) = self.task_mut(instance, task) {
            t.thread.push(StreamItemView {
                id,
                at,
                kind: StreamItemKind::Human(Msg {
                    author: Author::Human,
                    text,
                    status: MsgStatus::Done,
                    tools: None,
                    cards: vec![],
                    steer: false,
                }),
            });
        }
    }

    pub fn post_goal_note(&mut self, instance: InstanceId, goal: GoalId, text: String) {
        let at = self.now;
        if let Some(g) = self.instance_mut(instance).and_then(|i| {
            i.projects
                .iter_mut()
                .flat_map(|p| p.goals.iter_mut())
                .find(|g| g.id == goal)
        }) {
            g.team.insert(
                0,
                TeamPost {
                    kind: TeamPostKind::Question,
                    from: "你 → Planner".into(),
                    at,
                    body: text,
                },
            );
        }
    }

    /// 處理 Inbox 項目或對話內核准卡上的按鈕。回傳是否改變了狀態。
    pub fn act(&mut self, id: ApprovalId, action: InboxAction) -> bool {
        let Some(item) = self.inbox.iter_mut().find(|i| i.id == id) else {
            return false;
        };
        if !item.is_open() {
            return false;
        }
        let decision = match action {
            InboxAction::Approve => Decision::Approved,
            InboxAction::Reject => Decision::Rejected,
            InboxAction::Retry => Decision::Resolved("已重新開始自動修復".into()),
            InboxAction::CancelTask => Decision::Resolved("已取消任務".into()),
            InboxAction::StartLogin => Decision::Resolved("已開始登入流程".into()),
            _ => return false,
        };
        let instance = item.instance;
        let approve = decision == Decision::Approved;
        let is_approval = item.kind == InboxKind::Approval;
        item.decision = decision;
        // 核准的是 listener 提案：核准就生效，拒絕就撤掉。
        if is_approval && let Some(inst) = self.instance_mut(instance) {
            let pending = ListenerState::PendingApproval(id);
            if approve {
                for l in inst.listeners.iter_mut().filter(|l| l.state == pending) {
                    l.state = ListenerState::Active;
                }
            } else {
                inst.listeners.retain(|l| l.state != pending);
            }
        }
        if is_approval {
            self.outbox.push((
                instance,
                ClientCommand::Decide {
                    approval: id,
                    approve,
                    note: None,
                },
            ));
        }
        true
    }

    pub fn cancel_task(&mut self, instance: InstanceId, task: TaskId) {
        if let Some(t) = self.task_mut(instance, task) {
            if matches!(
                t.state,
                TaskStateView::Done { .. } | TaskStateView::Cancelled
            ) {
                return;
            }
            t.state = TaskStateView::Cancelled;
            self.outbox.push((instance, ClientCommand::Cancel { task }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_formats() {
        let t = Timestamp(10 * 3600 + 2 * 60);
        assert_eq!(t.hh_mm(), "10:02");
        assert_eq!(t.relative_to(Timestamp(t.0 + 30)), "剛剛");
        assert_eq!(t.relative_to(Timestamp(t.0 + 120)), "2 分鐘前");
        assert_eq!(t.relative_to(Timestamp(t.0 + 7200)), "2 小時前");
    }

    #[test]
    fn sections_round_trip() {
        let md =
            "# 標題\n\n前言\n\n## 目標\n\n內容\n\n```sh\n## 不是標題\n```\n\n## 範圍\n\n- 一\n";
        let sections = split_sections(md);
        let headings: Vec<_> = sections.iter().map(|s| s.heading.as_deref()).collect();
        assert_eq!(headings, [None, Some("目標"), Some("範圍")]);
        assert!(sections[1].source.contains("## 不是標題"));
        assert_eq!(
            join_sections(sections.iter().map(|s| s.source.as_str())),
            md
        );
        assert_eq!(section_heading("## 範圍\n\n- 一"), Some("範圍".into()));
        assert_eq!(section_heading("# 標題"), None);
    }

    #[test]
    fn dollars() {
        assert_eq!(format_dollars(200), "$2");
        assert_eq!(format_dollars(140), "$1.40");
        assert_eq!(format_dollars(5), "$0.05");
    }

    #[test]
    fn protocol_is_tagged() {
        let cmd = ClientCommand::Send {
            project: ProjectId(1),
            text: "hi".into(),
            mode: SendMode::Steer,
        };
        let json = serde_json::to_value(&cmd).unwrap();
        assert_eq!(json["t"], "Send");
        assert_eq!(json["mode"], "Steer");

        let ev: ServerEvent =
            serde_json::from_str(r#"{"t":"AgentState","seq":7,"project":3,"state":"WaitingCi"}"#)
                .unwrap();
        assert!(matches!(
            ev,
            ServerEvent::AgentState {
                seq: 7,
                project: ProjectId(3),
                state: AgentState::WaitingCi
            }
        ));
    }
}
