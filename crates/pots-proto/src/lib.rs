//! Pots 客戶端與服務共用的 view model 與即時協定。
//!
//! 型別依 `proposal/architecture.md` 的核心資料模型與 `proposal/ux-and-frontend.md`
//! 的即時協定命名，是伺服器推給 UI 的「檢視」而不是資料庫列。客戶端把
//! [`ServerEvent`] 套用到自己的狀態，使用者動作轉成 [`ClientCommand`] 送出。
//!
//! ID 是 UUIDv7，時間有序，JSON 裡是字串。

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 即時協定的版本。`ServerEvent`／`ClientCommand` 有不相容的變動（新增指令、新增必填欄位）
/// 時加一；客戶端遇到版本不同的本機服務會重啟它。
///
/// - 1：M0。
/// - 2：模型 provider 設定、`LoginUrl`、`ProviderTest`。
/// - 3：Observational Memory（`Memory` 事件、觀察的釘選／修正／刪除、記憶用的模型）。
/// - 4：任務樹與 artifact（`GoalChanged`、`ArtifactChanged`、`ArtifactHistory`、
///   `SaveArtifact`、`RestoreArtifact`、`Card::Milestone`、快照帶 Goal 與 artifact）。
/// - 5：外部事件與自動化（`Automation`、`Github`、`ProjectRepos`、`GithubTest`、GitHub 設定、
///   project 的 repo、listener 與排程的指令、`Card::Triggered`、快照帶自動化與 GitHub 設定）。
/// - 6：coding worker（`Worker`、`SaveWorkerSettings`、`RetryTask`、`TaskView` 的 PR 與進度、
///   `TeamPost::to`、`GoalView::quick`、快照帶 worker 設定）。
/// - 7：核准與通知（`ApprovalChanged` 取代 `Inbox`、`Decide` 生效、`Notifications`、`UserMd`、
///   `ProjectMergeMethod` 與對應的指令，快照帶核准、通知設定、USER.md 與 project 的 merge 方式）。
/// - 10：其他 connector（`SaveSource`／`DeleteSource`、`SourceDraft`、`SourceView::url`、
///   `AutomationView::custom_kinds`、feed／探測／狀態頁／自訂 webhook 的事件種類）。
pub const PROTOCOL_VERSION: u32 = 10;

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
    InstanceId,
    ProjectId,
    ItemId,
    GoalId,
    TaskId,
    ApprovalId,
    ArtifactId,
    ScheduleId,
    ListenerId,
    ProviderId,
    ObservationId,
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
    /// 所有暫停（含單一 project 的）；`automation_pause` 是其中「所有 project」那一筆的顯示用文字。
    #[serde(default)]
    pub pauses: Vec<PauseView>,
    /// 外部來源與健康狀態。
    #[serde(default)]
    pub sources: Vec<SourceView>,
    /// 通用 webhook 對應出來的自訂事件種類（listener 表單多選這些）。
    #[serde(default)]
    pub custom_kinds: Vec<String>,
    #[serde(default)]
    pub github: GithubView,
    /// worker 設定（設定的「Worker」類別）。
    #[serde(default)]
    pub worker: WorkerSettingsView,
    /// 桌面通知與靜音時段（設定的「通知」類別）。
    #[serde(default)]
    pub notifications: NotificationSettings,
    /// 跨 project 的使用者偏好（設定的「USER.md」類別）。
    #[serde(default)]
    pub user_md: UserMdView,
    /// 模型 provider 設定與使用中的 provider。
    pub providers: ProvidersView,
    /// 內建 ChatGPT provider 的登入狀態。
    pub model_auth: ModelAuthView,
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

    /// 這個 project 有沒有被單獨暫停。
    pub fn project_paused(&self, project: ProjectId) -> Option<&PauseView> {
        self.pauses.iter().find(|p| p.project == Some(project))
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
    /// 對話 agent 記得什麼（「記憶」分頁）。
    #[serde(default)]
    pub memory: MemoryView,
    /// 監看的 repo，例如 `github.com/owner/repo`。
    #[serde(default)]
    pub repos: Vec<String>,
    /// 平台 merge PR 時預設的方式（agent 申請時可以另外指定）。
    #[serde(default)]
    pub merge_method: MergeMethod,
}

// ---------------------------------------------------------------------------
// Observational Memory（proposal/memory.md）

/// 一條觀察最多幾個字（你的修正超過會被拒絕，模型的輸出超過會被截斷）。
pub const MAX_OBSERVATION_CHARS: usize = 500;

/// 一條觀察。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationView {
    pub id: ObservationId,
    /// 觀察到的對話發生的日期，`YYYY-MM-DD`。
    pub date: String,
    pub priority: Priority,
    pub text: String,
    /// 釘選的觀察在 Reflector 重寫時一定保留。
    pub pinned: bool,
    /// 你修正過。
    pub edited: bool,
}

/// 一個 project 對話 agent 目前的記憶區塊：反思後的長期觀察與之後追加的觀察，
/// 依時間排序。整份替換。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryView {
    /// Reflector 每重寫一次加一；舊世代保留作稽核，不送給模型。
    pub generation: u32,
    /// Observer 摘要的「目前在做什麼」；還沒有時是空字串。
    pub current_task: String,
    pub observations: Vec<ObservationView>,
    /// 記憶區塊的估計 token 數。
    pub tokens: u64,
    /// 上次整理記憶失敗的原因；成功後清掉。
    pub error: Option<String>,
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

