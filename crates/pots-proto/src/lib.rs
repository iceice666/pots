//! Pots 客戶端與服務共用的 view model 與即時協定。
//!
//! 型別依 `proposal/architecture.md` 的核心資料模型與 `proposal/ux-and-frontend.md`
//! 的即時協定命名，是伺服器推給 UI 的「檢視」而不是資料庫列。客戶端把
//! [`ServerEvent`] 套用到自己的狀態，使用者動作轉成 [`ClientCommand`] 送出。
//!
//! ID 是 UUIDv7，時間有序，JSON 裡是字串。

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! id_type {
    ($($name:ident),* $(,)?) => {$(
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            /// 新的 UUIDv7。
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            /// 固定的小編號，給假資料與測試用。
            pub const fn from_u64(n: u64) -> Self {
                Self(Uuid::from_u64_pair(0, n))
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }

        impl std::str::FromStr for $name {
            type Err = uuid::Error;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                s.parse().map(Self)
            }
        }
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
    pub fn now() -> Self {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        Self(secs)
    }

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

/// 事件屬於哪一條對話流。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id")]
pub enum StreamScope {
    Project(ProjectId),
    /// 總管的全域對話。
    Overseer,
    /// 任務 thread。
    Task(TaskId),
}

/// 連線（或重連時差距太大）時送的完整狀態，客戶端整份替換。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstanceSnapshot {
    pub id: InstanceId,
    pub name: String,
    pub kind: InstanceKind,
    pub projects: Vec<ProjectSnapshot>,
    pub model_auth: ModelAuthView,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectSnapshot {
    pub id: ProjectId,
    pub name: String,
    pub agent_name: String,
    pub agent_state: AgentState,
    /// 最近的對話；更早的走 HTTP `GET /projects/:id/items?before=<id>`。
    pub items: Vec<StreamItemView>,
}

/// 模型 provider（ChatGPT Plan，經 SIWC 授權）的狀態。不含任何 token。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state")]
pub enum ModelAuthView {
    SignedOut,
    /// 等你在瀏覽器完成登入；`url` 由客戶端用系統瀏覽器開啟。
    Pending {
        url: String,
    },
    SignedIn {
        /// 例如 email；沒有就是 `None`。
        account: Option<String>,
    },
    /// 授權失效、被撤銷或沒有推論權限，需要重新登入。
    Error {
        message: String,
    },
}

/// 伺服器推送的事件。`seq` 每個實例各自遞增，寫進事件日誌，重連時依序補送；
/// 不帶 `seq` 的事件只對當下的連線有意義，不補送。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum ServerEvent {
    Snapshot {
        seq: u64,
        instance: InstanceSnapshot,
    },
    ProjectAdded {
        seq: u64,
        project: ProjectSnapshot,
    },
    Item {
        seq: u64,
        scope: StreamScope,
        item: StreamItemView,
    },
    /// agent 串流的增量文字；不寫入事件日誌。
    Delta {
        scope: StreamScope,
        item_id: ItemId,
        text: String,
    },
    /// 串流中訊息到目前為止的全文，取代客戶端手上的內容；重連時補送，不寫入事件日誌。
    StreamingText {
        scope: StreamScope,
        item_id: ItemId,
        text: String,
    },
    /// 訊息內容有變（例如工具摘要），整筆替換。
    ItemUpdated {
        seq: u64,
        scope: StreamScope,
        item: StreamItemView,
    },
    /// 串流結束；帶最後的完整內容，事件日誌重播時不需要 `Delta`。
    ItemDone {
        seq: u64,
        scope: StreamScope,
        item: StreamItemView,
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
    ModelAuth {
        seq: u64,
        state: ModelAuthView,
    },
    /// 指令被拒絕或執行失敗。
    Error {
        message: String,
    },
}

impl ServerEvent {
    pub fn seq(&self) -> Option<u64> {
        match self {
            Self::Snapshot { seq, .. }
            | Self::ProjectAdded { seq, .. }
            | Self::Item { seq, .. }
            | Self::ItemUpdated { seq, .. }
            | Self::ItemDone { seq, .. }
            | Self::AgentState { seq, .. }
            | Self::TaskChanged { seq, .. }
            | Self::Inbox { seq, .. }
            | Self::ModelAuth { seq, .. } => Some(*seq),
            Self::Delta { .. } | Self::StreamingText { .. } | Self::Error { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum ClientCommand {
    /// 連線後的第一個指令。`last_seq` 為 0 或差距太大時，伺服器回 `Snapshot`。
    Resume {
        last_seq: u64,
    },
    CreateProject {
        name: String,
    },
    Send {
        project: ProjectId,
        /// 客戶端產生的 ID；伺服器用它存這則訊息，客戶端收到 `Item` 時據此去重。
        id: ItemId,
        text: String,
        mode: SendMode,
    },
    /// 停止這個 project 正在跑的 agent run。
    Stop {
        project: ProjectId,
    },
    Decide {
        approval: ApprovalId,
        approve: bool,
        note: Option<String>,
    },
    Cancel {
        task: TaskId,
    },
    /// 開始 ChatGPT（SIWC）登入；伺服器回 `ModelAuth { Pending { url } }`。
    StartModelLogin,
    CancelModelLogin,
    ModelLogout,
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
            project: ProjectId::from_u64(1),
            id: ItemId::from_u64(2),
            text: "hi".into(),
            mode: SendMode::Steer,
        };
        let json = serde_json::to_value(&cmd).unwrap();
        assert_eq!(json["t"], "Send");
        assert_eq!(json["mode"], "Steer");

        assert_eq!(json["project"], "00000000-0000-0000-0000-000000000001");

        let ev: ServerEvent = serde_json::from_str(
            r#"{"t":"AgentState","seq":7,"project":"00000000-0000-0000-0000-000000000003","state":"WaitingCi"}"#,
        )
        .unwrap();
        assert!(matches!(
            ev,
            ServerEvent::AgentState {
                seq: 7,
                project,
                state: AgentState::WaitingCi
            } if project == ProjectId::from_u64(3)
        ));
        assert_eq!(ev.seq(), Some(7));

        let scope = serde_json::to_value(StreamScope::Project(ProjectId::from_u64(3))).unwrap();
        assert_eq!(scope["kind"], "Project");
        let auth = serde_json::to_value(ModelAuthView::Pending { url: "u".into() }).unwrap();
        assert_eq!(auth["state"], "Pending");
    }
}
