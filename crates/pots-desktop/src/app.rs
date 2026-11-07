//! 根視圖：持有 [`Store`] 與所有 UI 狀態，各區塊的 render 拆在 `views/`。

use std::collections::HashSet;

use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, KeyBinding, ParentElement as _, Render, ScrollHandle, Styled as _, Subscription,
    Window, div,
};

use crate::model::*;
use crate::theme;

gpui_kit::actions!(pots, [OpenOverseer]);

pub fn bind_keys(cx: &mut App) {
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InboxFilter {
    All,
    Kind(InboxKind),
}

pub struct PotsApp {
    pub store: Store,
    /// 側欄目前顯示的實例。
    pub instance: InstanceId,
    pub screen: Screen,
    pub channel_tab: ChannelTab,
    pub panel: Option<Panel>,
    pub inbox_filter: InboxFilter,
    pub send_mode: SendMode,
    /// 已展開的「用了 N 個工具」摘要。
    pub expanded_tools: HashSet<ItemId>,
    /// 底部短暫顯示的提示，例如「看 diff 尚未實作」。
    pub toast: Option<String>,

    pub composer: Entity<TextareaState>,
    pub goal_note: Entity<InputState>,
    pub worker_note: Entity<InputState>,
    pub overseer_input: Entity<TextareaState>,
    pub stream_scroll: ScrollHandle,
    pub overseer_scroll: ScrollHandle,
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
            instance,
            screen: project.map_or(Screen::Inbox(None), Screen::Project),
            channel_tab: ChannelTab::Chat,
            panel: None,
            inbox_filter: InboxFilter::All,
            send_mode: SendMode::Queue,
            expanded_tools: HashSet::new(),
            toast: None,
            composer,
            goal_note,
            worker_note,
            overseer_input,
            stream_scroll: ScrollHandle::new(),
            overseer_scroll: ScrollHandle::new(),
            focus,
            _subscriptions: subscriptions,
        };
        if let Some(p) = project {
            this.open_project(instance, p, window, cx);
        }
        this
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
            Screen::Inbox(_) | Screen::Overseer => None,
        }
    }

    // ---- 導覽 ---------------------------------------------------------------

    pub fn select_instance(&mut self, id: InstanceId, window: &mut Window, cx: &mut Context<Self>) {
        self.instance = id;
        // 在總管畫面切換實例時，留在新實例的總管。
        if self.screen == Screen::Overseer {
            self.open_overseer(window, cx);
            return;
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
        self.panel = panel;
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
        let multi = self.store.instances.len() > 1;
        let center = match self.screen {
            Screen::Inbox(scope) => self.render_inbox(scope, cx).into_any_element(),
            Screen::Project(_) => self.render_channel(window, cx).into_any_element(),
            Screen::Overseer => self.render_overseer(cx).into_any_element(),
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