/// Artifact 的一個版本（版本紀錄用）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRevisionView {
    pub rev: u32,
    /// 「你」「對話 agent」「Planner」這類顯示名稱。
    pub author: String,
    pub at: Timestamp,
    /// 和前一版比較的 unified diff；第一版是空字串。
    pub diff: String,
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
    /// Planner 回報的里程碑（包含 Goal 完成）。
    Milestone {
        goal: GoalId,
        text: String,
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
    /// listener 依外部事件自動開了一個 Goal。
    Triggered {
        listener: ListenerId,
        goal: GoalId,
        /// 例如「issue #118 加上 agent label」。
        event: String,
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
    /// worker 開的 PR，例如 `github.com/owner/repo#42`。
    #[serde(default)]
    pub pr: Option<String>,
    #[serde(default)]
    pub pr_url: Option<String>,
    /// worker 最後一句進度。
    #[serde(default)]
    pub progress: String,
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
    /// 問題或交接的對象（任務標題）；沒有就是給整個 Goal（問題是問 Planner）。
    #[serde(default)]
    pub to: Option<String>,
}

/// Goal 的狀態。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum GoalState {
    #[default]
    Active,
    Done {
        summary: String,
    },
    Cancelled {
        reason: String,
    },
}

impl GoalState {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Active => "進行中",
            Self::Done { .. } => "已完成",
            Self::Cancelled { .. } => "已取消",
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GoalView {
    pub id: GoalId,
    pub title: String,
    #[serde(default)]
    pub state: GoalState,
    pub planner_note: String,
    pub budget_used_pct: u8,
    pub tasks: Vec<TaskView>,
    pub team: Vec<TeamPost>,
    pub brief: String,
    /// 快速修改（`quick_fix`）：不經 Planner、只有一個任務。
    #[serde(default)]
    pub quick: bool,
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
    /// 下一次觸發（改時間用）；已結束的排程是 `None`。
    #[serde(default)]
    pub next_at: Option<Timestamp>,
    /// 週期排程的過期時間。
    #[serde(default)]
    pub expires_at: Option<Timestamp>,
    /// Planner 的排程所屬的 Goal。
    #[serde(default)]
    pub goal: Option<GoalId>,
    /// 單次排程已觸發、或週期排程已過期。
    #[serde(default)]
    pub finished: bool,
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
    /// agent 用 `create_listener` 提案，等你在自動化頁啟用（M5 之前以啟用代替核准）。
    Proposed,
    /// 超過每小時觸發上限而暫停；重新啟用後恢復。
    Throttled,
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
    /// 結構化的條件與動作（編輯用）。假資料沒有。
    #[serde(default)]
    pub spec: Option<ListenerSpec>,
}

/// listener 的條件與動作（`proposal/events-and-automation.md#listener事件自動開工`）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenerSpec {
    pub project: ProjectId,
    /// 要聽的事件種類，例如 `issue.labeled`、`ci.finished`（見 [`EVENT_KINDS`]）。
    pub kinds: Vec<String>,
    /// 資源的 glob，例如 `change:github.com/me/*`；`None` 是這個 project 監看的所有 repo。
    pub resource: Option<String>,
    /// 欄位條件，全部符合才觸發；也是 Public 來源自動開工時需要的白名單。
    pub fields: Vec<FieldMatch>,
    pub action: ListenerActionSpec,
    pub max_per_hour: u32,
    /// 每次觸發的預算（M6 才執行）。
    pub budget_per_trigger_cents: Option<u32>,
}

/// 事件欄位要是其中一個值，例如 `label` 是 `agent`、`branch` 是 `main`、`author` 是白名單裡的人。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldMatch {
    pub field: String,
    pub any_of: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ListenerActionSpec {
    /// 模板加事件內容 → Goal brief。
    CreateGoal { template: String },
    /// 小修改走快速通道；M4 之前開一個 quick 種類的 Goal。
    QuickFix { template: String },
    /// 只在 project 對話裡告訴對話 agent。
    NotifyChat,
}

impl ListenerActionSpec {
    pub fn label(&self) -> &'static str {
        match self {
            Self::CreateGoal { .. } => "開 Goal",
            Self::QuickFix { .. } => "快速修正",
            Self::NotifyChat => "通知對話",
        }
    }
}

/// listener 可以聽的事件種類與說明。
pub const EVENT_KINDS: &[(&str, &str)] = &[
    ("change.opened", "PR 開啟"),
    ("change.reviewed", "PR 收到 review"),
    ("change.commented", "PR 有新留言"),
    ("change.merged", "PR 已 merge"),
    ("issue.opened", "issue 開啟"),
    ("issue.labeled", "issue 加上 label"),
    ("issue.commented", "issue 有新留言"),
    ("ci.finished", "CI 結束"),
    ("feed.item", "feed 有新項目"),
    ("probe.failed", "探測失敗"),
    ("probe.recovered", "探測恢復"),
    ("status.incident.opened", "狀態頁 incident 開啟"),
    ("status.incident.updated", "狀態頁 incident 更新"),
    ("status.incident.resolved", "狀態頁 incident 解決"),
    ("status.component.changed", "狀態頁元件狀態變化"),
];

/// 批次暫停。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseView {
    /// `None` 表示所有 project（含全域排程）。
    pub project: Option<ProjectId>,
    /// 自動恢復的時間；`None` 表示要手動恢復。
    pub until: Option<Timestamp>,
    /// 例如「你」、「總管」。
    pub by: String,
    pub at: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceHealth {
    /// 還沒收過或輪詢過。
    Unknown,
    Ok,
    /// token 過期、連線失敗、連續輪詢失敗等；不會默默停止收事件。
    Error {
        message: String,
        since: Timestamp,
    },
}

/// 外部來源（connector 的一個實例）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceView {
    /// 例如 `github:personal`。
    pub id: String,
    /// 例如「GitHub」。
    pub kind: String,
    pub name: String,
    pub trust: TrustLevel,
    /// 例如「輪詢（每 60 秒）」。
    pub ingest: String,
    pub health: SourceHealth,
    pub last_ok: Option<Timestamp>,
    pub enabled: bool,
    /// 輪詢的 URL；webhook 沒有。
    #[serde(default)]
    pub url: Option<String>,
    /// 種類的識別碼（`rss`、`probe`、`statuspage`、`webhook`、`github`）。
    #[serde(default)]
    pub kind_id: String,
    /// 通用 webhook 的欄位對應；編輯時回填。
    #[serde(default)]
    pub mapping: WebhookMapping,
}

