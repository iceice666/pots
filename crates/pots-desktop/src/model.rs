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
    InstanceId, ProjectId, ItemId, GoalId, TaskId, ApprovalId, ArtifactId,
);

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
    pub schedules: AutomationSummary,
    pub listeners: AutomationSummary,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct AutomationSummary {
    pub active: u32,
    pub paused: bool,
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
    pub artifact_count: u32,
    pub automation_count: u32,
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
        actions: Vec<String>,
    },
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
