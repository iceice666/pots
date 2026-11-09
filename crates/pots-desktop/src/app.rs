//! 根視圖：持有 [`Store`] 與所有 UI 狀態，各區塊的 render 拆在 `views/`。

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt as _;

use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, KeyBinding, ParentElement as _, Render, ScrollHandle, Styled as _, Subscription,
    Window, div,
};

use markdown_editor::{BlockEditorEvent, BlockEditorState};

use crate::connection::{self, ConnEvent, Connection, Endpoint};
use crate::model::*;
use crate::notify::{self, Notifier};
use crate::sync::{self, Notice, Sync};
use crate::theme;

gpui_kit::actions!(pots, [OpenOverseer, CloseSettings]);

/// 在 `gpui_kit::init` 之後呼叫：註冊 Artifact 編輯器與 app 的快捷鍵。
pub fn init(cx: &mut App) {
    markdown_editor::init(cx);
    cx.bind_keys([
        // macOS 用 cmd，其他平台用 ctrl。
        KeyBinding::new(
            if cfg!(target_os = "macos") {
                "cmd-shift-o"
            } else {
                "ctrl-shift-o"
            },
            OpenOverseer,
            None,
        ),
        // 設定視窗開著時關掉它；沒開時讓 Esc 照常往外傳（例如輸入框）。
        KeyBinding::new("escape", CloseSettings, Some("PotsApp")),
    ]);
}

/// 中欄顯示什麼。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    /// `None` 是合併所有實例的全域 Inbox。
    Inbox(Option<InstanceId>),
    Project(ProjectId),
    /// 目前實例的總管對話。
    Overseer,
    /// 目前實例的排程與 listener。
    Automations,
}

/// 設定視窗左側的類別。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsTab {
    /// 模型 provider：ChatGPT 方案與 API provider。
    Providers,
    /// Observational Memory：整理記憶用的模型。
    Memory,
    /// GitHub 連線：token、API base URL、webhook secret。
    Github,
    /// Coding worker：後端、執行環境、同時執行數，Claude Code 的登入指令。
    Worker,
    /// 桌面通知與靜音時段。
    Notifications,
    /// 跨 project 的使用者偏好（USER.md）。
    UserMd,
    /// 版本與目前實例的資訊。
    About,
}

impl SettingsTab {
    pub const ALL: [Self; 7] = [
        Self::Providers,
        Self::Memory,
        Self::Github,
        Self::Worker,
        Self::Notifications,
        Self::UserMd,
        Self::About,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Providers => "模型 Provider",
            Self::Memory => "記憶",
            Self::Github => "GitHub",
            Self::Worker => "Worker",
            Self::Notifications => "通知",
            Self::UserMd => "USER.md",
            Self::About => "關於",
        }
    }
}