/// 自動化頁的所有資料；任一項有變就整筆替換（`ServerEvent::Automation`）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AutomationView {
    pub schedules: Vec<ScheduleView>,
    pub listeners: Vec<ListenerView>,
    pub pauses: Vec<PauseView>,
    pub sources: Vec<SourceView>,
    /// 通用 webhook 對應出來的自訂事件種類（listener 可以多選這些）。
    #[serde(default)]
    pub custom_kinds: Vec<String>,
}

/// 外部來源的種類（`sources.kind`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Rss,
    Probe,
    StatusPage,
    Webhook,
}

impl SourceKind {
    pub const ALL: &'static [SourceKind] = &[
        SourceKind::Rss,
        SourceKind::Probe,
        SourceKind::StatusPage,
        SourceKind::Webhook,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rss => "rss",
            Self::Probe => "probe",
            Self::StatusPage => "statuspage",
            Self::Webhook => "webhook",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_str() == s)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Rss => "RSS／Atom",
            Self::Probe => "HTTP 探測",
            Self::StatusPage => "狀態頁",
            Self::Webhook => "通用 webhook",
        }
    }

    /// 預設輪詢間隔（秒）；webhook 不輪詢。
    pub fn default_interval_secs(self) -> u32 {
        match self {
            Self::Rss => 300,
            Self::Probe => 60,
            Self::StatusPage => 60,
            Self::Webhook => 0,
        }
    }
}

/// 通用 webhook 的欄位對應：用 JSON pointer 從收到的 JSON 取出事件的各部分。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookMapping {
    /// 事件種類，例如 `/type`；取出的字串就是 listener 看到的名稱。
    pub kind: String,
    /// 資源鍵，例如 `/issue/id`；同一個資源的事件會回到同一個 Goal。
    pub resource: String,
    /// 平台產生、可信的摘要。
    pub summary: String,
    /// 去重用的欄位；空字串時用整個 body 的雜湊。
    #[serde(default)]
    pub dedupe: String,
    /// 他人寫的內容（不可信）；空字串表示沒有。
    #[serde(default)]
    pub body: String,
}

impl Default for WebhookMapping {
    fn default() -> Self {
        Self {
            kind: "/kind".into(),
            resource: "/resource".into(),
            summary: "/summary".into(),
            dedupe: "/id".into(),
            body: "/body".into(),
        }
    }
}

/// 儲存一個外部來源。新增時 `id` 是新的，編輯時是原本的 id。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceDraft {
    /// 例如 `rss:hn`，只能是英數字、`-`、`_`、`.`、`:`。
    pub id: String,
    pub kind: SourceKind,
    pub name: String,
    /// RSS、狀態頁、探測的 URL；webhook 沒有。
    #[serde(default)]
    pub url: Option<String>,
    /// 輪詢間隔（秒）；`None` 用種類的預設值，夾在 30 到 3600。
    #[serde(default)]
    pub interval_secs: Option<u32>,
    pub trust: TrustLevel,
    /// webhook 的 secret；`None` 表示沿用已存的，空字串表示清掉。
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default)]
    pub mapping: WebhookMapping,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// GitHub 連線設定（設定視窗的「GitHub」類別）。客戶端只看得到 token 的末四碼。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubView {
    /// 例如 `…1234`；沒有設定 token 時是 `None`。
    pub token_hint: Option<String>,
    /// API base URL，預設 `https://api.github.com`（GitHub Enterprise 是 `https://<host>/api/v3`）。
    pub api_base: String,
    /// 有沒有設定 webhook secret（`POST /webhooks/github` 驗簽用）。
    pub webhook_secret_set: bool,
    /// 最後一次驗證成功的帳號。
    pub login: Option<String>,
}

/// 儲存 GitHub 設定。`token`、`webhook_secret` 為 `None` 時保留已存的，空字串表示清掉。
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubDraft {
    pub token: Option<String>,
    pub api_base: Option<String>,
    pub webhook_secret: Option<String>,
}

impl std::fmt::Debug for GithubDraft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GithubDraft")
            .field("token", &self.token.as_ref().map(|_| "***"))
            .field("api_base", &self.api_base)
            .field(
                "webhook_secret",
                &self.webhook_secret.as_ref().map(|_| "***"),
            )
            .finish()
    }
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
    OpenSettings,
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
            Self::OpenSettings => "開啟設定",
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
    /// 「需要你處理」的任務（重試、取消用）。
    #[serde(default)]
    pub task: Option<TaskId>,
    /// 服務的核准（live 模式）；假資料沒有。
    #[serde(default)]
    pub approval: Option<ApprovalView>,
}

impl InboxItem {
    pub fn is_open(&self) -> bool {
        self.decision == Decision::Pending
    }
}

/// 平台 merge PR 的方式（GitHub 的 `merge_method`）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeMethod {
    Merge,
    #[default]
    Squash,
    Rebase,
}

impl MergeMethod {
    pub const ALL: [Self; 3] = [Self::Squash, Self::Merge, Self::Rebase];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Squash => "squash",
            Self::Rebase => "rebase",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Merge => "merge commit",
            Self::Squash => "squash",
            Self::Rebase => "rebase",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == s)
    }
}

/// 一筆核准現在怎樣了。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalState {
    /// 等你決定。
    Pending,
    /// 你核准了，平台正在執行（例如正在 merge）。
    Executing,
    /// 已核准並執行完成。
    Executed,
    Rejected,
    /// 逾時沒有決定。
    Expired,
    /// 核准了，但執行失敗（`ApprovalView::result` 是原因）。
    Failed,
    /// 不再需要：任務被取消、listener 被刪掉等。
    Cancelled,
}

