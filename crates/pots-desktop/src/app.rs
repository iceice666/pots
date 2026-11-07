//! 根視圖：持有 [`Store`] 與所有 UI 狀態，各區塊的 render 拆在 `views/`。

use std::collections::HashSet;
use std::time::Duration;

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
use crate::sync::{self, Sync};
use crate::theme;

gpui_kit::actions!(pots, [OpenOverseer]);

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
}

/// 右欄正在編輯的 artifact：每個 `##` 區段一個編輯器。
pub struct ArtifactEditor {
    pub artifact: ArtifactId,
    pub sections: Vec<SectionEditor>,
    pub scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

pub struct SectionEditor {
    pub heading: Option<String>,
    pub editor: Entity<BlockEditorState>,
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
    /// 底部短暫顯示的提示，例如「看 diff 尚未實作」。
    pub toast: Option<String>,

    pub composer: Entity<TextareaState>,
    pub goal_note: Entity<InputState>,
    pub worker_note: Entity<InputState>,
    pub overseer_input: Entity<TextareaState>,
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
            toast: None,
            composer,
            goal_note,
            worker_note,
            overseer_input,
            new_project: None,
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
                let applied = live.sync.apply(&mut self.store, instance, event);
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
                if let Some(toast) = applied.toast {
                    self.toast = Some(toast);
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
            }
        }
        cx.notify();
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
        self.channel_tab = ChannelTab::Chat;
        // 有進行中的 Goal 時預設在右欄打開。
        self.panel = self
            .store
            .project(instance, project)
            .and_then(|p| p.goals.first())
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
        let Some((project, artifact)) = self.store.artifact(self.instance, id) else {
            self.toast = Some("找不到這份 artifact，可能已經刪除。".into());
            cx.notify();
            return;
        };
        let project = project.id;
        let body = artifact.body.clone();
        let agent_writing = artifact.agent_writing.clone();

        if self.screen != Screen::Project(project) {
            self.open_project(self.instance, project, window, cx);
            self.channel_tab = ChannelTab::Artifacts;
        }

        // 同一份已經開著就沿用，保留游標與復原紀錄。
        if self
            .artifact_editor
            .as_ref()
            .is_none_or(|e| e.artifact != id)
        {
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
                sections.push(SectionEditor {
                    heading: section.heading,
                    editor,
                });
            }
            self.artifact_editor = Some(ArtifactEditor {
                artifact: id,
                sections,
                scroll,
                _subscriptions: subscriptions,
            });
        }
        self.panel = Some(Panel::Artifact(id));
        cx.notify();
    }

    fn on_section_edited(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(editor) = &self.artifact_editor else {
            return;
        };
        let sources: Vec<String> = editor
            .sections
            .iter()
            .map(|s| s.editor.read(cx).source())
            .collect();
        let heading = sources.get(ix).and_then(|s| section_heading(s));
        let artifact = editor.artifact;
        if let Some(s) = self
            .artifact_editor
            .as_mut()
            .and_then(|e| e.sections.get_mut(ix))
        {
            s.heading = heading.clone();
        }
        self.store.edit_artifact(
            self.instance,
            artifact,
            join_sections(sources.iter().map(String::as_str)),
            heading,
        );
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
        self.store.resume_automations(self.instance);
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
            InboxAction::ViewDiff | InboxAction::EditConditions => {
                self.notify_unimplemented(action.label(), cx);
            }
            _ => {
                self.store.act(id, action);
                cx.notify();
            }
        }
    }

    /// 頂端的「停止」：中斷對話 agent 本輪。
    pub fn stop_agent(&mut self, cx: &mut Context<Self>) {
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
        self.store.send_to_task(self.instance, task, text);
        self.worker_note.update(cx, |s, cx| s.clean(window, cx));
        cx.notify();
    }
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
            .children(self.render_toast(cx))
    }
}