/// 設定視窗「GitHub」類別正在改的連線設定。秘密欄位留空表示沿用已存的。
pub struct GithubEditor {
    pub token: Entity<InputState>,
    pub api_base: Entity<InputState>,
    pub webhook_secret: Entity<InputState>,
    pub test: GithubTest,
    pub saving: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GithubTest {
    Idle,
    Running(u64),
    Ok(String),
    Err(String),
}

/// 設定視窗「Worker」類別正在改的設定。
pub struct WorkerEditor {
    pub backend: WorkerBackendKind,
    pub exec: ExecKind,
    pub runtime: Entity<InputState>,
    pub image: Entity<InputState>,
    /// 留空用 Claude Code CLI 的預設模型。
    pub claude_model: Entity<InputState>,
    pub max_parallel: u8,
    /// 已送出 `SaveWorkerSettings`、等服務回 `Worker`。
    pub saving: bool,
    pub error: Option<String>,
}

/// 設定視窗「通知」類別正在改的設定。
pub struct NotificationEditor {
    pub enabled: bool,
    pub quiet: bool,
    pub start: Entity<InputState>,
    pub end: Entity<InputState>,
    pub saving: bool,
    pub error: Option<String>,
}

/// 設定視窗「USER.md」類別正在改的內容。
pub struct UserMdEditor {
    pub input: Entity<TextareaState>,
    /// 開始編輯時的版本（儲存時當 `base_rev`）。
    pub base_rev: u32,
    pub saving: bool,
    pub error: Option<String>,
}

/// 拒絕核准時填理由的對話框。
pub struct RejectDialog {
    pub id: ApprovalId,
    pub input: Entity<InputState>,
    _subscription: Subscription,
}

/// 右欄「新增 listener」表單。
pub struct ListenerForm {
    pub project: Option<ProjectId>,
    /// 選了哪些事件種類（`EVENT_KINDS` 或通用 webhook 的自訂種類）。
    pub kinds: Vec<String>,
    pub action: ListenerActionKind,
    pub label: Entity<InputState>,
    pub author: Entity<InputState>,
    pub branch: Entity<InputState>,
    pub conclusion: Entity<InputState>,
    pub template: Entity<InputState>,
    pub max_per_hour: Entity<InputState>,
    pub saving: bool,
    /// 送出時已有幾個 listener；多出一個才算建立成功。
    pub listeners_before: usize,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListenerActionKind {
    CreateGoal,
    QuickFix,
    NotifyChat,
}

/// 自動化頁正在新增或修改的外部來源。
pub struct SourceEditor {
    /// 修改時的原本 id；新增時是 `None`。
    pub editing: Option<String>,
    pub kind: SourceKind,
    pub trust: TrustLevel,
    pub id: Entity<InputState>,
    pub name: Entity<InputState>,
    pub url: Entity<InputState>,
    /// 輪詢間隔（秒）；留空用種類的預設值。
    pub interval: Entity<InputState>,
    /// webhook 的 secret；修改時留空表示沿用已存的。
    pub secret: Entity<InputState>,
    pub map_kind: Entity<InputState>,
    pub map_resource: Entity<InputState>,
    pub map_summary: Entity<InputState>,
    pub map_dedupe: Entity<InputState>,
    pub map_body: Entity<InputState>,
    pub enabled: bool,
    pub saving: bool,
    pub error: Option<String>,
}

/// 頻道「自動化」分頁正在改的監看 repo。
pub struct RepoEditor {
    pub project: ProjectId,
    pub input: Entity<InputState>,
    pub saving: bool,
    pub error: Option<String>,
    _subscription: Subscription,
}

/// 設定視窗「記憶」類別正在改的整理記憶模型。
pub struct MemoryModelEditor {
    /// `None`：跟對話 agent 相同。
    pub provider: Option<ProviderId>,
    /// 留空用該 provider 設定的模型。
    pub model: Entity<InputState>,
    /// 已送出 `SetMemoryModel`、等服務回 `Providers`。
    pub saving: bool,
    pub error: Option<String>,
}

/// 「記憶」分頁正在修正的觀察。
pub struct ObservationEditor {
    pub project: ProjectId,
    pub id: ObservationId,
    pub input: Entity<InputState>,
    _subscription: Subscription,
}

impl RepoEditor {
    pub(crate) fn new(
        project: ProjectId,
        input: Entity<InputState>,
        subscription: Subscription,
    ) -> Self {
        Self {
            project,
            input,
            saving: false,
            error: None,
            _subscription: subscription,
        }
    }
}

/// 設定頁正在新增或修改的 provider。
pub struct ProviderEditor {
    /// `None` 是新增。
    pub id: Option<ProviderId>,
    pub kind: ProviderKind,
    pub name: Entity<InputState>,
    pub base_url: Entity<InputState>,
    /// 遮蔽輸入；修改時留空表示沿用已存的 key。
    pub api_key: Entity<InputState>,
    pub model: Entity<InputState>,
    /// 每次請求額外送出的 header；空白列在送出時略過。
    pub headers: Vec<ProviderHeaderRow>,
    pub test: ProviderTest,
    /// 已送出 `SaveProvider`、等服務回覆；成功（收到 `Providers`）就收起表單。
    pub saving: bool,
    /// 服務拒絕儲存的原因，顯示在表單裡，打過的內容不會不見。
    pub error: Option<String>,
}

/// provider 表單裡的一列 header。
pub struct ProviderHeaderRow {
    pub name: Entity<InputState>,
    pub value: Entity<InputState>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderTest {
    Idle,
    /// 等伺服器回 `ProviderTest`，帶請求編號。
    Running(u64),
    /// 連線成功，可用的模型。
    Ok(Vec<ModelOption>),
    Err(String),
}

/// 左欄「自動化」下的兩列都進自動化頁，分別捲到各自的區塊。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutomationSection {
    Schedules,
    Listeners,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelTab {
    Chat,
    Goals,
    Artifacts,
    Automations,
    Memory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GoalTab {
    Tasks,
    Brief,
    Ledger,
    Team,
}

/// 右欄（狀態面板）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Panel {
    Goal(GoalId, GoalTab),
    Task(TaskId),
    /// Artifact 編輯器，取代狀態面板。只能經由 [`PotsApp::open_artifact`] 打開。
    Artifact(ArtifactId),
    Schedule(ScheduleId),
    Listener(ListenerId),
    /// 新增 listener 的表單（[`PotsApp::listener_form`]）。
    NewListener,
    /// 新增或修改外部來源的表單（[`PotsApp::source_editor`]）。
    NewSource,
}

/// 右欄正在編輯的 artifact：每個 `##` 區段一個編輯器。
///
/// 編輯先留在編輯器裡（草稿），按「儲存」才帶著 `base_rev` 送給服務；期間 agent 改過的話
/// 服務以區段合併。沒有草稿時，服務推來的新版本直接換掉編輯器內容。
pub struct ArtifactEditor {
    pub artifact: ArtifactId,
    pub sections: Vec<SectionEditor>,
    pub scroll: ScrollHandle,
    /// 編輯器內容根據的版本，儲存時當 `base_rev`。
    pub base_rev: u32,
    /// `base_rev` 那一版的全文，用來判斷有沒有草稿。
    base_body: String,
    /// 有還沒儲存的修改。
    pub dirty: bool,
    /// 已送出儲存，等服務回覆；儲存期間編輯器唯讀。
    pub saving: bool,
    /// 送出的草稿，用來認出服務回的是不是這次儲存的結果。
    sent: Option<String>,
    /// 有草稿時服務更新到的版本（儲存時會合併）。
    pub remote_rev: Option<u32>,
    /// 儲存被拒絕的原因。
    pub error: Option<String>,
    /// 版本紀錄；`None` 是編輯模式。
    pub history: Option<ArtifactHistory>,
    _subscriptions: Vec<Subscription>,
}

/// 版本紀錄模式。
pub struct ArtifactHistory {
    /// 由新到舊；還沒收到服務回覆時是 `None`。
    pub revisions: Option<Vec<ArtifactRevisionView>>,
    /// 選取的版本（`revisions` 的索引）。
    pub selected: usize,
}

pub struct SectionEditor {
    pub heading: Option<String>,
    pub editor: Entity<BlockEditorState>,
    /// 這段原本的 Markdown。編輯器會把內容正規化（例如 `*` 清單改成 `-`），
    /// 沒動過的區段儲存時送原文，免得被當成你改過、在合併時和 agent 的修改衝突。
    raw: String,
    /// 編輯器剛載入時的 `source()`，用來判斷這段有沒有被你動過。
    initial: String,
}

impl SectionEditor {
    /// 這段目前要存的 Markdown：沒動過就是原文。
    fn draft(&self, cx: &App) -> String {
        let source = self.editor.read(cx).source();
        if source == self.initial {
            self.raw.clone()
        } else {
            source
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InboxFilter {
    All,
    Kind(InboxKind),
}

/// 連著真正的服務時的狀態；假資料模式是 `None`。
pub struct Live {
    pub conn: Connection,
    pub sync: Sync,
    /// 這條連線的實例在 `Store` 裡目前的 ID（收到快照前是暫用的）。
    pub instance: InstanceId,
    /// 你按了「開始登入」，收到登入網址時要開瀏覽器。
    login_requested: bool,
    /// 你剛建立、還沒收到 `ProjectAdded` 的 project 名稱。
    creating: Option<String>,
}

pub struct PotsApp {
    pub store: Store,
    pub live: Option<Live>,
    /// 側欄目前顯示的實例。
    pub instance: InstanceId,
    pub screen: Screen,
    pub channel_tab: ChannelTab,
    pub panel: Option<Panel>,
    pub inbox_filter: InboxFilter,
    pub automation_section: AutomationSection,
    /// 自動化頁的專案篩選；`None` 是所有專案。
    pub automation_filter: Option<ProjectId>,
    pub artifact_editor: Option<ArtifactEditor>,
    pub send_mode: SendMode,
    /// 已展開的「用了 N 個工具」摘要。
    pub expanded_tools: HashSet<ItemId>,
    /// 「為什麼」面板的內容（`ClientCommand::Why` 的結果）。
    pub why: Option<WhyView>,
    /// 底部短暫顯示的提示，例如「看 diff 尚未實作」。
    pub toast: Option<String>,

    pub composer: Entity<TextareaState>,
    pub goal_note: Entity<InputState>,
    pub worker_note: Entity<InputState>,
    pub overseer_input: Entity<TextareaState>,
    /// 設定視窗開著時是目前的類別；`None` 是關著。
    pub settings: Option<SettingsTab>,
    pub provider_editor: Option<ProviderEditor>,
    pub memory_model_editor: Option<MemoryModelEditor>,
    pub observation_editor: Option<ObservationEditor>,
    pub github_editor: Option<GithubEditor>,
    pub worker_editor: Option<WorkerEditor>,
    /// 設定視窗「Worker」類別剛複製了哪一段指令（按鈕顯示「已複製」）。
    pub copied: Option<&'static str>,
    pub listener_form: Option<ListenerForm>,
    pub source_editor: Option<SourceEditor>,
    pub repo_editor: Option<RepoEditor>,
    pub notification_editor: Option<NotificationEditor>,
    pub user_md_editor: Option<UserMdEditor>,
    pub reject_dialog: Option<RejectDialog>,
    /// 桌面通知；假資料模式是 `None`。
    pub notifier: Option<Notifier>,
    /// 最後一則送出的通知：視窗在之後幾分鐘內被叫到前景時開啟它。
    pub last_notice: Option<(Notice, Instant)>,
    pub(crate) next_test_request: u64,
    /// 左欄「新增專案」的輸入框；`None` 時顯示按鈕。
    pub new_project: Option<(Entity<InputState>, Subscription)>,
    pub stream_scroll: ScrollHandle,
    pub overseer_scroll: ScrollHandle,
    pub automations_scroll: ScrollHandle,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl PotsApp {
    pub fn new(store: Store, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let instance = store
            .instances
            .last()
            .map(|i| i.id)
            .expect("至少要有一個實例");
        let project = store
            .instance(instance)
            .and_then(|i| i.projects.first())
            .map(|p| p.id);

        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 8)
                .submit_on_enter(true)
                .placeholder("傳訊息")
        });
        let overseer_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 6)
                .submit_on_enter(true)
                .placeholder("問總管任何專案的事")
        });
        let goal_note =
            cx.new(|cx| InputState::new(window, cx).placeholder("在這個 Goal 留言給 Planner"));
        let worker_note =
            cx.new(|cx| InputState::new(window, cx).placeholder("直接對這個 worker 說話"));