impl ApprovalState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "等你核准",
            Self::Executing => "已核准 · 執行中",
            Self::Executed => "已核准",
            Self::Rejected => "已拒絕",
            Self::Expired => "已逾時",
            Self::Failed => "執行失敗",
            Self::Cancelled => "已撤回",
        }
    }
}

/// 要核准的具體動作（顯示用）。平台只執行 `action_hash` 和核准時一致的動作。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalActionView {
    MergePr {
        /// `github.com/owner/repo#42`
        pr: String,
        url: Option<String>,
        /// 核准的 head commit；PR 之後有新的 commit 就不會 merge。
        sha: String,
        method: MergeMethod,
        branch: String,
        base: String,
    },
    DeleteBranch {
        repo: String,
        branch: String,
    },
    CreateListener {
        listener: ListenerId,
        /// 例如「issue 加上 label（label：agent）→ 開 Goal」。
        summary: String,
    },
    EditUserMd {
        base_rev: u32,
        /// 和提案時的內容比較的 unified diff。
        diff: String,
    },
    EditProjectNotes {
        /// 還沒有 Project Notes 時是 `None`（核准後建立）。
        artifact: Option<ArtifactId>,
        base_rev: u32,
        diff: String,
    },
    /// Goal 的預算用完了，核准就追加 `tokens`。
    IncreaseBudget {
        goal: GoalId,
        tokens: u64,
    },
    /// project 的每小時用量超過上限而熔斷，核准就恢復所有 run。
    ResumeProject {
        project: ProjectId,
    },
}

/// 一筆核准（`proposal/approvals-and-notifications.md`）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalView {
    pub id: ApprovalId,
    pub project: ProjectId,
    pub goal: Option<GoalId>,
    /// 等這筆核准的任務（merge PR）。
    pub task: Option<TaskId>,
    /// 例如「merge PR #42：Add hello」。
    pub title: String,
    /// agent 給的理由。
    pub reason: String,
    /// 誰申請的，例如「worker · 內建」「Planner」「對話 agent」。
    pub requested_by: String,
    pub action: ApprovalActionView,
    /// 動作內容的 SHA-256（前 12 碼）。
    pub action_hash: String,
    pub state: ApprovalState,
    /// 你決定時附的理由。
    pub note: Option<String>,
    /// 執行結果或失敗原因。
    pub result: Option<String>,
    /// 這筆核准要不要推播（merge 後自動開的「刪除分支」不推）。
    pub notify: bool,
    pub created_at: Timestamp,
    pub expires_at: Timestamp,
    pub decided_at: Option<Timestamp>,
}

impl ApprovalView {
    pub fn is_pending(&self) -> bool {
        self.state == ApprovalState::Pending
    }
}

/// 桌面通知（設定的「通知」類別）。存在實例裡，由客戶端發出。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationSettings {
    pub enabled: bool,
    /// 靜音時段：期間不彈通知，結束時合併成一則。
    pub quiet: bool,
    /// 本機時間 `HH:MM`；開始比結束晚表示跨午夜。
    pub quiet_start: String,
    pub quiet_end: String,
}

impl Default for NotificationSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            quiet: false,
            quiet_start: "22:00".into(),
            quiet_end: "08:00".into(),
        }
    }
}

/// `HH:MM` → 一天裡的第幾分鐘。
pub fn parse_hh_mm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    if h.is_empty() || h.len() > 2 || m.len() != 2 {
        return None;
    }
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

impl NotificationSettings {
    /// 一天裡的第 `minute` 分鐘是不是靜音時段（開始含、結束不含）。
    pub fn is_quiet_at(&self, minute: u32) -> bool {
        if !self.quiet {
            return false;
        }
        let (Some(start), Some(end)) =
            (parse_hh_mm(&self.quiet_start), parse_hh_mm(&self.quiet_end))
        else {
            return false;
        };
        match start.cmp(&end) {
            std::cmp::Ordering::Equal => false,
            std::cmp::Ordering::Less => (start..end).contains(&minute),
            std::cmp::Ordering::Greater => minute >= start || minute < end,
        }
    }
}

/// 跨 project 的使用者偏好（`proposal/memory.md#usermd跨-project-的使用者偏好`）。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserMdView {
    /// 還沒有內容時是 0。
    pub rev: u32,
    pub body: String,
}

/// USER.md 最多幾個字；超過 [`USER_MD_HINT_CHARS`] 時在畫面上提示整理。
pub const MAX_USER_MD_CHARS: usize = 20_000;
pub const USER_MD_HINT_CHARS: usize = 4_000;

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

impl Default for OverseerView {
    fn default() -> Self {
        Self {
            state: AgentState::Idle,
            items: Vec::new(),
            overview: Vec::new(),
            memory: Vec::new(),
            memory_total: 0,
        }
    }
}

/// 「為什麼」面板的一步（一次模型呼叫或一次工具呼叫）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceStepView {
    /// `model` 或 `tool`。
    pub kind: String,
    pub name: String,
    pub at: Timestamp,
    pub ok: bool,
    /// 遮蔽、截短後的參數與結果。
    pub detail: String,
}

/// 成本報表的分類：主要工作或 Observational Memory 的開銷。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CostCategory {
    Work,
    Om,
}

impl CostCategory {
    pub fn label(self) -> &'static str {
        match self {
            Self::Work => "主要工作",
            Self::Om => "記憶整理",
        }
    }
}

/// 費用屬於哪一種帳：訂閱用量、API 金額或 CLI 回報的估計。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Billing {
    Subscription,
    Api,
    Cli,
    /// 舊的 run 沒有記錄 provider。
    Unknown,
}

impl Billing {
    pub fn label(self) -> &'static str {
        match self {
            Self::Subscription => "訂閱",
            Self::Api => "API",
            Self::Cli => "CLI",
            Self::Unknown => "未知",
        }
    }
}

/// 成本報表的一列：同一個 project、Goal、agent、provider／runtime 與模型的彙總。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CostRow {
    pub project: ProjectId,
    pub project_name: String,
    pub goal: Option<GoalId>,
    pub goal_title: Option<String>,
    /// `chat`、`planner`、`worker`、`observer`、`reflector` 等。
    pub agent: String,
    pub category: CostCategory,
    pub provider: String,
    pub billing: Billing,
    pub runtime: String,
    pub model: String,
    pub runs: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// 已結束的 run 是實際時間；還在跑的是到報表產生為止。
    pub duration_secs: u64,
    /// CLI 回報的費用（Claude Code 的 `total_cost_usd`）；不是帳單。
    pub reported_cost_usd: Option<f64>,
    /// 依單價表算出的估計金額；沒有單價或訂閱用量時是 `None`。
    pub estimated_cost_usd: Option<f64>,
    /// 有單價可算的 token 數與沒有單價的 token 數。
    pub priced_tokens: u64,
    pub unpriced_tokens: u64,
}

/// 成本報表的總計。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CostTotals {
    pub runs: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub duration_secs: u64,
    pub reported_cost_usd: f64,
    pub estimated_cost_usd: f64,
    pub priced_tokens: u64,
    pub unpriced_tokens: u64,
    /// 依計費方式分開的 token（訂閱用量與 API 金額分帳）。
    pub subscription_tokens: u64,
    pub api_tokens: u64,
    pub cli_tokens: u64,
    /// OM 與主要工作分開。
    pub om_tokens: u64,
    pub work_tokens: u64,
}

/// 單價表的摘要（明細用 `SetPrice`／`DeletePrice` 維護）。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PricingInfo {
    /// `models.dev` 或 `manual`；空表示還沒有任何單價。
    pub source: String,
    pub models: u64,
    pub updated_at: Option<Timestamp>,
}

/// 一筆單價：每 100 萬 token 的美元單價。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelPrice {
    /// models.dev 的 provider 鍵，例如 `openai`、`anthropic`；`*` 是所有 provider 的後備值。
    pub provider: String,
    pub model: String,
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    pub cache_read_per_mtok: Option<f64>,
}

/// 成本報表。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CostReport {
    pub since: Timestamp,
    pub until: Timestamp,
    pub generated_at: Timestamp,
    pub rows: Vec<CostRow>,
    pub totals: CostTotals,
    pub pricing: PricingInfo,
}

/// 一則 agent 訊息為什麼這樣回答。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhyView {
    pub item: ItemId,
    pub agent: String,
    pub model: Option<String>,
    /// 觸發來源（使用者訊息、排程、外部事件等）的摘要。
    pub trigger: String,
    /// run 開始時的記憶版本；沒有記憶的 profile 是 `None`。
    pub memory_revision: Option<i64>,
    pub started_at: Timestamp,
    pub finished_at: Option<Timestamp>,
    /// `budget`、`breaker` 或 `None`。
    pub stop_reason: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub steps: Vec<TraceStepView>,
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
    pub providers: ProvidersView,
    #[serde(default)]
    pub automation: AutomationView,
    #[serde(default)]
    pub github: GithubView,
    #[serde(default)]
    pub worker: WorkerSettingsView,
    /// 等你決定的核准，加上最近決定過的（新的在後）。
    #[serde(default)]
    pub approvals: Vec<ApprovalView>,
    #[serde(default)]
    pub notifications: NotificationSettings,
    #[serde(default)]
    pub user_md: UserMdView,
    /// 總管的全域對話、專案概況與記憶。
    #[serde(default)]
    pub overseer: OverseerView,
}

// ---------------------------------------------------------------------------
// Coding worker 設定（proposal/agent-runtime.md#worker-adapter內建-worker-與外部-coding-cli）

/// worker 的後端。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkerBackendKind {
    /// 內建 worker：Pots 自己的 loop，用對話 agent 使用中的模型 provider。
    #[default]
    Builtin,
    /// 未修改的官方 Claude Code CLI，由你自己官方登入。
    ClaudeCode,
}

impl WorkerBackendKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Builtin => "內建 worker",
            Self::ClaudeCode => "Claude Code",
        }
    }
}

/// worker 在哪裡執行。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecKind {
    /// 每次 run 一個 Docker／Podman 容器，只掛任務的工作目錄與 Claude Code 的專屬 home。
    #[default]
    Container,
    /// 直接在這台電腦上執行（沒有沙箱）。
    Local,
}

impl ExecKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Container => "容器",
            Self::Local => "本機（沒有沙箱）",
        }
    }
}

/// worker 設定。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerSettingsView {
    pub backend: WorkerBackendKind,
    pub exec: ExecKind,
    /// 容器執行檔，例如 `docker` 或 `podman`。
    pub runtime: String,
    /// 容器 image（repo 附的 `docker/worker/Dockerfile`）。
    pub image: String,
    /// Claude Code 用的模型；空字串是 CLI 的預設。
    pub claude_model: String,
    /// 登入 Claude Code 要在終端機跑的指令（依目前的設定產生）。
    pub login_command: String,
    /// 同時最多幾個 worker。
    pub max_parallel: u8,
}

/// 修改 worker 設定時客戶端送出的內容。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerSettingsDraft {
    pub backend: WorkerBackendKind,
    pub exec: ExecKind,
    pub runtime: String,
    pub image: String,
    pub claude_model: String,
    pub max_parallel: u8,
}

// ---------------------------------------------------------------------------
// 模型 provider 設定