        let subscriptions = vec![
            cx.subscribe_in(&composer, window, |this, _, ev: &InputEvent, window, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    this.send_composer(window, cx);
                }
            }),
            cx.subscribe_in(
                &overseer_input,
                window,
                |this, _, ev: &InputEvent, window, cx| {
                    if matches!(ev, InputEvent::PressEnter { .. }) {
                        this.send_overseer(window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &goal_note,
                window,
                |this, _, ev: &InputEvent, window, cx| {
                    if matches!(ev, InputEvent::PressEnter { .. }) {
                        this.send_goal_note(window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &worker_note,
                window,
                |this, _, ev: &InputEvent, window, cx| {
                    if matches!(ev, InputEvent::PressEnter { .. }) {
                        this.send_worker_note(window, cx);
                    }
                },
            ),
        ];

        let focus = cx.focus_handle();
        focus.focus(window, cx);
        // 點了通知（或之後幾分鐘內回到視窗）：開啟那一筆。
        let mut subscriptions = subscriptions;
        subscriptions.push(cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                this.open_last_notice(window, cx);
            }
        }));

        let mut this = Self {
            store,
            live: None,
            instance,
            screen: project.map_or(Screen::Inbox(None), Screen::Project),
            channel_tab: ChannelTab::Chat,
            panel: None,
            inbox_filter: InboxFilter::All,
            automation_section: AutomationSection::Schedules,
            automation_filter: None,
            artifact_editor: None,
            send_mode: SendMode::Queue,
            expanded_tools: HashSet::new(),
            why: None,
            toast: None,
            composer,
            goal_note,
            worker_note,
            overseer_input,
            new_project: None,
            settings: None,
            provider_editor: None,
            memory_model_editor: None,
            observation_editor: None,
            github_editor: None,
            worker_editor: None,
            copied: None,
            listener_form: None,
            source_editor: None,
            repo_editor: None,
            notification_editor: None,
            user_md_editor: None,
            reject_dialog: None,
            notifier: None,
            last_notice: None,
            next_test_request: 0,
            stream_scroll: ScrollHandle::new(),
            overseer_scroll: ScrollHandle::new(),
            automations_scroll: ScrollHandle::new(),
            focus,
            _subscriptions: subscriptions,
        };
        if let Some(p) = project {
            this.open_project(instance, p, window, cx);
        }
        this
    }

    /// 連到真正的服務。視窗先顯示「連線中」的本機實例，收到快照後換成伺服器的資料。
    pub fn new_live(endpoint: impl Endpoint, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let offset = sync::local_offset_secs();
        let placeholder = InstanceId::new();
        let store = Store {
            now: Timestamp(Timestamp::now().0 + offset),
            instances: vec![sync::placeholder_instance(placeholder, "本機")],
            inbox: vec![],
            outbox: vec![],
        };
        let mut this = Self::new(store, window, cx);
        this.screen = Screen::Inbox(Some(placeholder));

        let (conn, mut events) = connection::spawn(endpoint);
        this.notifier = Some(Notifier::new(Arc::new(notify::Os)));
        this.live = Some(Live {
            conn,
            sync: Sync::new(offset),
            instance: placeholder,
            login_requested: false,
            creating: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = events.next().await {
                if this
                    .update_in(cx, |app, window, cx| app.on_conn_event(event, window, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        // 相對時間（「2 分鐘前」）要跟著走。
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(30))
                    .await;
                let alive = this.update(cx, |app, cx| {
                    app.store.now = Timestamp(Timestamp::now().0 + offset);
                    app.tick_notifications();
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
        this
    }

    fn on_conn_event(&mut self, event: ConnEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(live) = &mut self.live else {
            return;
        };
        let instance = live.instance;
        match event {
            ConnEvent::State(state) => {
                if state != ConnectionState::Connected
                    && let Some(editor) = &mut self.artifact_editor
                    && editor.saving
                {
                    editor.saving = false;
                    editor.sent = None;
                    editor.error = Some("連線中斷，請再儲存一次".into());
                    set_readonly(editor, false, cx);
                }
                // 斷線時等不到回覆了。
                if state != ConnectionState::Connected
                    && let Some(editor) = &mut self.provider_editor
                {
                    if matches!(editor.test, ProviderTest::Running(_)) {
                        editor.test = ProviderTest::Idle;
                    }
                    if editor.saving {
                        editor.saving = false;
                        editor.error = Some("連線中斷，請再儲存一次".into());
                    }
                }
                if state != ConnectionState::Connected
                    && let Some(editor) = &mut self.memory_model_editor
                    && editor.saving
                {
                    editor.saving = false;
                    editor.error = Some("連線中斷，請再儲存一次".into());
                }
                if state != ConnectionState::Connected {
                    self.forms().on_disconnected();
                }
                if let Some(i) = self.store.instance_mut(instance) {
                    i.connection = state;
                    i.host = match state {
                        ConnectionState::Connected => "這台電腦".into(),
                        ConnectionState::Reconnecting => "重新連線中…".into(),
                        ConnectionState::Offline => "離線".into(),
                    };
                }
            }
            ConnEvent::Problem(problem) => self.toast = Some(problem),
            ConnEvent::Server(event) => {
                let applied = live.sync.apply(&mut self.store, instance, *event);
                crate::views::automation_forms::Forms {
                    github: &mut self.github_editor,
                    worker: &mut self.worker_editor,
                    listener: &mut self.listener_form,
                    source: &mut self.source_editor,
                    repo: &mut self.repo_editor,
                    notifications: &mut self.notification_editor,
                    user_md: &mut self.user_md_editor,
                    panel: &mut self.panel,
                }
                .applied(&applied);
                if applied.why.is_some() {
                    self.why = applied.why;
                }
                if let Some((old, new)) = applied.rekeyed {
                    live.instance = new;
                    if self.instance == old {
                        self.instance = new;
                    }
                    if self.screen == Screen::Inbox(Some(old)) {
                        self.screen = Screen::Inbox(Some(new));
                    }
                }
                if let Some(url) = applied.login_url
                    && std::mem::take(&mut live.login_requested)
                {
                    cx.open_url(&url);
                }
                // 自動化與 GitHub 的表單被拒絕時，原因顯示在表單裡。
                let toast = applied.toast.clone().and_then(|t| {
                    crate::views::automation_forms::Forms {
                        github: &mut self.github_editor,
                        worker: &mut self.worker_editor,
                        listener: &mut self.listener_form,
                        source: &mut self.source_editor,
                        repo: &mut self.repo_editor,
                        notifications: &mut self.notification_editor,
                        user_md: &mut self.user_md_editor,
                        panel: &mut self.panel,
                    }
                    .route_error(t)
                });
                if let Some(toast) = toast {
                    // 儲存 provider 或 artifact 被拒絕：原因顯示在表單或編輯器裡。
                    match (
                        &mut self.provider_editor,
                        &mut self.memory_model_editor,
                        &mut self.artifact_editor,
                    ) {
                        (Some(editor), _, _) if editor.saving => {
                            editor.saving = false;
                            editor.error = Some(toast);
                        }
                        (_, Some(editor), _) if editor.saving => {
                            editor.saving = false;
                            editor.error = Some(toast);
                        }
                        (_, _, Some(editor)) if editor.saving => {
                            editor.saving = false;
                            editor.sent = None;
                            editor.error = Some(toast);
                            set_readonly(editor, false, cx);
                        }
                        _ => self.toast = Some(toast),
                    }
                }
                if let Some((id, revisions)) = applied.artifact_history
                    && let Some(editor) = &mut self.artifact_editor
                    && editor.artifact == id
                    && let Some(history) = &mut editor.history
                {
                    history.selected = 0;
                    history.revisions = Some(revisions);
                }
                if applied.providers_changed
                    && self.memory_model_editor.as_ref().is_some_and(|e| e.saving)
                {
                    self.memory_model_editor = None;
                }
                // 修正到一半的觀察被整理掉了（例如反思成新的一代）：收起輸入框。
                if let Some(project) = applied.memory_changed
                    && let Some(editor) = &self.observation_editor
                    && editor.project == project
                    && self
                        .store
                        .project(live.instance, project)
                        .is_none_or(|p| !p.memory.observations.iter().any(|o| o.id == editor.id))
                {
                    self.observation_editor = None;
                }
                if applied.providers_changed
                    && let Some(editor) = &self.provider_editor
                {
                    let gone = editor.id.is_some_and(|id| {
                        self.store
                            .instance(live.instance)
                            .is_none_or(|i| i.providers.get(id).is_none())
                    });
                    if editor.saving || gone {
                        self.provider_editor = None;
                    }
                }
                if let Some((request, result)) = applied.provider_test
                    && let Some(editor) = &mut self.provider_editor
                    && editor.test == ProviderTest::Running(request)
                {
                    editor.test = match result {
                        Ok(models) => ProviderTest::Ok(models),
                        Err(message) => ProviderTest::Err(message),
                    };
                }
                let created = applied.project_added.filter(|p| {
                    let name = self
                        .store
                        .project(live.instance, *p)
                        .map(|p| p.name.as_str());
                    live.creating.is_some() && live.creating.as_deref() == name
                });
                let current_missing = match self.screen {
                    Screen::Project(p) => self.store.project(live.instance, p).is_none(),
                    _ => false,
                };
                let first = self
                    .store
                    .instance(live.instance)
                    .and_then(|i| i.projects.first())
                    .map(|p| p.id);
                let instance = live.instance;
                if let Some(p) = created {
                    live.creating = None;
                    self.open_project(instance, p, window, cx);
                } else if current_missing
                    || (applied.rekeyed.is_some()
                        && self.screen == Screen::Inbox(Some(instance))
                        && self.store.pending(Some(instance)) == 0)
                {
                    // 快照換掉了目前的 project，或剛連上而且沒有待處理的事：開第一個 project。
                    match first {
                        Some(p) => self.open_project(instance, p, window, cx),
                        None => self.open_inbox(Some(instance), cx),
                    }
                }
                if matches!(self.screen, Screen::Project(_)) {
                    self.stream_scroll.scroll_to_bottom();
                }
                if applied.artifacts_reset {
                    self.on_artifacts_reset(window, cx);
                } else if let Some(id) = applied.artifact_changed {
                    self.on_artifact_changed(id, window, cx);
                }
                // 決定好了的核准不再需要拒絕的對話框。
                if let Some(d) = &self.reject_dialog
                    && self.store.approval(d.id).is_none_or(|i| !i.is_open())
                {
                    self.reject_dialog = None;
                }
                let active = window.is_window_active();
                for notice in applied.notices {
                    self.push_notice(notice, active);
                }
            }
        }
        cx.notify();
    }

    /// 依通知設定送出（或累積）一則通知。
    fn push_notice(&mut self, notice: Notice, window_active: bool) {
        let Some(notifier) = &mut self.notifier else {
            return;
        };
        let Some(settings) = self
            .store
            .instance(notice.instance)
            .map(|i| i.notifications.clone())
        else {
            return;
        };
        let focused = window_active
            && notice.instance == self.instance
            && notice
                .project
                .is_some_and(|p| self.screen == Screen::Project(p));
        if notifier.notice(notice.clone(), &settings, notify::local_minute(), focused) {
            self.last_notice = Some((notice, Instant::now()));
        }
    }

    /// 靜音時段結束：把累積的通知合併成一則。
    fn tick_notifications(&mut self) {
        let Some(notifier) = &mut self.notifier else {
            return;
        };
        let Some(settings) = self
            .live
            .as_ref()
            .and_then(|l| self.store.instance(l.instance))
            .map(|i| i.notifications.clone())
        else {
            return;
        };
        if let Some(n) = notifier.tick(&settings, notify::local_minute()) {
            self.last_notice = Some((n, Instant::now()));
        }
    }

    /// 送出通知後不久視窗被叫到前景（多半是點了通知）：開啟那一筆。
    fn open_last_notice(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((notice, at)) = self.last_notice.take() else {
            return;
        };
        if at.elapsed() > Duration::from_secs(10 * 60) {
            return;
        }
        match (notice.item, notice.project) {
            (Some(id), _) if self.store.approval(id).is_some() => {
                self.open_inbox_item(id, window, cx);
            }
            (_, Some(p)) if self.store.project(notice.instance, p).is_some() => {
                self.open_project(notice.instance, p, window, cx);
            }
            _ => self.open_inbox(Some(notice.instance), cx),
        }
    }

    fn forms(&mut self) -> crate::views::automation_forms::Forms<'_> {
        crate::views::automation_forms::Forms {
            github: &mut self.github_editor,
            worker: &mut self.worker_editor,
            listener: &mut self.listener_form,
            source: &mut self.source_editor,
            repo: &mut self.repo_editor,
            notifications: &mut self.notification_editor,
            user_md: &mut self.user_md_editor,
            panel: &mut self.panel,
        }
    }

    /// 把使用者動作累積的指令送給服務。假資料模式不送，留在 outbox 方便測試。
    fn flush_outbox(&mut self) {
        let Some(live) = &mut self.live else {
            return;
        };
        for (instance, command) in self.store.outbox.drain(..) {
            if instance != live.instance {
                continue;
            }
            if matches!(command, ClientCommand::StartModelLogin) {
                live.login_requested = true;
            }
            live.conn.send(command);
        }
    }

    /// 左欄「新增專案」：顯示輸入框。
    pub fn begin_new_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("專案名稱，Enter 建立"));
        input.update(cx, |s, cx| s.focus(window, cx));
        let sub = cx.subscribe(&input, |this, input, ev: &InputEvent, cx| match ev {
            InputEvent::PressEnter { .. } => {
                let name = input.read(cx).value().trim().to_string();
                this.new_project = None;
                if !name.is_empty() {
                    this.create_project(name, cx);
                }
                cx.notify();
            }
            InputEvent::Blur => {
                this.new_project = None;
                cx.notify();
            }
            _ => {}
        });
        self.new_project = Some((input, sub));
        cx.notify();
    }

    fn create_project(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(live) = &mut self.live else {
            self.notify_unimplemented("新增專案", cx);
            return;
        };
        live.creating = Some(name.clone());
        let instance = live.instance;
        self.store
            .outbox
            .push((instance, ClientCommand::CreateProject { name }));
        cx.notify();
    }

    // ---- 設定 ---------------------------------------------------------------

    /// 開啟設定視窗（目前實例的設定）。已經開著時切到指定類別。
    pub fn open_settings(&mut self, tab: SettingsTab, cx: &mut Context<Self>) {
        self.settings = Some(tab);
        cx.notify();
    }

    /// 關掉設定視窗，丟掉打到一半的 provider 表單（可能含 API key）與記憶模型表單。
    pub fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.settings = None;
        self.provider_editor = None;
        self.memory_model_editor = None;
        self.github_editor = None;
        self.worker_editor = None;
        self.notification_editor = None;
        self.user_md_editor = None;
        self.copied = None;
        cx.notify();
    }

    /// 開啟「整理記憶的模型」表單，帶入目前的設定。
    pub fn edit_memory_model(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.current_instance().providers.memory.clone();
        let model = cx.new(|cx| {
            let mut state =
                InputState::new(window, cx).placeholder("留空用這個 provider 設定的模型");
            state.set_value(current.model.clone(), window, cx);
            state
        });
        self.memory_model_editor = Some(MemoryModelEditor {
            provider: current.provider,
            model,
            saving: false,
            error: None,
        });
        cx.notify();
    }

    /// 表單裡選一個 provider；`None` 是跟對話 agent 相同。
    pub fn pick_memory_provider(&mut self, provider: Option<ProviderId>, cx: &mut Context<Self>) {
        if let Some(editor) = &mut self.memory_model_editor {
            editor.provider = provider;
            editor.error = None;
        }
        cx.notify();
    }

    pub fn save_memory_model(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.memory_model_editor else {
            return;
        };
        let provider = editor.provider;
        let model = if provider.is_some() {
            editor.model.read(cx).value().trim().to_string()
        } else {
            String::new()
        };
        let sent = self.settings_command(
            ClientCommand::SetMemoryModel { provider, model },
            "儲存記憶模型",
            cx,
        );
        if let Some(editor) = &mut self.memory_model_editor {
            editor.saving = sent;
            editor.error = None;
        }
    }

    pub fn cancel_memory_model_edit(&mut self, cx: &mut Context<Self>) {
        self.memory_model_editor = None;
        cx.notify();
    }

    // ---- 記憶分頁 -------------------------------------------------------------

    pub fn pin_observation(&mut self, id: ObservationId, pinned: bool, cx: &mut Context<Self>) {
        let Screen::Project(project) = self.screen else {
            return;
        };
        self.store
            .pin_observation(self.instance, project, id, pinned);
        cx.notify();
    }

    pub fn delete_observation(&mut self, id: ObservationId, cx: &mut Context<Self>) {
        let Screen::Project(project) = self.screen else {
            return;
        };
        if self.observation_editor.as_ref().is_some_and(|e| e.id == id) {
            self.observation_editor = None;
        }
        self.store.delete_observation(self.instance, project, id);
        cx.notify();
    }

    /// 開始修正一條觀察：文字變成輸入框，Enter 儲存。
    pub fn edit_observation(
        &mut self,
        id: ObservationId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Screen::Project(project) = self.screen else {
            return;
        };
        let Some(text) = self
            .store
            .project(self.instance, project)
            .and_then(|p| p.memory.observations.iter().find(|o| o.id == id))
            .map(|o| o.text.clone())
        else {
            return;
        };
        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder("觀察的內容");
            state.set_value(text, window, cx);
            state
        });
        input.update(cx, |s, cx| s.focus(window, cx));
        let subscription = cx.subscribe(&input, |this, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) {
                this.save_observation_edit(cx);
            }
        });
        self.observation_editor = Some(ObservationEditor {
            project,
            id,
            input,
            _subscription: subscription,
        });
        cx.notify();
    }

    pub fn save_observation_edit(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.observation_editor.take() else {
            return;
        };
        let text = editor.input.read(cx).value().to_string();
        if let Err(message) =
            self.store
                .edit_observation(self.instance, editor.project, editor.id, text)
        {
            self.toast = Some(message);
            self.observation_editor = Some(editor);
        }
        cx.notify();
    }

    pub fn cancel_observation_edit(&mut self, cx: &mut Context<Self>) {
        self.observation_editor = None;
        cx.notify();
    }

    fn on_close_settings(&mut self, _: &CloseSettings, _: &mut Window, cx: &mut Context<Self>) {
        if self.reject_dialog.is_some() {
            self.cancel_reject(cx);
        } else if self.settings.is_some() {
            self.close_settings(cx);
        } else {
            cx.propagate();
        }
    }

    /// 設定頁的動作在假資料模式下沒有服務可以存，只提示。回傳是否送出。
    pub(crate) fn settings_command(
        &mut self,
        command: ClientCommand,
        label: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.live.is_none() {
            self.notify_unimplemented(label, cx);
            return false;
        }
        self.store.outbox.push((self.instance, command));
        cx.notify();
        true
    }

    /// 開啟 provider 編輯表單：`id` 為 `None` 時新增一個 `kind` 的 provider。
    pub fn edit_provider(
        &mut self,
        id: Option<ProviderId>,
        kind: ProviderKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing = id.and_then(|id| self.current_instance().providers.get(id).cloned());
        let (name, base_url, model) = match &existing {
            Some(p) => (p.name.clone(), p.base_url.clone(), p.model.clone()),
            None => (
                kind.label().to_string(),
                kind.default_base_url().to_string(),
                String::new(),
            ),
        };
        let input = |value: String,
                     placeholder: &str,
                     masked: bool,
                     window: &mut Window,
                     cx: &mut Context<Self>| {
            let placeholder = placeholder.to_string();
            cx.new(|cx| {
                let mut state = InputState::new(window, cx)
                    .placeholder(placeholder)
                    .masked(masked);
                state.set_value(value, window, cx);
                state
            })
        };
        let key_placeholder = if existing.as_ref().is_some_and(|p| p.key_hint.is_some()) {
            "留空沿用已存的 key"
        } else if kind == ProviderKind::OpenAiCompatible {
            "貼上 API key；本機服務（例如 Ollama）可以留空"
        } else {
            "貼上 API key"
        };
        let model_placeholder = match kind {
            ProviderKind::ChatGpt => "留空自動選帳號清單的第一個",
            _ if kind.preset().is_some() => "按「列出模型」從內建清單挑一個",
            _ => "例如 gpt-4.1、claude-sonnet-4-5；可先「測試連線」再從清單挑",
        };
        let headers = match &existing {
            Some(p) => p
                .headers
                .iter()
                .map(|h| ProviderHeaderRow {
                    name: input(h.name.clone(), "header 名稱", false, window, cx),
                    value: input(h.value.clone(), "header 值", false, window, cx),
                })
                .collect(),
            // 新的 preset 先帶上目錄預設的 header（例如 x-opencode-client）。
            None => kind
                .preset()
                .map(|preset| {
                    preset
                        .headers
                        .iter()
                        .map(|h| ProviderHeaderRow {
                            name: input(h.name.clone(), "header 名稱", false, window, cx),
                            value: input(h.value.clone(), "header 值", false, window, cx),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        };
        self.provider_editor = Some(ProviderEditor {
            id,
            kind,
            name: input(name, "名稱", false, window, cx),
            base_url: input(base_url, kind.default_base_url(), false, window, cx),
            api_key: input(String::new(), key_placeholder, true, window, cx),
            model: input(model, model_placeholder, false, window, cx),
            headers,
            test: ProviderTest::Idle,
            saving: false,
            error: None,
        });
        cx.notify();
    }

    pub fn cancel_provider_edit(&mut self, cx: &mut Context<Self>) {
        self.provider_editor = None;
        cx.notify();
    }

    fn provider_draft(&self, cx: &App) -> Option<ProviderDraft> {
        let e = self.provider_editor.as_ref()?;
        let value = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let key = value(&e.api_key);
        Some(ProviderDraft {
            id: e.id,
            kind: e.kind,
            name: value(&e.name),
            base_url: value(&e.base_url),
            model: value(&e.model),
            api_key: (!key.is_empty()).then_some(key),
            headers: e
                .headers
                .iter()
                .filter_map(|h| {
                    let name = value(&h.name);
                    let value = value(&h.value);
                    (!name.is_empty()).then_some(ProviderHeader { name, value })
                })
                .collect(),
        })
    }

    /// 表單加一列空白 header。
    pub fn add_provider_header(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.provider_editor else {
            return;
        };
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("header 名稱"));
        let value = cx.new(|cx| InputState::new(window, cx).placeholder("header 值"));
        editor.headers.push(ProviderHeaderRow { name, value });
        cx.notify();
    }

    /// 移除表單的第 `index` 列 header。
    pub fn remove_provider_header(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.provider_editor else {
            return;
        };
        if index < editor.headers.len() {
            editor.headers.remove(index);
        }
        cx.notify();
    }

    pub fn test_provider(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = self.provider_draft(cx) else {
            return;
        };
        self.next_test_request += 1;
        let request = self.next_test_request;
        let sent = self.settings_command(
            ClientCommand::TestProvider { request, draft },
            "測試連線",
            cx,
        );
        if let Some(editor) = &mut self.provider_editor {
            editor.test = if sent {
                ProviderTest::Running(request)
            } else {
                ProviderTest::Err("沒有連到服務，不能測試".into())
            };
        }
    }

    pub fn save_provider(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = self.provider_draft(cx) else {
            return;
        };
        if draft.name.is_empty() {
            self.set_editor_error("請填名稱", cx);
            return;
        }
        // OpenAI 相容的本機服務（例如 Ollama）不需要 key。
        if draft.id.is_none() && draft.kind.key_required() && draft.api_key.is_none() {
            self.set_editor_error(&format!("新增 {} 需要 API key", draft.kind.label()), cx);
            return;
        }
        // 表單留著等服務回覆：收到 `Providers` 才收起來，被拒絕時把原因顯示在表單裡。
        let sent =
            self.settings_command(ClientCommand::SaveProvider { draft }, "儲存 provider", cx);
        if let Some(editor) = &mut self.provider_editor {
            editor.saving = sent;
            editor.error = None;
        }
    }

    fn set_editor_error(&mut self, message: &str, cx: &mut Context<Self>) {
        if let Some(editor) = &mut self.provider_editor {
            editor.error = Some(message.into());
            editor.saving = false;
        }
        cx.notify();
    }

    /// 從「測試連線」的模型清單挑一個填進表單。
    pub fn pick_model(&mut self, model: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = &self.provider_editor {
            editor
                .model
                .update(cx, |s, cx| s.set_value(model, window, cx));
        }
        cx.notify();
    }

    pub fn set_active_provider(&mut self, id: ProviderId, cx: &mut Context<Self>) {
        self.settings_command(ClientCommand::SetActiveProvider { id }, "切換 provider", cx);
    }

    pub fn delete_provider(&mut self, id: ProviderId, cx: &mut Context<Self>) {
        if self
            .provider_editor
            .as_ref()
            .is_some_and(|e| e.id == Some(id))
        {
            self.provider_editor = None;
        }
        self.settings_command(ClientCommand::DeleteProvider { id }, "刪除 provider", cx);
    }

    pub fn chatgpt_login(&mut self, cx: &mut Context<Self>) {
        self.settings_command(ClientCommand::StartModelLogin, "登入 ChatGPT", cx);
    }

    pub fn chatgpt_logout(&mut self, cx: &mut Context<Self>) {
        self.settings_command(ClientCommand::ModelLogout, "登出 ChatGPT", cx);
    }

    // ---- 查詢 ---------------------------------------------------------------

    pub fn current_instance(&self) -> &InstanceView {
        self.store
            .instance(self.instance)
            .expect("目前實例必須存在")
    }

    pub fn current_project(&self) -> Option<&ProjectView> {
        match self.screen {
            Screen::Project(p) => self.store.project(self.instance, p),
            Screen::Inbox(_) | Screen::Overseer | Screen::Automations => None,
        }
    }

    // ---- 導覽 ---------------------------------------------------------------

    pub fn select_instance(&mut self, id: InstanceId, window: &mut Window, cx: &mut Context<Self>) {
        // provider 設定屬於實例：換實例時丟掉打到一半的表單（可能含 API key）。
        if id != self.instance {
            self.provider_editor = None;
            self.memory_model_editor = None;
            self.github_editor = None;
            self.worker_editor = None;
        }
        self.instance = id;
        // 在總管或自動化畫面切換實例時，留在新實例的同一個畫面。
        match self.screen {
            Screen::Overseer => {
                self.open_overseer(window, cx);
                return;
            }
            Screen::Automations => {
                self.automation_filter = None;
                self.open_automations(self.automation_section, cx);
                return;
            }
            _ => {}
        }
        let first = self
            .store
            .instance(id)
            .and_then(|i| i.projects.first())
            .map(|p| p.id);
        match first {
            Some(p) => self.open_project(id, p, window, cx),
            None => self.open_inbox(Some(id), cx),
        }
    }

    pub fn open_project(
        &mut self,
        instance: InstanceId,
        project: ProjectId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.instance = instance;
        self.screen = Screen::Project(project);
        // 看到這個 project 了：服務據此判斷「你不在期間」要不要插一張摘要卡片。
        self.store
            .outbox
            .push((instance, ClientCommand::MarkRead { project }));
        self.channel_tab = ChannelTab::Chat;
        self.observation_editor = None;
        // 有進行中的 Goal 時預設在右欄打開。
        self.panel = self
            .store
            .project(instance, project)
            .and_then(|p| p.goals.iter().find(|g| g.state.is_active()))
            .map(|g| Panel::Goal(g.id, GoalTab::Tasks));
        if let Some(p) = self.store.project_mut(instance, project) {
            p.unread = 0;
        }
        let placeholder = self
            .store
            .project(instance, project)
            .map(|p| format!("傳訊息給 {}", p.agent_name))
            .unwrap_or_default();
        self.composer.update(cx, |s, cx| {
            s.set_placeholder(placeholder, window, cx);
        });
        self.stream_scroll.scroll_to_bottom();
        cx.notify();
    }

    pub fn open_inbox(&mut self, scope: Option<InstanceId>, cx: &mut Context<Self>) {
        if let Some(id) = scope {
            self.instance = id;
        }
        self.screen = Screen::Inbox(scope);
        self.panel = None;
        cx.notify();
    }

    /// 從 Inbox 跳到項目所屬的 project 頻道。
    pub fn open_inbox_item(&mut self, id: ApprovalId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.store.approval(id) else {
            return;
        };
        let instance = item.instance;
        match item.project {
            Some(p) => self.open_project(instance, p, window, cx),
            None => {
                self.instance = instance;
                self.open_overseer(window, cx);
            }
        }
    }

    pub fn open_panel(&mut self, panel: Option<Panel>, cx: &mut Context<Self>) {
        debug_assert!(
            !matches!(panel, Some(Panel::Artifact(_))),
            "artifact 面板要用 open_artifact 打開"
        );
        self.panel = panel;
        cx.notify();
    }

    /// 在右欄開啟 artifact 編輯器。artifact 不在目前的 project 時，先切到它所屬的
    /// project 的 Artifacts 分頁。
    pub fn open_artifact(&mut self, id: ArtifactId, window: &mut Window, cx: &mut Context<Self>) {
        let Some((project, _)) = self.store.artifact(self.instance, id) else {
            self.toast = Some("找不到這份 artifact，可能已經刪除。".into());
            cx.notify();
            return;
        };
        let project = project.id;
        // 換到別份會丟掉草稿。
        if let Some(editor) = &self.artifact_editor
            && editor.artifact != id
            && editor.dirty
            && self.store.instances.iter().any(|i| {
                i.projects
                    .iter()
                    .any(|p| p.artifacts.iter().any(|a| a.id == editor.artifact))
            })
        {
            self.toast = Some("目前的 artifact 有還沒儲存的修改，先儲存或放棄。".into());
            if let Some(Panel::Artifact(_)) | None = self.panel {
                self.panel = Some(Panel::Artifact(editor.artifact));
            }
            cx.notify();
            return;
        }

        if self.screen != Screen::Project(project) {
            self.open_project(self.instance, project, window, cx);
            self.channel_tab = ChannelTab::Artifacts;
        }

        // 同一份已經開著就沿用，保留游標、復原紀錄與草稿。
        if self
            .artifact_editor
            .as_ref()
            .is_none_or(|e| e.artifact != id)
        {
            self.artifact_editor = self.build_artifact_editor(id, window, cx);
        }
        self.panel = Some(Panel::Artifact(id));
        cx.notify();
    }

    /// 依目前的內容建立編輯器。
    fn build_artifact_editor(
        &mut self,
        id: ArtifactId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<ArtifactEditor> {
        let (_, artifact) = self.store.artifact(self.instance, id)?;
        let body = artifact.body.clone();
        let rev = artifact.rev;
        let agent_writing = artifact.agent_writing.clone();
        let scroll = ScrollHandle::new();
        let mut sections = Vec::new();
        let mut subscriptions = Vec::new();
        for (ix, section) in split_sections(&body).into_iter().enumerate() {
            // agent 正在寫的區段唯讀，其他區段可以照常編輯。
            let readonly = section.heading.is_some() && section.heading == agent_writing;
            let editor = cx.new(|cx| {
                let mut state = BlockEditorState::new(window, cx).with_source(&section.source);
                state.set_scroll_handle(scroll.clone());
                if readonly {
                    state.set_readonly(true, cx);
                }
                state
            });
            subscriptions.push(cx.subscribe(
                &editor,
                move |this, _, event: &BlockEditorEvent, cx| {
                    if let BlockEditorEvent::Changed { .. } = event {
                        this.on_section_edited(ix, cx);
                    }
                },
            ));
            let initial = editor.read(cx).source();
            sections.push(SectionEditor {
                heading: section.heading,
                editor,
                raw: section.source,
                initial,
            });
        }
        Some(ArtifactEditor {
            artifact: id,
            base_body: join_sections(sections.iter().map(|s| s.raw.as_str())),
            sections,
            scroll,
            base_rev: rev,
            dirty: false,
            saving: false,
            sent: None,
            remote_rev: None,
            error: None,
            history: None,
            _subscriptions: subscriptions,
        })
    }

    /// 編輯器目前的全文（沒動過的區段用原文）。
    fn artifact_draft(&self, cx: &Context<Self>) -> Option<String> {
        let editor = self.artifact_editor.as_ref()?;
        let sources: Vec<String> = editor.sections.iter().map(|s| s.draft(cx)).collect();
        Some(join_sections(sources.iter().map(String::as_str)))
    }

    fn on_section_edited(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(draft) = self.artifact_draft(cx) else {
            return;
        };
        let heading = self
            .artifact_editor
            .as_ref()
            .and_then(|e| e.sections.get(ix))
            .and_then(|s| section_heading(&s.editor.read(cx).source()));
        let Some(editor) = self.artifact_editor.as_mut() else {
            return;
        };
        if let Some(s) = editor.sections.get_mut(ix) {
            s.heading = heading;
        }
        editor.dirty = draft != editor.base_body;
        editor.error = None;
        cx.notify();
    }

    /// 「儲存」：連著服務時帶 `base_rev` 送出，等 `ArtifactChanged`；假資料模式只改本地。
    pub fn save_artifact(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = self.artifact_draft(cx) else {
            return;
        };
        let instance = self.instance;
        let live = self.live.is_some();
        let connected = self.current_instance().connection == ConnectionState::Connected;
        let Some(editor) = self.artifact_editor.as_mut() else {
            return;
        };
        if !editor.dirty || editor.saving {
            return;
        }
        // 離線時不排進待送佇列：重連後才送出的儲存會和之後的儲存互相衝突。
        if live && !connected {
            editor.error = Some("目前離線，連上服務後再儲存。".into());
            cx.notify();
            return;
        }
        let artifact = editor.artifact;
        if live {
            editor.saving = true;
            editor.sent = Some(draft.clone());
            editor.error = None;
            set_readonly(editor, true, cx);
            self.store.outbox.push((
                instance,
                ClientCommand::SaveArtifact {
                    artifact,
                    base_rev: editor.base_rev,
                    body: draft,
                },
            ));
        } else if let Some(rev) = self.store.edit_artifact(instance, artifact, draft.clone()) {
            editor.base_rev = rev;
            editor.base_body = draft;
            editor.dirty = false;
        }
        cx.notify();
    }

    /// 「放棄修改」：用目前的內容重建編輯器。
    pub fn discard_artifact_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 儲存中也可以放棄：服務沒有回覆（例如錯誤被其他表單接走）時，這是唯一的出口。
        // 儲存若之後還是成功，新版本會照常推來並顯示。
        let Some(id) = self.artifact_editor.as_ref().map(|e| e.artifact) else {
            return;
        };
        self.artifact_editor = self.build_artifact_editor(id, window, cx);
        cx.notify();
    }

    /// 服務推來 artifact 的新版本。沒有草稿時換掉編輯器內容；有草稿時保留，提示儲存時會合併。
    /// 儲存中收到的新版本要包含你送出的修改才算是這次儲存的結果（可能是 agent 剛好也寫了），
    /// 否則繼續等，草稿不動。
    fn on_artifact_changed(&mut self, id: ArtifactId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.artifact_editor else {
            return;
        };
        if editor.artifact != id {
            return;
        }
        let Some((_, artifact)) = self.store.artifact(self.instance, id) else {
            return;
        };
        let rev = artifact.rev;
        let body = join_sections(
            split_sections(&artifact.body)
                .iter()
                .map(|s| s.source.as_str()),
        );
        if editor.saving {
            let sent = editor.sent.as_deref().unwrap_or_default();
            if !save_applied(&editor.base_body, sent, &body) {
                if rev != editor.base_rev {
                    editor.remote_rev = Some(rev);
                }
                return;
            }
        } else if editor.dirty {
            if rev != editor.base_rev {
                editor.remote_rev = Some(rev);
            }
            return;
        } else if body == editor.base_body {
            editor.base_rev = rev;
            editor.saving = false;
            return;
        }
        let history = editor.history.take();
        self.artifact_editor = self.build_artifact_editor(id, window, cx);
        if let (Some(editor), Some(_)) = (&mut self.artifact_editor, history) {
            // 版本紀錄開著：重新讀一次，包含新的版本。
            editor.history = Some(ArtifactHistory {
                revisions: None,
                selected: 0,
            });
            self.store.outbox.push((
                self.instance,
                ClientCommand::ArtifactHistory { artifact: id },
            ));
        }
    }

    /// 快照換掉了 artifact：開著的編輯器跟著更新；那份 artifact 不在了而且沒有草稿就關掉。
    fn on_artifacts_reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = &self.artifact_editor else {
            return;
        };
        let id = editor.artifact;
        if self.store.artifact(self.instance, id).is_some() {
            self.on_artifact_changed(id, window, cx);
        } else if !editor.dirty {
            self.artifact_editor = None;
            if self.panel == Some(Panel::Artifact(id)) {
                self.panel = None;
            }
        }
    }

    /// 開關版本紀錄。
    pub fn toggle_artifact_history(&mut self, cx: &mut Context<Self>) {
        if self.live.is_none() {
            self.toast = Some("假資料模式沒有版本紀錄。".into());
            cx.notify();
            return;
        }
        let instance = self.instance;
        let Some(editor) = &mut self.artifact_editor else {
            return;
        };
        if editor.history.take().is_none() {
            editor.history = Some(ArtifactHistory {
                revisions: None,
                selected: 0,
            });
            self.store.outbox.push((
                instance,
                ClientCommand::ArtifactHistory {
                    artifact: editor.artifact,
                },
            ));
        }
        cx.notify();
    }

    pub fn select_revision(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(history) = self
            .artifact_editor
            .as_mut()
            .and_then(|e| e.history.as_mut())
        {
            history.selected = ix;
        }
        cx.notify();
    }

    /// 還原成某一版（服務新增一個版本）。有草稿時不做，免得蓋掉。
    pub fn restore_revision(&mut self, rev: u32, cx: &mut Context<Self>) {
        let instance = self.instance;
        let Some(editor) = &mut self.artifact_editor else {
            return;
        };
        if editor.dirty {
            editor.error = Some("先儲存或放棄你的修改，再還原。".into());
            cx.notify();
            return;
        }
        editor.history = None;
        self.store.outbox.push((
            instance,
            ClientCommand::RestoreArtifact {
                artifact: editor.artifact,
                rev,
            },
        ));
        cx.notify();
    }

    /// 切到目前實例的自動化頁，並捲到指定區塊。
    pub fn open_automations(&mut self, section: AutomationSection, cx: &mut Context<Self>) {
        if self.screen != Screen::Automations {
            self.panel = None;
        }
        self.screen = Screen::Automations;
        self.automation_section = section;
        // 捲動區的子元素：暫停橫幅（若有）、排程、listener。
        let paused = self.current_instance().automation_pause.is_some();
        match section {
            AutomationSection::Schedules => self.automations_scroll.scroll_to_top_of_item(0),
            AutomationSection::Listeners => self
                .automations_scroll
                .scroll_to_top_of_item(if paused { 2 } else { 1 }),
        }
        cx.notify();
    }

    pub fn set_schedule_enabled(&mut self, id: ScheduleId, enabled: bool, cx: &mut Context<Self>) {
        self.store.set_schedule_enabled(self.instance, id, enabled);
        cx.notify();
    }

    pub fn set_listener_enabled(&mut self, id: ListenerId, enabled: bool, cx: &mut Context<Self>) {
        self.store.set_listener_enabled(self.instance, id, enabled);
        cx.notify();
    }

    pub fn delete_schedule(&mut self, id: ScheduleId, cx: &mut Context<Self>) {
        self.store.delete_schedule(self.instance, id);
        if self.panel == Some(Panel::Schedule(id)) {
            self.panel = None;
        }
        cx.notify();
    }

    pub fn delete_listener(&mut self, id: ListenerId, cx: &mut Context<Self>) {
        self.store.delete_listener(self.instance, id);
        if self.panel == Some(Panel::Listener(id)) {
            self.panel = None;
        }
        cx.notify();
    }

    pub fn resume_automations(&mut self, cx: &mut Context<Self>) {
        self.store.resume_automations(self.instance, None);
        cx.notify();
    }

    /// 切到目前實例的總管對話。
    pub fn open_overseer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.screen = Screen::Overseer;
        self.panel = None;
        self.overseer_scroll.scroll_to_bottom();
        self.overseer_input.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    fn on_open_overseer(&mut self, _: &OpenOverseer, window: &mut Window, cx: &mut Context<Self>) {
        self.open_overseer(window, cx);
    }

    /// 問一則 agent 訊息為什麼這樣回答。
    pub fn request_why(&mut self, item: ItemId, cx: &mut Context<Self>) {
        self.why = None;
        if self.live.is_some() {
            self.store
                .outbox
                .push((self.instance, ClientCommand::Why { item }));
            self.toast = Some("正在查這一則的來源與工具順序…".into());
        } else {
            self.toast = Some("「為什麼」需要連上服務才有 trace。".into());
        }
        cx.notify();
    }

    pub fn close_why(&mut self, cx: &mut Context<Self>) {
        self.why = None;
        cx.notify();
    }

    pub fn toggle_tools(&mut self, id: ItemId, cx: &mut Context<Self>) {
        if !self.expanded_tools.remove(&id) {
            self.expanded_tools.insert(id);
        }
        cx.notify();
    }

    pub fn notify_unimplemented(&mut self, what: &str, cx: &mut Context<Self>) {
        self.toast = Some(format!("「{what}」尚未接上後端，這版只是畫面。"));
        cx.notify();
    }

    // ---- 動作 ---------------------------------------------------------------

    pub fn act(
        &mut self,
        id: ApprovalId,
        action: InboxAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            InboxAction::OpenThread => self.open_inbox_item(id, window, cx),
            InboxAction::OpenOverseer => {
                if let Some(item) = self.store.approval(id) {
                    self.instance = item.instance;
                }
                // 看過就算處理了。
                if let Some(item) = self.store.inbox.iter_mut().find(|i| i.id == id) {
                    item.decision = Decision::Resolved("已讀".into());
                }
                self.open_overseer(window, cx);
            }
            InboxAction::OpenSettings => {
                if let Some(item) = self.store.approval(id) {
                    self.instance = item.instance;
                }
                self.open_settings(SettingsTab::Providers, cx);
            }
            InboxAction::ViewDiff => {
                let url = self.store.approval(id).and_then(|i| match &i.approval {
                    Some(ApprovalView {
                        action: ApprovalActionView::MergePr { url: Some(u), .. },
                        ..
                    }) => Some(u.clone()),
                    _ => None,
                });
                match url {
                    Some(u) => cx.open_url(&u),
                    None => self.notify_unimplemented(action.label(), cx),
                }
            }
            InboxAction::EditConditions => {
                self.notify_unimplemented(action.label(), cx);
            }
            // 服務的核准：先問理由（選填，會轉告 agent）。
            InboxAction::Reject
                if self
                    .store
                    .approval(id)
                    .is_some_and(|i| i.approval.is_some() && i.is_open()) =>
            {
                self.begin_reject(id, window, cx);
            }
            _ => {
                self.store.act(id, action);
                cx.notify();
            }
        }
    }

    pub fn begin_reject(&mut self, id: ApprovalId, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("理由（選填）：例如「請補上測試」，會轉告 agent")
        });
        input.update(cx, |s, cx| s.focus(window, cx));
        let sub = cx.subscribe(&input, |this, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) {
                this.confirm_reject(cx);
            }
        });
        self.reject_dialog = Some(RejectDialog {
            id,
            input,
            _subscription: sub,
        });
        cx.notify();
    }

    pub fn confirm_reject(&mut self, cx: &mut Context<Self>) {
        let Some(d) = self.reject_dialog.take() else {
            return;
        };
        let note = d.input.read(cx).value().trim().to_string();
        self.store.decide(d.id, false, Some(note));
        cx.notify();
    }

    pub fn cancel_reject(&mut self, cx: &mut Context<Self>) {
        self.reject_dialog = None;
        cx.notify();
    }

    /// 頂端的「停止」：中斷對話 agent 本輪。
    pub fn stop_agent(&mut self, cx: &mut Context<Self>) {
        if self.screen == Screen::Overseer {
            if self.live.is_some() {
                self.store
                    .outbox
                    .push((self.instance, ClientCommand::StopOverseer));
            } else if let Some(inst) = self.store.instance_mut(self.instance) {
                inst.overseer.state = AgentState::Idle;
            }
            cx.notify();
            return;
        }
        let Screen::Project(project) = self.screen else {
            return;
        };
        if self.live.is_some() {
            // 狀態由伺服器回報，這裡只送出指令。
            self.store
                .outbox
                .push((self.instance, ClientCommand::Stop { project }));
            cx.notify();
            return;
        }
        if let Some(p) = self.store.project_mut(self.instance, project) {
            p.agent_state = AgentState::Idle;
            for item in &mut p.items {
                if let StreamItemKind::Agent(m) = &mut item.kind
                    && m.status == MsgStatus::Streaming
                {
                    m.status = MsgStatus::Done;
                }
            }
        }
        cx.notify();
    }

    pub fn retry_task(&mut self, task: TaskId, cx: &mut Context<Self>) {
        self.store.retry_task(self.instance, task);
        cx.notify();
    }

    pub fn cancel_task(&mut self, task: TaskId, cx: &mut Context<Self>) {
        self.store.cancel_task(self.instance, task);
        cx.notify();
    }

    pub fn send_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Screen::Project(project) = self.screen else {
            return;
        };
        let text = self.composer.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        self.store
            .send_to_project(self.instance, project, text, self.send_mode);
        self.send_mode = SendMode::Queue;
        self.composer.update(cx, |s, cx| s.clean(window, cx));
        self.stream_scroll.scroll_to_bottom();
        cx.notify();
    }

    pub fn send_overseer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.overseer_input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        self.store.send_to_overseer(self.instance, text);
        self.overseer_input.update(cx, |s, cx| s.clean(window, cx));
        self.overseer_scroll.scroll_to_bottom();
        cx.notify();
    }

    pub fn send_goal_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Panel::Goal(goal, _)) = self.panel else {
            return;
        };
        let text = self.goal_note.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        if self.live.is_some() {
            // 協定還沒有「留言給 Planner」的指令；要改方向請在對話裡說，或直接改 Brief。
            self.notify_unimplemented("留言給 Planner", cx);
            return;
        }
        self.store.post_goal_note(self.instance, goal, text);
        self.panel = Some(Panel::Goal(goal, GoalTab::Team));
        self.goal_note.update(cx, |s, cx| s.clean(window, cx));
        cx.notify();
    }

    pub fn send_worker_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Panel::Task(task)) = self.panel else {
            return;
        };
        let text = self.worker_note.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        if self.live.is_some() {
            // 協定還沒有直接對 worker 說話的指令（任務 thread 之後再做）。
            self.notify_unimplemented("對 worker 說話", cx);
            return;
        }
        self.store.send_to_task(self.instance, task, text);
        self.worker_note.update(cx, |s, cx| s.clean(window, cx));
        cx.notify();
    }
}