/// 模型 provider 的種類。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProviderKind {
    /// ChatGPT 方案（SIWC 授權），內建一筆，ID 固定為 [`ProviderId::CHATGPT`]。
    ChatGpt,
    /// OpenAI 相容的 Chat Completions API（`POST {base_url}/chat/completions`）。
    OpenAiCompatible,
    /// Anthropic 相容的 Messages API（`POST {base_url}/v1/messages`）。
    AnthropicCompatible,
}

impl ProviderKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::ChatGpt => "ChatGPT 方案",
            Self::OpenAiCompatible => "OpenAI 相容 API",
            Self::AnthropicCompatible => "Anthropic 相容 API",
        }
    }

    /// 新增時預填的 base URL。
    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::ChatGpt => "",
            Self::OpenAiCompatible => "https://api.openai.com/v1",
            Self::AnthropicCompatible => "https://api.anthropic.com",
        }
    }
}

impl ProviderId {
    /// 內建的 ChatGPT 方案 provider。
    pub const CHATGPT: Self = Self::from_u64(1);
}

/// 一個 provider 的設定。不含 API key。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderView {
    pub id: ProviderId,
    pub kind: ProviderKind,
    pub name: String,
    /// ChatGPT 是空字串。
    pub base_url: String,
    /// 對話 agent 用的模型 ID；ChatGPT 為空字串時自動選帳號清單的第一個。
    pub model: String,
    /// API key 的末四碼，例如 `…a1b2`；沒有 key 是 `None`。
    pub key_hint: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvidersView {
    /// 目前對話用的 provider；`None` 表示還沒設定。
    pub active: Option<ProviderId>,
    /// 內建的 ChatGPT 一定在第一筆。
    pub items: Vec<ProviderView>,
    /// 整理記憶（Observer／Reflector）用的模型。
    #[serde(default)]
    pub memory: MemoryModelView,
}

/// 整理記憶用的 provider 與模型，和對話 agent 分開設定。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryModelView {
    /// `None`：跟對話 agent 用同一個 provider 與模型。
    pub provider: Option<ProviderId>,
    /// 空字串：用該 provider 設定的模型。
    pub model: String,
}

impl ProvidersView {
    pub fn get(&self, id: ProviderId) -> Option<&ProviderView> {
        self.items.iter().find(|p| p.id == id)
    }

    pub fn active(&self) -> Option<&ProviderView> {
        self.active.and_then(|id| self.get(id))
    }
}

/// 新增或修改 provider 時客戶端送出的內容。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderDraft {
    /// `None` 是新增。
    pub id: Option<ProviderId>,
    pub kind: ProviderKind,
    pub name: String,
    pub base_url: String,
    pub model: String,
    /// `None` 表示沿用已存的 key；新增時必填（ChatGPT 除外）。
    pub api_key: Option<String>,
}

impl std::fmt::Debug for ProviderDraft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderDraft")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("name", &self.name)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &self.api_key.as_ref().map(|_| ".."))
            .finish()
    }
}

/// provider 可用的模型。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelOption {
    pub id: String,
    pub display_name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectSnapshot {
    pub id: ProjectId,
    pub name: String,
    pub agent_name: String,
    pub agent_state: AgentState,
    /// 最近的對話；更早的走 HTTP `GET /projects/:id/items?before=<id>`。
    pub items: Vec<StreamItemView>,
    #[serde(default)]
    pub memory: MemoryView,
    /// 所有 Goal（含任務樹），依建立時間。
    #[serde(default)]
    pub goals: Vec<GoalView>,
    /// 所有 artifact，依建立時間。
    #[serde(default)]
    pub artifacts: Vec<ArtifactView>,
    /// 監看的 repo，例如 `github.com/owner/repo`。
    #[serde(default)]
    pub repos: Vec<String>,
    #[serde(default)]
    pub merge_method: MergeMethod,
}

/// 模型 provider（ChatGPT Plan，經 SIWC 授權）的狀態。不含任何 token；`Debug` 不印出
/// 登入網址（可能帶 `id_token_hint`）。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
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

impl std::fmt::Debug for ModelAuthView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SignedOut => f.write_str("SignedOut"),
            Self::Pending { .. } => f.write_str("Pending { url: .. }"),
            Self::SignedIn { account } => f
                .debug_struct("SignedIn")
                .field("account", account)
                .finish(),
            Self::Error { message } => f.debug_struct("Error").field("message", message).finish(),
        }
    }
}