/// 儲存期間鎖住編輯器，避免回覆到來時重建編輯器丟掉之後打的字。
fn set_readonly(editor: &ArtifactEditor, readonly: bool, cx: &mut App) {
    for s in &editor.sections {
        s.editor
            .update(cx, |state, cx| state.set_readonly(readonly, cx));
    }
}

/// `current` 是否包含 `sent` 相對 `base` 的所有修改（以 `##` 區段比對）。
fn save_applied(base: &str, sent: &str, current: &str) -> bool {
    fn sections(md: &str) -> Vec<(Option<String>, String)> {
        split_sections(md)
            .into_iter()
            .map(|s| (s.heading, s.source))
            .collect()
    }
    fn get<'a>(v: &'a [(Option<String>, String)], k: &Option<String>) -> Option<&'a String> {
        v.iter().find(|(h, _)| h == k).map(|(_, s)| s)
    }
    let (base, sent, current) = (sections(base), sections(sent), sections(current));
    base.iter()
        .chain(&sent)
        .map(|(k, _)| k)
        .filter(|k| get(&base, k) != get(&sent, k))
        .all(|k| get(&current, k) == get(&sent, k))
}

impl Focusable for PotsApp {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for PotsApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 每個使用者動作都會 notify 而重繪，在這裡把它們累積的指令送出。
        self.flush_outbox();
        let multi = self.store.instances.len() > 1;
        let center = match self.screen {
            Screen::Inbox(scope) => self.render_inbox(scope, cx).into_any_element(),
            Screen::Project(_) => self.render_channel(window, cx).into_any_element(),
            Screen::Overseer => self.render_overseer(cx).into_any_element(),
            Screen::Automations => self.render_automations_page(cx).into_any_element(),
        };
        // 總管畫面的右欄固定是專案概況與記憶；其他畫面是狀態面板。
        let panel = match self.screen {
            Screen::Overseer => Some(self.render_overseer_aside(cx).into_any_element()),
            _ => self.panel.and_then(|p| self.render_panel(p, cx)),
        };