/// 伺服器推送的事件。`seq` 每個實例各自遞增，寫進事件日誌，重連時依序補送；
/// 不帶 `seq` 的事件只對當下的連線有意義，不補送。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum ServerEvent {
    Snapshot {
        seq: u64,
        instance: Box<InstanceSnapshot>,
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
    /// 「為什麼」面板的內容；只回給問的連線，不記日誌。
    Why {
        why: Option<WhyView>,
    },
    /// 成本報表；只回給問的連線，不記日誌。
    CostReport {
        report: CostReport,
    },
    /// 單價表的摘要；只回給問的連線，不記日誌。
    Pricing {
        info: PricingInfo,
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
    /// 核准新增或有變（決定、執行結果、逾時、撤回），整筆替換。
    ApprovalChanged {
        seq: u64,
        approval: Box<ApprovalView>,
    },
    /// 通知設定有變。
    Notifications {
        seq: u64,
        settings: NotificationSettings,
    },
    /// USER.md 有變（你的編輯或核准了 agent 的提案）。
    UserMd {
        seq: u64,
        user_md: UserMdView,
    },
    /// project 的預設 merge 方式有變。
    ProjectMergeMethod {
        seq: u64,
        project: ProjectId,
        method: MergeMethod,
    },
    ModelAuth {
        seq: u64,
        state: ModelAuthView,
    },
    /// provider 設定或使用中的 provider 有變，整份替換。
    Providers {
        seq: u64,
        providers: ProvidersView,
    },
    /// project 的記憶有變（Observer、Reflector 或你的修正），整份替換。
    Memory {
        seq: u64,
        project: ProjectId,
        memory: MemoryView,
    },
    /// Goal 新增或有變（狀態、任務樹、Brief、Planner 備註），整筆替換。
    GoalChanged {
        seq: u64,
        project: ProjectId,
        goal: GoalView,
    },
    /// Goal 被封存：從 project 列表移除；成本報表仍算得到。
    GoalArchived {
        seq: u64,
        project: ProjectId,
        goal: GoalId,
    },
    /// Artifact 新增或有變，整筆替換。
    ArtifactChanged {
        seq: u64,
        project: ProjectId,
        artifact: ArtifactView,
    },
    /// 回給送出 `ArtifactHistory` 的那條連線：最近的版本（最多 50 版），由新到舊。不寫入事件日誌。
    ArtifactHistory {
        artifact: ArtifactId,
        revisions: Vec<ArtifactRevisionView>,
    },
    /// 回給送出 `TestProvider` 的那條連線：成功時是可用的模型清單。不寫入事件日誌。
    ProviderTest {
        /// 對應 `TestProvider::request`。
        request: u64,
        result: Result<Vec<ModelOption>, String>,
    },
    /// 排程、listener、暫停或來源健康狀態有變，整筆替換。
    Automation {
        seq: u64,
        automation: AutomationView,
    },
    /// GitHub 設定有變。
    Github {
        seq: u64,
        github: GithubView,
    },
    /// project 監看的 repo 有變。
    ProjectRepos {
        seq: u64,
        project: ProjectId,
        repos: Vec<String>,
    },
    /// worker 設定有變。
    Worker {
        seq: u64,
        worker: WorkerSettingsView,
    },
    /// 回給送出 `TestGithub` 的那條連線：成功時是帳號名稱。不寫入事件日誌。
    GithubTest {
        request: u64,
        result: Result<String, String>,
    },
    /// 回給按下「開始登入」的那條連線：要開啟的登入網址。不寫入事件日誌（網址可能
    /// 帶 `id_token_hint`），也不廣播給其他連線。
    LoginUrl {
        url: String,
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
            | Self::ApprovalChanged { seq, .. }
            | Self::Notifications { seq, .. }
            | Self::UserMd { seq, .. }
            | Self::ProjectMergeMethod { seq, .. }
            | Self::ModelAuth { seq, .. }
            | Self::Providers { seq, .. }
            | Self::Memory { seq, .. }
            | Self::GoalChanged { seq, .. }
            | Self::GoalArchived { seq, .. }
            | Self::ArtifactChanged { seq, .. }
            | Self::Automation { seq, .. }
            | Self::Github { seq, .. }
            | Self::ProjectRepos { seq, .. }
            | Self::Worker { seq, .. } => Some(*seq),
            Self::GithubTest { .. }
            | Self::Delta { .. }
            | Self::StreamingText { .. }
            | Self::ArtifactHistory { .. }
            | Self::LoginUrl { .. }
            | Self::ProviderTest { .. }
            | Self::Why { .. }
            | Self::CostReport { .. }
            | Self::Pricing { .. }
            | Self::Error { .. } => None,
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
    /// 對總管說話；`id` 的去重與 `Send` 一樣。
    SendOverseer {
        id: ItemId,
        text: String,
    },
    /// 停止總管的 run。
    StopOverseer,
    /// 看到這個 project 的對話了；伺服器據此判斷要不要插入「你不在期間」摘要。
    MarkRead {
        project: ProjectId,
    },
    /// 問一則 agent 訊息為什麼這樣回答（觸發來源、記憶版本、工具順序）。
    Why {
        item: ItemId,
    },
    /// 成本報表：依 project、Goal、agent、provider／runtime 彙總；`project` 不給就是全部。
    CostReport {
        project: Option<ProjectId>,
        since: Option<Timestamp>,
        until: Option<Timestamp>,
    },
    /// 新增或覆寫一筆單價。
    SetPrice {
        price: ModelPrice,
    },
    /// 刪除一筆單價。
    DeletePrice {
        provider: String,
        model: String,
    },
    /// 從 models.dev 重新抓單價表（不快取整份 JSON，只留模型的價錢）。
    RefreshPricing,
    /// 單價表的摘要。
    Pricing,
    /// 核准或拒絕（可以附一句理由，拒絕的理由會帶給 agent）。伺服器回 `ApprovalChanged`；
    /// 已經決定過、逾時時回 `Error`。
    Decide {
        approval: ApprovalId,
        approve: bool,
        note: Option<String>,
    },
    Cancel {
        task: TaskId,
    },
    /// 重試一個需要決定（或失敗）的任務：回到佇列，worker 從工作目錄的現況接著做。
    RetryTask {
        task: TaskId,
    },
    /// 開始 ChatGPT（SIWC）登入；伺服器回 `ModelAuth { Pending { url } }`。
    StartModelLogin,
    CancelModelLogin,
    ModelLogout,
    /// 新增或修改 provider；伺服器回 `Providers`。
    SaveProvider {
        draft: ProviderDraft,
    },
    /// 刪除 provider（內建的 ChatGPT 不能刪）。刪掉使用中的那個時，`active` 變成 `None`。
    DeleteProvider {
        id: ProviderId,
    },
    SetActiveProvider {
        id: ProviderId,
    },
    /// 用這份設定列出模型，驗證 base URL 與 key；不儲存。`draft.api_key` 為 `None` 且
    /// `draft.id` 有值時用已存的 key。伺服器回 `ProviderTest`。
    TestProvider {
        request: u64,
        draft: ProviderDraft,
    },
    /// 設定整理記憶用的 provider 與模型；伺服器回 `Providers`。
    SetMemoryModel {
        /// `None`：跟對話 agent 相同。
        provider: Option<ProviderId>,
        model: String,
    },
    /// 釘選或取消釘選一條觀察。有變動時伺服器回 `Memory`；被拒絕時回 `Error`，並重送一次
    /// 目前的 `Memory`（下面兩個指令也一樣）。
    PinObservation {
        project: ProjectId,
        id: ObservationId,
        pinned: bool,
    },
    /// 修正一條觀察的文字（連續的空白合成一個空格，最多 [`MAX_OBSERVATION_CHARS`] 字）。
    /// 原本的內容保留作稽核，模型只看到修正後的。
    EditObservation {
        project: ProjectId,
        id: ObservationId,
        text: String,
    },
    /// 刪除一條觀察（保留作稽核，不再送給模型）。
    DeleteObservation {
        project: ProjectId,
        id: ObservationId,
    },
    /// 儲存你對 artifact 的修改。`base_rev` 是你開始編輯時的版本；期間 agent 改過的話，
    /// 伺服器以 `## ` 區段合併，同一段兩邊都改過就拒絕（回 `Error`，你的草稿留在客戶端）。
    /// 成功時伺服器回 `ArtifactChanged`；改了 Goal 的 Brief 會叫醒 Planner。
    SaveArtifact {
        artifact: ArtifactId,
        base_rev: u32,
        body: String,
    },
    /// 列出 artifact 最近的版本；伺服器回 `ArtifactHistory`。
    ArtifactHistory {
        artifact: ArtifactId,
    },
    /// 把 artifact 還原成某一版的內容（新增一個版本，不刪除歷史）。
    RestoreArtifact {
        artifact: ArtifactId,
        rev: u32,
    },
    /// 儲存 GitHub 設定；伺服器回 `Github`。
    SaveGithub {
        draft: GithubDraft,
    },
    /// 新增或修改一個外部來源（RSS、探測、狀態頁、通用 webhook）；伺服器回 `Automation`。
    SaveSource {
        draft: SourceDraft,
    },
    /// 刪除一個外部來源（GitHub 不能刪）；伺服器回 `Automation`。
    DeleteSource {
        id: String,
    },
    /// 啟用或停用一個外部來源；伺服器回 `Automation`。
    SetSourceEnabled {
        id: String,
        enabled: bool,
    },
    /// 用這份 token（`None` 時用已存的）查詢帳號；不儲存。伺服器回 `GithubTest`。
    TestGithub {
        request: u64,
        draft: GithubDraft,
    },
    /// 設定 project 監看的 repo（`owner/repo` 或 `github.com/owner/repo`）；伺服器回 `ProjectRepos`。
    SetProjectRepos {
        project: ProjectId,
        repos: Vec<String>,
    },
    /// 新增（`id` 為 `None`）或修改 listener。你建立的 listener 直接生效。
    SaveListener {
        id: Option<ListenerId>,
        spec: ListenerSpec,
    },
    /// 啟用或停用 listener；啟用 agent 提案的 listener 就是核准它。
    SetListenerEnabled {
        id: ListenerId,
        enabled: bool,
    },
    DeleteListener {
        id: ListenerId,
    },
    SetScheduleEnabled {
        id: ScheduleId,
        enabled: bool,
    },
    /// 改下一次觸發的時間。
    RescheduleSchedule {
        id: ScheduleId,
        next_at: Timestamp,
    },
    /// 設定週期排程的過期時間（延長）。
    SetScheduleExpiry {
        id: ScheduleId,
        expires_at: Timestamp,
    },
    DeleteSchedule {
        id: ScheduleId,
    },
    /// 暫停一個 project（`None` 是所有 project）的排程與 listener；暫停期間錯過的觸發不補跑。
    PauseAutomations {
        project: Option<ProjectId>,
        until: Option<Timestamp>,
    },
    ResumeAutomations {
        project: Option<ProjectId>,
    },
    /// 儲存 worker 設定；伺服器回 `Worker`。只影響之後開始的 run。
    SaveWorkerSettings {
        draft: WorkerSettingsDraft,
    },
    /// 儲存通知設定；伺服器回 `Notifications`。
    SaveNotifications {
        settings: NotificationSettings,
    },
    /// 儲存 USER.md。`base_rev` 是開始編輯時的版本；期間被改過（例如核准了 agent 的提案）時
    /// 回 `Error`。成功時回 `UserMd`。
    SaveUserMd {
        base_rev: u32,
        body: String,
    },
    /// 設定 project 的預設 merge 方式；伺服器回 `ProjectMergeMethod`。
    SetMergeMethod {
        project: ProjectId,
        method: MergeMethod,
    },
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
    fn quiet_hours() {
        assert_eq!(parse_hh_mm("08:05"), Some(485));
        assert_eq!(parse_hh_mm("8:05"), Some(485));
        for bad in ["24:00", "12:60", "1200", "12:5", "", "a:bc"] {
            assert_eq!(parse_hh_mm(bad), None, "{bad}");
        }
        let mut s = NotificationSettings::default();
        assert!(!s.is_quiet_at(23 * 60), "預設沒開");
        s.quiet = true;
        // 22:00–08:00 跨午夜。
        assert!(s.is_quiet_at(22 * 60));
        assert!(s.is_quiet_at(3 * 60));
        assert!(!s.is_quiet_at(8 * 60));
        assert!(!s.is_quiet_at(12 * 60));
        s.quiet_start = "13:00".into();
        s.quiet_end = "14:00".into();
        assert!(s.is_quiet_at(13 * 60 + 30));
        assert!(!s.is_quiet_at(14 * 60));
        s.quiet_end = "13:00".into();
        assert!(!s.is_quiet_at(13 * 60), "開始等於結束視為沒有靜音");
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