        div()
            .key_context("PotsApp")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::on_open_overseer))
            .on_action(cx.listener(Self::on_close_settings))
            .relative()
            .flex()
            .size_full()
            .bg(theme::white())
            .text_color(theme::text())
            .text_size(gpui_kit::px(14.))
            // 只連一個實例時不顯示實例直列。
            .when(multi, |d| d.child(self.render_rail(cx)))
            .child(match self.screen {
                Screen::Inbox(None) => self.render_inbox_sidebar(cx).into_any_element(),
                _ => self.render_sidebar(cx).into_any_element(),
            })
            .child(center)
            .children(panel)
            .children(self.render_settings_dialog(cx))
            .children(self.render_reject_dialog(cx))
            .children(self.render_why_dialog(cx))
            .children(self.render_toast(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::save_applied;

    const BASE: &str = "# 標題\n\n## 目標\n\n舊\n\n## 範圍\n\n- 一\n";

    #[test]
    fn save_applied_needs_every_section_you_changed() {
        let sent = BASE.replace("舊", "我的");
        // 服務回的是你的儲存（可能合併了 agent 改的別段）。
        assert!(save_applied(BASE, &sent, &sent));
        assert!(save_applied(
            BASE,
            &sent,
            &sent.replace("- 一", "- 一\n- agent")
        ));
        // agent 剛好寫了別段，還不是你的儲存結果。
        assert!(!save_applied(BASE, &sent, &BASE.replace("- 一", "- agent")));
        // 刪掉的區段也要不在。
        let removed = BASE.replace("## 範圍\n\n- 一\n", "");
        assert!(save_applied(BASE, &removed, &removed));
        assert!(!save_applied(BASE, &removed, BASE));
        // 沒有修改的儲存，任何回覆都算。
        assert!(save_applied(BASE, BASE, BASE));
    }
}
