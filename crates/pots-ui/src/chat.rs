//! The main workspace view: top bar, sidebar, channel header, message list,
//! composer, and the Discord-style thread panel.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    AnyElement, AnyView, AsyncApp, ClipboardItem, Context, Entity, EntityInputHandler, FocusHandle, Focusable,
    FontWeight, HighlightStyle, KeyBinding, KeyDownEvent, ListAlignment, ListState, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ScrollHandle, SharedString, StyledText, Subscription, Task, WeakEntity, Window,
    actions, canvas, div, list, prelude::*, px, rgb,
};

use pots_agent::agent::{self, Agents};
use pots_agent::data::{self, Card, Conv, MergeOutcome, Message, UserKind, Workspace};
use pots_agent::markdown::{self, Sep};
use pots_agent::reply::{Host, Outcome, blocking};
use pots_agent::search::{Hit, MessageHit};
use pots_agent::{sandbox, store};
use pots_llm::Usage;
use pots_llm::cost::{self, Catalog};

use crate::composer::{self, Composer, ComposerEvent};
use crate::selection::{Pos, Selection, SharedSelection};
use crate::settings::{Settings, SettingsEvent};
use crate::{render_md, theme};

actions!(messages, [CopySelection, SelectAllMessages, ClearSelection]);
actions!(pots, [OpenSettings, OpenSearch]);

const MESSAGES_CONTEXT: &str = "Messages";
/// Name of a thread started from the header when none is typed.
const NEW_THREAD: &str = "New Thread";
/// Most search results shown, for names and for messages each.
const SEARCH_LIMIT: usize = 20;
/// How long a message opened from search stays highlighted.
const HIGHLIGHT_FOR: Duration = Duration::from_secs(3);
/// Width of the top bar's search box and its results.
const SEARCH_WIDTH: f32 = 560.;

pub fn bind_keys(cx: &mut gpui::App) {
    cx.bind_keys(bindings());
}

pub(crate) fn bindings() -> Vec<KeyBinding> {
    let c = Some(MESSAGES_CONTEXT);
    vec![
        KeyBinding::new(secondary!("c"), CopySelection, c),
        KeyBinding::new(secondary!("a"), SelectAllMessages, c),
        KeyBinding::new("escape", ClearSelection, c),
        KeyBinding::new(secondary!(","), OpenSettings, None),
        KeyBinding::new(secondary!("k"), OpenSearch, None),
    ]
}

/// One scrollable, selectable message list (the channel, or the thread panel).
struct Pane {
    list: ListState,
    selection: SharedSelection,
    focus: FocusHandle,
}

impl Pane {
    fn new(cx: &mut Context<ChatApp>) -> Self {
        Self {
            list: ListState::new(0, ListAlignment::Bottom, px(800.)),
            selection: Rc::new(RefCell::new(Selection::default())),
            focus: cx.focus_handle(),
        }
    }

    fn reset(&self, count: usize) {
        self.selection.borrow_mut().clear();
        self.list.reset(count);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PaneId {
    Main,
    Side,
}

/// What the thread panel on the right shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Panel {
    Closed,
    /// Discord's "Create Thread" form in `channel`, for message `msg`, or
    /// with no message (from the header's button). The thread only comes
    /// into existence when its first message is sent.
    NewThread { channel: usize, msg: Option<usize> },
    Thread(usize),
}

/// The top bar's search, while it's open.
struct Search {
    hits: Vec<Hit>,
    /// The result Enter opens (an index into `hits`).
    selected: usize,
    scroll: ScrollHandle,
}

/// The channel dialog: creating a channel, or editing channel `editing`.
struct ChannelDialog {
    editing: Option<usize>,
    /// Why the last Create/Save didn't go through.
    error: Option<String>,
}

pub struct ChatApp {
    ws: Workspace,
    /// False when the saved workspace couldn't be read: never overwrite it.
    persist: bool,
    agents: Agents,
    /// Agent replies being written, by message id. Dropping one stops it.
    running: HashMap<usize, Task<()>>,
    /// Agents to start in a conversation once its writer finishes replying.
    waiting: Vec<(usize, Conv)>,
    /// Notes from the app for agents that were replying where the note
    /// should go: (agent, conversation, for the agent, shown to the owner).
    /// They're handed over once that reply is done.
    notes: Vec<(usize, Conv, String, String)>,
    /// Merge cards whose approval is being carried out.
    merging: HashSet<usize>,
    active: usize,
    main: Pane,
    side: Pane,
    panel: Panel,
    composer: Entity<Composer>,
    thread_composer: Entity<Composer>,
    thread_name: Entity<Composer>,
    dialog: Option<ChannelDialog>,
    channel_name: Entity<Composer>,
    channel_topic: Entity<Composer>,
    channel_repo: Entity<Composer>,
    /// The workspace menu (under the workspace name) is open.
    menu: bool,
    settings: Option<(Entity<Settings>, Subscription)>,
    /// Id of the channel message the mouse is over; it shows the action bar.
    hovered: Option<usize>,
    search_box: Entity<Composer>,
    search: Option<Search>,
    /// Id of the message opened from search, shown highlighted for a while,
    /// and which highlight that is (so an older timer leaves a newer one).
    highlight: Option<usize>,
    highlight_seq: usize,
    _subscriptions: Vec<Subscription>,
}

impl ChatApp {
    /// With `mock`, the app shows demo data and leaves the saved workspace
    /// alone (neither read nor written).
    pub fn new(mock: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (mut ws, persist) = if mock {
            (data::seed(), false)
        } else {
            match store::load_workspace() {
                Ok(Some(ws)) => (ws, true),
                Ok(None) => (data::empty(), true),
                Err(e) => {
                    eprintln!("pots: couldn't read the saved workspace ({e}); starting empty without saving");
                    (data::empty(), false)
                }
            }
        };
        let agents = Agents::load(&mut ws);
        if let Some(problem) = &agents.problem {
            eprintln!("pots: {problem}");
        }
        let active = ws.channels.iter().position(|c| c.name == "engineering").unwrap_or(0);
        let composer = cx.new(Composer::new);
        let thread_composer = cx.new(Composer::new);
        let thread_name = cx.new(Composer::compact);
        let channel_name = cx.new(Composer::compact);
        let channel_topic = cx.new(Composer::compact);
        let channel_repo = cx.new(Composer::compact);
        let search_box = cx.new(Composer::small);
        search_box.update(cx, |c, cx| c.set_placeholder(format!("Search {}", ws.name), cx));
        let subscriptions = vec![
            cx.subscribe(&composer, |this, _, event: &ComposerEvent, cx| {
                if let ComposerEvent::Submit(text) = event {
                    this.send(text, cx)
                }
            }),
            cx.subscribe_in(&thread_composer, window, |this, _, event: &ComposerEvent, window, cx| {
                if let ComposerEvent::Submit(text) = event {
                    this.send_in_thread(text, window, cx)
                }
            }),
            // Enter in the thread name field moves on to the message box.
            cx.subscribe_in(&thread_name, window, |this, _, event: &ComposerEvent, window, cx| {
                if let ComposerEvent::Submit(_) = event {
                    window.focus(&this.thread_composer.read(cx).focus_handle(cx), cx)
                }
            }),
            // The channel dialog: Enter moves down the fields, then saves.
            cx.subscribe_in(&channel_name, window, |this, _, event: &ComposerEvent, window, cx| {
                if let ComposerEvent::Submit(_) = event {
                    window.focus(&this.channel_topic.read(cx).focus_handle(cx), cx)
                }
            }),
            cx.subscribe_in(&channel_topic, window, |this, _, event: &ComposerEvent, window, cx| {
                if let ComposerEvent::Submit(_) = event {
                    window.focus(&this.channel_repo.read(cx).focus_handle(cx), cx)
                }
            }),
            cx.subscribe_in(&channel_repo, window, |this, _, event: &ComposerEvent, window, cx| {
                if let ComposerEvent::Submit(_) = event {
                    this.save_channel(window, cx)
                }
            }),
            // Search as you type; while an input method is still composing,
            // the last results stay until the text is final.
            cx.subscribe_in(&search_box, window, |this, search_box, event: &ComposerEvent, window, cx| match event {
                ComposerEvent::Changed if !search_box.read(cx).is_composing() => this.run_search(cx),
                ComposerEvent::Changed => {}
                ComposerEvent::Submit(_) => {
                    let selected = this.search.as_ref().map_or(0, |s| s.selected);
                    this.open_hit(selected, window, cx)
                }
            }),
        ];
        let mut this = Self {
            ws,
            persist,
            agents,
            running: HashMap::new(),
            waiting: Vec::new(),
            notes: Vec::new(),
            merging: HashSet::new(),
            active,
            main: Pane::new(cx),
            side: Pane::new(cx),
            panel: Panel::Closed,
            composer,
            thread_composer,
            thread_name,
            dialog: None,
            channel_name,
            channel_topic,
            channel_repo,
            menu: false,
            settings: None,
            hovered: None,
            search_box,
            search: None,
            highlight: None,
            highlight_seq: 0,
            _subscriptions: subscriptions,
        };
        this.select_channel(active, window, cx);
        this.save();
        // Model prices, for showing what replies cost.
        cx.spawn(async move |this, cx| {
            match blocking(|| Catalog::load(&store::home())).await.and_then(|r| r) {
                Ok(catalog) => this.update(cx, |app, _| app.agents.catalog = Some(std::sync::Arc::new(catalog))).ok(),
                Err(e) => Some(eprintln!("pots: no model prices: {e}")),
            };
        })
        .detach();
        this
    }

    fn save(&self) {
        if self.persist {
            if let Err(e) = store::save_workspace(&self.ws) {
                eprintln!("pots: couldn't save the workspace: {e}");
            }
        }
    }

    pub fn composer(&self) -> &Entity<Composer> {
        &self.composer
    }

    // --- panes -----------------------------------------------------------

    fn pane(&self, id: PaneId) -> &Pane {
        match id {
            PaneId::Main => &self.main,
            PaneId::Side => &self.side,
        }
    }

    fn pane_composer(&self, id: PaneId) -> &Entity<Composer> {
        match id {
            PaneId::Main => &self.composer,
            PaneId::Side => &self.thread_composer,
        }
    }

    fn pane_len(&self, id: PaneId) -> usize {
        match (id, self.panel) {
            (PaneId::Main, _) => self.ws.len(Conv::Channel(self.active)),
            (PaneId::Side, Panel::Thread(t)) => self.ws.len(Conv::Thread(t)),
            (PaneId::Side, Panel::NewThread { msg, .. }) => usize::from(msg.is_some()),
            (PaneId::Side, Panel::Closed) => 0,
        }
    }

    fn pane_message(&self, id: PaneId, ix: usize) -> &Message {
        match (id, self.panel) {
            (PaneId::Main, _) => self.ws.message(Conv::Channel(self.active), ix),
            (PaneId::Side, Panel::Thread(t)) => self.ws.message(Conv::Thread(t), ix),
            (PaneId::Side, Panel::NewThread { channel, msg: Some(msg) }) => &self.ws.channels[channel].messages[msg],
            (PaneId::Side, Panel::Closed | Panel::NewThread { msg: None, .. }) => {
                unreachable!("this thread panel has no messages")
            }
        }
    }

    fn pane_message_mut(&mut self, id: PaneId, ix: usize) -> &mut Message {
        match (id, self.panel) {
            (PaneId::Main, _) => self.ws.message_mut(Conv::Channel(self.active), ix),
            (PaneId::Side, Panel::Thread(t)) => self.ws.message_mut(Conv::Thread(t), ix),
            (PaneId::Side, Panel::NewThread { channel, msg: Some(msg) }) => &mut self.ws.channels[channel].messages[msg],
            (PaneId::Side, Panel::Closed | Panel::NewThread { msg: None, .. }) => {
                unreachable!("this thread panel has no messages")
            }
        }
    }

    fn pane_messages(&self, id: PaneId) -> Vec<&Message> {
        (0..self.pane_len(id)).map(|ix| self.pane_message(id, ix)).collect()
    }

    /// Re-measure message `id` wherever it is shown (a thread's starter is in both panes).
    fn refresh_message(&self, id: usize) {
        for pane in [PaneId::Main, PaneId::Side] {
            if let Some(ix) = (0..self.pane_len(pane)).find(|&ix| self.pane_message(pane, ix).id == id) {
                self.pane(pane).list.splice(ix..ix + 1, 1);
            }
        }
    }

    // --- navigation ------------------------------------------------------

    fn panel_channel(&self) -> Option<usize> {
        match self.panel {
            Panel::Closed => None,
            Panel::NewThread { channel, .. } => Some(channel),
            Panel::Thread(t) => Some(self.ws.threads[t].channel),
        }
    }

    fn select_channel(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.active = ix;
        self.hovered = None;
        self.highlight = None;
        let channel = &mut self.ws.channels[ix];
        channel.unread = false;
        let placeholder = match channel.dm {
            Some(_) => format!("Message {}", channel.name),
            None => format!("Message #{}", channel.name),
        };
        self.main.reset(self.ws.len(Conv::Channel(ix)));
        // Like Discord, a thread panel belongs to its parent channel.
        if self.panel_channel() != Some(ix) {
            self.panel = Panel::Closed;
            self.side.reset(0);
        }
        self.composer.update(cx, |c, cx| c.set_placeholder(placeholder, cx));
        window.focus(&self.composer.read(cx).focus_handle(cx), cx);
        cx.notify();
    }

    /// "Create Thread" on a channel message: open the new-thread form, or
    /// the existing thread if the message already has one.
    fn start_thread(&mut self, msg: usize, window: &mut Window, cx: &mut Context<Self>) {
        let message = &self.ws.channels[self.active].messages[msg];
        if let Some(t) = message.thread {
            return self.open_thread(t, window, cx);
        }
        let name = data::default_thread_name(message);
        self.open_new_thread(Some(msg), name, window, cx);
    }

    /// The header's "Create Thread": a thread straight from the channel,
    /// with no message to start it from.
    fn start_channel_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_new_thread(None, NEW_THREAD.to_string(), window, cx);
    }

    /// Open the new-thread form for message `msg` of the channel (or none),
    /// suggesting `name`.
    fn open_new_thread(&mut self, msg: Option<usize>, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.panel = Panel::NewThread { channel: self.active, msg };
        self.highlight = None;
        self.side.reset(usize::from(msg.is_some()));
        self.thread_name.update(cx, |c, cx| {
            c.set_text("", cx);
            c.set_placeholder(name, cx);
        });
        self.thread_composer
            .update(cx, |c, cx| c.set_placeholder("Enter a message to start the conversation!", cx));
        window.focus(&self.thread_name.read(cx).focus_handle(cx), cx);
        cx.notify();
    }

    fn open_thread(&mut self, t: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.panel = Panel::Thread(t);
        self.highlight = None;
        self.side.reset(self.ws.len(Conv::Thread(t)));
        let placeholder = format!("Message {}", self.ws.threads[t].name);
        self.thread_composer.update(cx, |c, cx| c.set_placeholder(placeholder, cx));
        window.focus(&self.thread_composer.read(cx).focus_handle(cx), cx);
        cx.notify();
    }

    fn select_thread(&mut self, t: usize, window: &mut Window, cx: &mut Context<Self>) {
        let channel = self.ws.threads[t].channel;
        if channel != self.active {
            self.select_channel(channel, window, cx);
        }
        self.open_thread(t, window, cx);
    }

    /// Archive thread `t` (it leaves the sidebar) or bring it back. An open
    /// panel stays open, so the user can change their mind.
    fn set_archived(&mut self, t: usize, archived: bool, cx: &mut Context<Self>) {
        self.ws.threads[t].archived = archived;
        self.save();
        cx.notify();
    }

    fn close_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.panel = Panel::Closed;
        self.highlight = None;
        self.side.reset(0);
        window.focus(&self.composer.read(cx).focus_handle(cx), cx);
        cx.notify();
    }

    // --- search ----------------------------------------------------------

    fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Not over the dialogs (they're on top of the top bar).
        if self.dialog.is_some() || self.settings.is_some() {
            return;
        }
        self.menu = false;
        if self.search.is_none() {
            self.search = Some(Search { hits: Vec::new(), selected: 0, scroll: ScrollHandle::new() });
            self.run_search(cx);
        }
        window.focus(&self.search_box.read(cx).focus_handle(cx), cx);
        cx.notify();
    }

    /// Close the search and forget what was typed.
    fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.take().is_none() {
            return;
        }
        self.search_box.update(cx, |c, cx| c.set_text("", cx));
        window.focus(&self.composer.read(cx).focus_handle(cx), cx);
        cx.notify();
    }

    fn run_search(&mut self, cx: &mut Context<Self>) {
        let query = self.search_box.read(cx).text().to_string();
        let hits = self.ws.search(&query, SEARCH_LIMIT);
        if let Some(search) = &mut self.search {
            search.hits = hits;
            search.selected = 0;
            search.scroll.scroll_to_item(0);
            cx.notify();
        }
    }

    /// Move the search's selection up (-1) or down (1), stopping at the ends.
    fn move_hit(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(search) = &mut self.search else { return };
        if search.hits.is_empty() {
            return;
        }
        search.selected = search.selected.saturating_add_signed(delta).min(search.hits.len() - 1);
        // The list has a heading before the names and one before the messages.
        let names = search.hits.iter().filter(|h| !matches!(h, Hit::Message(_))).count();
        let headings = usize::from(names > 0) + usize::from(search.selected >= names);
        search.scroll.scroll_to_item(search.selected + headings);
        cx.notify();
    }

    /// Go to search result `ix`, and close the search.
    fn open_hit(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(hit) = self.search.as_ref().and_then(|s| s.hits.get(ix)).cloned() else { return };
        self.close_search(window, cx);
        match hit {
            Hit::Channel(c) if c < self.ws.channels.len() => self.select_channel(c, window, cx),
            Hit::Thread(t) if t < self.ws.threads.len() => self.select_thread(t, window, cx),
            Hit::Message(m) => {
                let Some((conv, ix)) = self.ws.locate(m.id) else { return };
                match conv {
                    Conv::Channel(c) => {
                        self.select_channel(c, window, cx);
                        self.main.list.scroll_to_reveal_item(ix);
                    }
                    Conv::Thread(t) => {
                        self.select_thread(t, window, cx);
                        self.side.list.scroll_to_reveal_item(ix);
                    }
                }
                // After switching, which clears any highlight.
                self.highlight_message(m.id, cx);
            }
            Hit::Channel(_) | Hit::Thread(_) => {}
        }
    }

    /// Highlight message `id` for a while.
    fn highlight_message(&mut self, id: usize, cx: &mut Context<Self>) {
        self.highlight_seq += 1;
        let seq = self.highlight_seq;
        self.highlight = Some(id);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(HIGHLIGHT_FOR).await;
            this.update(cx, |this, cx| {
                if this.highlight_seq == seq && this.highlight.take().is_some() {
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    // --- channel dialog --------------------------------------------------

    /// Open the dialog for a new channel, or to edit channel `editing`.
    fn open_channel_dialog(&mut self, editing: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let (name, topic, repo) = match editing {
            Some(c) => {
                let ch = &self.ws.channels[c];
                (ch.name.clone(), ch.topic.clone(), ch.repo.clone().unwrap_or_default())
            }
            None => Default::default(),
        };
        let repo_hint = match &self.agents.project {
            Some(p) => format!("Default: {}", p.repo),
            None => "~/code/my-project".to_string(),
        };
        for (field, text, placeholder) in [
            (&self.channel_name, name, "e.g. backend".to_string()),
            (&self.channel_topic, topic, "What it's for (optional)".to_string()),
            (&self.channel_repo, repo, repo_hint),
        ] {
            field.update(cx, |f, cx| {
                f.set_text(text, cx);
                f.set_placeholder(placeholder, cx);
            });
        }
        self.dialog = Some(ChannelDialog { editing, error: None });
        window.focus(&self.channel_name.read(cx).focus_handle(cx), cx);
        cx.notify();
    }

    fn close_channel_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dialog = None;
        window.focus(&self.composer.read(cx).focus_handle(cx), cx);
        cx.notify();
    }

    /// Create (or update) the channel from the dialog's fields, if they're valid.
    fn save_channel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editing) = self.dialog.as_ref().map(|d| d.editing) else { return };
        let name = self.channel_name.read(cx).text().to_string();
        let topic = self.channel_topic.read(cx).text().trim().to_string();
        let repo = self.channel_repo.read(cx).text().trim().to_string();
        let checked = self.ws.channel_name(&name, editing).and_then(|name| match repo.as_str() {
            "" => Ok((name, None)),
            r => sandbox::check_repo(r).map(|_| (name, Some(r.to_string()))),
        });
        let (name, repo) = match checked {
            Ok(fields) => fields,
            Err(e) => {
                if let Some(dialog) = &mut self.dialog {
                    dialog.error = Some(e);
                }
                return cx.notify();
            }
        };
        let c = editing.unwrap_or_else(|| self.ws.add_channel(&name));
        let channel = &mut self.ws.channels[c];
        channel.name = name;
        channel.topic = topic;
        channel.repo = repo;
        self.dialog = None;
        self.save();
        self.select_channel(c, window, cx);
    }

    // --- settings --------------------------------------------------------

    fn toggle_menu(&mut self, cx: &mut Context<Self>) {
        self.menu = !self.menu;
        cx.notify();
    }

    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = false;
        if self.settings.is_none() {
            let catalog = self.agents.catalog.clone();
            let settings = cx.new(|cx| Settings::new(catalog, window, cx));
            let sub = cx.subscribe_in(&settings, window, |this, _, event: &SettingsEvent, window, cx| match event {
                SettingsEvent::Changed(config) => this.agents.apply_settings(config),
                SettingsEvent::Close => this.close_settings(window, cx),
            });
            self.settings = Some((settings, sub));
        }
        cx.notify();
    }

    fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings = None;
        window.focus(&self.composer.read(cx).focus_handle(cx), cx);
        cx.notify();
    }

    // --- posting ---------------------------------------------------------

    fn send(&mut self, text: &str, cx: &mut Context<Self>) {
        let conv = Conv::Channel(self.active);
        let ix = self.ws.post(conv, self.ws.me, text);
        self.main.list.splice(ix..ix, 1);
        self.main.list.scroll_to_reveal_item(ix);
        self.posted(conv, ix, cx);
        cx.notify();
    }

    /// After someone posts: save, and let agents that should answer start.
    fn posted(&mut self, conv: Conv, ix: usize, cx: &mut Context<Self>) {
        let triggered = self.agents.triggered(&self.ws, conv, ix);
        if triggered.paused {
            let limit = self.agents.team.max_agent_turns;
            let msg = self.ws.message_mut(conv, ix);
            msg.note = Some(format!("Agents paused after {limit} messages in a row. Reply to keep them going."));
            let id = msg.id;
            self.message_changed(id);
        }
        if !triggered.waiting.is_empty() {
            let names: Vec<String> =
                triggered.waiting.iter().map(|&a| self.agents.list[a].config.name.clone()).collect();
            for &a in &triggered.waiting {
                if !self.waiting.contains(&(a, conv)) {
                    self.waiting.push((a, conv));
                }
            }
            let msg = self.ws.message_mut(conv, ix);
            msg.note = Some(format!("{} will look once the engineer's reply is done.", names.join(", ")));
            let id = msg.id;
            self.message_changed(id);
        }
        self.save();
        for a in triggered.agents {
            self.start_reply(a, conv, cx);
        }
    }

    /// Start the agents waiting in `conv`, once no writer is replying there.
    fn release_waiting(&mut self, conv: Conv, cx: &mut Context<Self>) {
        if self.agents.writer_busy(&self.ws, conv) {
            return;
        }
        let (ready, rest): (Vec<_>, Vec<_>) = self.waiting.drain(..).partition(|&(_, c)| c == conv);
        self.waiting = rest;
        for (a, _) in ready {
            // An @mention in the writer's reply may have started it already.
            if !self.agents.busy(a, &self.ws, conv) {
                self.start_reply(a, conv, cx);
            }
        }
    }

    // --- agents ----------------------------------------------------------

    /// Agent `a` starts answering in `conv`: a pending message shows up at
    /// once and fills in as the reply streams.
    fn start_reply(&mut self, a: usize, conv: Conv, cx: &mut Context<Self>) {
        self.start_reply_with(a, conv, None, cx)
    }

    /// [`start_reply`](Self::start_reply), with `note`: something for the
    /// agent from the app rather than from a message, and what the reply
    /// shows the owner about it.
    fn start_reply_with(&mut self, a: usize, conv: Conv, note: Option<(String, String)>, cx: &mut Context<Self>) {
        let ix = self.ws.post(conv, self.agents.list[a].user, "");
        let msg = self.ws.message_mut(conv, ix);
        msg.pending = true;
        let id = msg.id;
        let mut reply = self.agents.reply(a, &self.ws, conv, ix);
        if let Some((for_agent, shown)) = note {
            agent::add_note(&mut reply.request, &for_agent);
            self.ws.message_mut(conv, ix).note = Some(shown);
        }
        self.message_added(conv, ix);
        if let Some(problem) = self.agents.list[a].missing_key.clone() {
            return self.finish_reply(id, Some(problem), cx);
        }
        let task = cx.spawn(async move |this, cx| {
            let outcome = reply.run(&mut UiHost { app: this.clone(), cx, id, agent: a }).await;
            this.update(cx, |app, cx| match outcome {
                Ok(Outcome::Passed) => app.drop_reply(id, cx),
                Ok(Outcome::Replied) => app.finish_reply(id, None, cx),
                Err(e) => app.finish_reply(id, Some(e), cx),
            })
            .ok();
        });
        self.running.insert(id, task);
    }

    /// The owner's Stop button: cancel the reply where it is.
    fn stop_reply(&mut self, id: usize, cx: &mut Context<Self>) {
        if let Some(task) = self.running.remove(&id) {
            drop(task);
            self.finish_reply(id, Some("Stopped.".into()), cx);
        }
    }

    fn reply_usage(&mut self, id: usize, agent: usize, usage: Usage, worker: bool, cx: &mut Context<Self>) {
        let Some(msg) = self.ws.message_by_id_mut(id) else { return };
        match worker {
            true => msg.worker_usage = Some(usage),
            false => msg.usage = Some(usage),
        }
        // The agent's tokens at its model's price, the worker's at the worker's.
        let own = msg.usage.zip(self.agents.price(agent)).map(|(u, p)| p.cost(&u));
        let asks = msg.worker_usage.zip(self.agents.worker_price()).map(|(u, p)| p.cost(&u));
        msg.cost = match (own, asks) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
        };
        self.message_changed(id);
        cx.notify();
    }

    fn attach_card(&mut self, id: usize, card: Card, cx: &mut Context<Self>) {
        let merge = match &card {
            Card::Merge { branch, summary, commits, conflicts, .. } => {
                Some((branch.clone(), summary.clone(), commits.clone(), conflicts.clone()))
            }
            _ => None,
        };
        let question = match &card {
            Card::Choice { question, options, .. } => Some((question.clone(), options.clone())),
            _ => None,
        };
        let Some(msg) = self.ws.message_by_id_mut(id) else { return };
        let author = msg.author;
        msg.card = Some(card);
        self.message_changed(id);
        // A task waiting on the owner: PM tells them in #general. Questions
        // from coordinators themselves, or outside task threads, don't.
        let worker = self.agents.list.iter().any(|a| a.user == author && a.config.workspace.is_some());
        if let Some((question, options)) = question
            && worker
            && let Some(((conv @ Conv::Thread(_), _), asker)) = self.ws.locate(id).zip(self.author_name(id))
        {
            let place = agent::task_place(&self.ws, conv);
            let note = agent::owner_question_note(&asker, &place, &question, &options);
            let shown = format!("Pots asked for this: {asker} is waiting for your answer in {place}.");
            self.wake_coordinators(conv, note, shown, cx);
        }
        if let Some((branch, summary, commits, conflicts)) = merge
            && let Some(((conv, _), requester)) = self.ws.locate(id).zip(self.author_name(id))
        {
            let place = agent::task_place(&self.ws, conv);
            let note = agent::merge_request_note(&requester, &branch, &place, &summary, &commits, conflicts.as_deref());
            let shown = match conflicts {
                Some(_) => format!("Pots asked for this: the merge request for `{branch}` conflicts with other work."),
                None => format!("Pots asked for this: {requester} asked you to merge `{branch}`."),
            };
            self.wake_coordinators(conv, note, shown, cx);
        }
        cx.notify();
    }

    /// News about the task in `conv` (a merge request, an accepted
    /// branch): have the agents that coordinate it (no workspace, like PM)
    /// take it up in #general, or else in the task's channel. One that's
    /// replying there gets it once that reply is done.
    fn wake_coordinators(&mut self, conv: Conv, note: String, shown: String, cx: &mut Context<Self>) {
        let target = agent::report_conv(&self.ws, conv);
        for a in self.agents.coordinators(&self.ws, conv) {
            if self.agents.busy(a, &self.ws, target) {
                self.notes.push((a, target, note.clone(), shown.clone()));
            } else {
                self.start_reply_with(a, target, Some((note.clone(), shown.clone())), cx);
            }
        }
    }

    /// Hand the notes queued in `conv` to agents no longer replying there,
    /// all of an agent's notes in one reply.
    fn release_notes(&mut self, conv: Conv, cx: &mut Context<Self>) {
        let mut ready: Vec<(usize, String, String)> = Vec::new();
        let agents = &self.agents;
        let ws = &self.ws;
        self.notes.retain(|(a, c, note, shown)| {
            if *c != conv || agents.busy(*a, ws, conv) {
                return true;
            }
            match ready.iter_mut().find(|(r, ..)| r == a) {
                Some((_, n, s)) => {
                    n.push_str("\n\n");
                    n.push_str(note);
                    s.push(' ');
                    s.push_str(shown);
                }
                None => ready.push((*a, note.clone(), shown.clone())),
            }
            false
        });
        for (a, note, shown) in ready {
            self.start_reply_with(a, conv, Some((note, shown)), cx);
        }
    }

    /// Agent `a`'s `start_task` call: open the task's thread and brief, and
    /// let whoever the brief @mentions start on it.
    fn start_task(&mut self, a: usize, input: &serde_json::Value, cx: &mut Context<Self>) -> (String, bool) {
        let author = self.agents.list[a].user;
        let started = match agent::tools::start_task(&mut self.ws, author, input) {
            Ok(started) => started,
            Err(e) => return (e, true),
        };
        let thread = Conv::Thread(started.thread);
        self.message_added(Conv::Channel(started.channel), started.starter);
        self.message_added(thread, started.brief);
        let mut result = started.result;
        if self.agents.repo(&self.ws, thread).is_none() {
            let channel = &self.ws.channels[started.channel].name;
            result.push_str(&format!(
                " Note: #{channel} has no repository (no repo for it in [[channels]] and no [project]), \
                 so engineers there have no workspace to work in."
            ));
        }
        self.posted(thread, started.brief, cx);
        cx.notify();
        (result, false)
    }

    /// Agent `a`'s `task_done` call, from its reply `id`: have the task's
    /// coordinators report it to the owner.
    fn task_done(&mut self, id: usize, a: usize, input: &serde_json::Value, cx: &mut Context<Self>) -> (String, bool) {
        let done = match agent::tools::task_done(input) {
            Ok(done) => done,
            Err(e) => return (e, true),
        };
        let Some((conv @ Conv::Thread(t), _)) = self.ws.locate(id) else {
            return ("task_done only works in a task's thread.".to_string(), true);
        };
        let coordinators = self.agents.coordinators(&self.ws, conv);
        if coordinators.is_empty() {
            return ("Noted. There is no coordinator (like PM) to tell; say it in your reply.".to_string(), false);
        }
        let who = self.agents.list[a].config.name.clone();
        let place = agent::task_place(&self.ws, conv);
        let note = agent::task_done_note(&who, &place, &done);
        let shown = match done.status {
            agent::tools::DoneStatus::Done => format!("Pots asked for this: {who} finished {place}."),
            agent::tools::DoneStatus::Stuck => format!("Pots asked for this: {who} is stuck in {place}."),
        };
        let names: Vec<String> = coordinators.iter().map(|&c| self.agents.list[c].config.name.clone()).collect();
        self.wake_coordinators(conv, note, shown, cx);
        // A finished task is closed: its outcome reaches the owner through
        // the coordinator, so the thread leaves the sidebar. Not when it's
        // stuck, or still has a card for the owner to answer.
        if done.status == agent::tools::DoneStatus::Done && !self.ws.waits_on_owner(t) {
            self.set_archived(t, true, cx);
        }
        cx.notify();
        (format!("{} will tell the owner. Finish your reply here as usual.", names.join(", ")), false)
    }

    /// Agent `a`'s `record_decision` call, from its reply `id`: add it to
    /// the log, and say so under the reply so the owner sees what was kept.
    fn record_decision(&mut self, id: usize, a: usize, input: &serde_json::Value, cx: &mut Context<Self>) -> (String, bool) {
        let author = self.agents.list[a].user;
        let conv = self.ws.locate(id).map(|(conv, _)| conv);
        let recorded = match agent::tools::record_decision(&mut self.ws, author, conv, input) {
            Ok(recorded) => recorded,
            Err(e) => return (e, true),
        };
        if let Some(msg) = self.ws.message_by_id_mut(id) {
            msg.note = Some(match msg.note.take() {
                Some(note) => format!("{note}\n{}", recorded.shown),
                None => recorded.shown,
            });
        }
        self.save();
        cx.notify();
        (recorded.result, false)
    }

    /// Agent `a`'s `post_in_thread` call: its message in the thread, and
    /// whoever it @mentions starts on it there.
    fn post_in_thread(&mut self, a: usize, input: &serde_json::Value, cx: &mut Context<Self>) -> (String, bool) {
        let author = self.agents.list[a].user;
        let posted = match agent::tools::post_in_thread(&mut self.ws, author, input) {
            Ok(posted) => posted,
            Err(e) => return (e, true),
        };
        let thread = Conv::Thread(posted.thread);
        self.message_added(thread, posted.ix);
        self.posted(thread, posted.ix, cx);
        cx.notify();
        (posted.result, false)
    }

    /// Post as the owner in the conversation of message `about`, as if typed.
    fn reply_as_owner(&mut self, about: usize, text: &str, cx: &mut Context<Self>) {
        let Some((conv, _)) = self.ws.locate(about) else { return };
        let ix = self.ws.post(conv, self.ws.me, text);
        self.message_added(conv, ix);
        self.posted(conv, ix, cx);
        cx.notify();
    }

    /// The owner picked option `option` on a choice card.
    fn answer_choice(&mut self, id: usize, option: usize, cx: &mut Context<Self>) {
        let Some(name) = self.author_name(id) else { return };
        let Some(msg) = self.ws.message_by_id_mut(id) else { return };
        let Some(Card::Choice { question, options, answer, .. }) = &mut msg.card else { return };
        if answer.is_some() || option >= options.len() {
            return;
        }
        *answer = Some(option);
        let text = format!("@{name} {} → **{}**", question.trim(), options[option]);
        self.message_changed(id);
        self.reply_as_owner(id, &text, cx);
    }

    /// The owner approved or rejected a merge card.
    fn answer_merge(&mut self, id: usize, approve: bool, cx: &mut Context<Self>) {
        if self.merging.contains(&id) {
            return;
        }
        let Some(name) = self.author_name(id) else { return };
        let Some(msg) = self.ws.message_by_id_mut(id) else { return };
        if !msg.card.as_ref().is_some_and(Card::is_open) {
            return;
        }
        let Some(Card::Merge { branch, source, repo, .. }) = &msg.card else { return };
        let (branch, source, repo) = (branch.clone(), source.clone(), repo.clone());
        // Cards from before repos were per channel don't say: ask the channel.
        let repo = repo.map(|r| pots_agent::config::expand_home(&r)).or_else(|| {
            let (conv, _) = self.ws.locate(id)?;
            self.agents.repo(&self.ws, conv)
        });
        let Some(msg) = self.ws.message_by_id_mut(id) else { return };
        let Some(Card::Merge { outcome, .. }) = &mut msg.card else { return };
        if !approve {
            *outcome = Some(MergeOutcome::Rejected);
            self.message_changed(id);
            return self.reply_as_owner(id, &format!("@{name} Merge of `{branch}` rejected."), cx);
        }
        let Some(repo) = repo else {
            *outcome = Some(MergeOutcome::Failed("no repo for this channel ([[channels]] or [project] in config.toml)".into()));
            self.message_changed(id);
            return cx.notify();
        };
        self.merging.insert(id);
        self.message_changed(id);
        cx.spawn(async move |this, cx| {
            let result = blocking(move || sandbox::accept_branch(&repo, &source, &branch)).await.and_then(|r| r);
            this.update(cx, |app, cx| app.merge_done(id, result, cx)).ok();
        })
        .detach();
        cx.notify();
    }

    fn merge_done(&mut self, id: usize, result: Result<String, String>, cx: &mut Context<Self>) {
        self.merging.remove(&id);
        let Some(msg) = self.ws.message_by_id_mut(id) else { return };
        let Some(Card::Merge { branch, outcome, .. }) = &mut msg.card else { return };
        let branch = branch.clone();
        let ok = result.is_ok();
        *outcome = Some(match result {
            Ok(tip) => MergeOutcome::Approved(tip),
            Err(e) => MergeOutcome::Failed(e),
        });
        self.message_changed(id);
        if ok && let Some((conv, _)) = self.ws.locate(id) {
            // An accepted merge request ends the task: say so in the thread
            // without waking anyone there (any owner post would wake the
            // last agent, and its reply would reopen the thread), close the
            // thread, and let the coordinator report it in #general.
            let ix = self.ws.post(conv, self.ws.me, &format!("Approved: `{branch}` is now a branch in the project repo."));
            self.message_added(conv, ix);
            if let Conv::Thread(t) = conv
                && !self.ws.waits_on_owner(t)
            {
                self.ws.threads[t].archived = true;
            }
            let note = agent::merge_approved_note(&branch, &agent::task_place(&self.ws, conv));
            let shown = format!("Pots asked for this: you accepted `{branch}`.");
            self.wake_coordinators(conv, note, shown, cx);
        }
        self.save();
        cx.notify();
    }

    fn author_name(&self, id: usize) -> Option<String> {
        let (conv, ix) = self.ws.locate(id)?;
        Some(self.ws.users[self.ws.message(conv, ix).author].name.clone())
    }

    /// What each agent is doing now: where, and its current activity.
    fn agent_status(&self, user: usize) -> Option<String> {
        self.running.keys().find_map(|&id| {
            let (conv, ix) = self.ws.locate(id)?;
            let msg = self.ws.message(conv, ix);
            (msg.author == user).then(|| {
                let place = match conv {
                    Conv::Channel(c) => match self.ws.channels[c].dm {
                        Some(_) => "DM".to_string(),
                        None => format!("#{}", self.ws.channels[c].name),
                    },
                    Conv::Thread(t) => format!("🧵 {}", self.ws.threads[t].name),
                };
                let doing = msg.activity.clone().unwrap_or_else(|| "Writing…".into());
                format!("{place} · {doing}")
            })
        })
    }

    /// What an agent's replies have cost so far.
    fn agent_spend(&self, user: usize) -> Option<f64> {
        let costs = self
            .ws
            .channels
            .iter()
            .flat_map(|c| &c.messages)
            .chain(self.ws.threads.iter().flat_map(|t| &t.messages))
            .filter(|m| m.author == user)
            .filter_map(|m| m.cost);
        let total: f64 = costs.sum();
        (total > 0.0).then_some(total)
    }

    /// Show a message that was just added, wherever its conversation is visible.
    fn message_added(&mut self, conv: Conv, ix: usize) {
        match conv {
            Conv::Channel(c) if c == self.active => {
                self.main.list.splice(ix..ix, 1);
                self.main.list.scroll_to_reveal_item(ix);
            }
            Conv::Channel(c) => self.ws.channels[c].unread = true,
            Conv::Thread(t) => {
                if self.panel == Panel::Thread(t) {
                    self.side.list.splice(ix..ix, 1);
                    self.side.list.scroll_to_reveal_item(ix);
                }
                self.thread_changed(t);
            }
        }
    }

    /// Re-measure message `id`, and the card of the thread it's in.
    fn message_changed(&mut self, id: usize) {
        self.refresh_message(id);
        if let Some((Conv::Thread(t), _)) = self.ws.locate(id) {
            self.thread_changed(t);
        }
    }

    /// A thread got a reply: its card (under the starter) shows the latest one.
    fn thread_changed(&mut self, t: usize) {
        let thread = &self.ws.threads[t];
        let channel = thread.channel;
        let starter = self.ws.channels[channel].messages[thread.starter].id;
        if channel == self.active {
            self.refresh_message(starter);
        } else {
            self.ws.channels[channel].unread = true;
        }
    }

    /// More of a streaming reply: text to append, and/or what the agent is doing.
    fn reply_progress(&mut self, id: usize, text: &str, activity: Option<String>, cx: &mut Context<Self>) {
        let Some(msg) = self.ws.message_by_id_mut(id) else { return };
        if !text.is_empty() {
            msg.push_source(text);
            msg.activity = None;
        }
        if activity.is_some() {
            msg.activity = activity;
        }
        self.message_changed(id);
        cx.notify();
    }

    fn finish_reply(&mut self, id: usize, error: Option<String>, cx: &mut Context<Self>) {
        // Called from inside the task too, which must run to its end: detach, don't cancel.
        if let Some(task) = self.running.remove(&id) {
            task.detach();
        }
        let Some(msg) = self.ws.message_by_id_mut(id) else { return };
        msg.pending = false;
        msg.activity = None;
        msg.error = error;
        if msg.source.trim().is_empty() && msg.error.is_none() && msg.card.is_none() {
            msg.error = Some("The model sent an empty reply.".into());
        }
        let failed = msg.error.is_some();
        self.message_changed(id);
        // A finished reply can hand work on by @mentioning someone.
        let place = self.ws.locate(id);
        match place {
            Some((conv, ix)) if !failed => self.posted(conv, ix, cx),
            _ => self.save(),
        }
        if let Some((conv, _)) = place {
            self.release_waiting(conv, cx);
            self.release_notes(conv, cx);
        }
        cx.notify();
    }

    /// The agent chose to say nothing: its reply's message goes away.
    fn drop_reply(&mut self, id: usize, cx: &mut Context<Self>) {
        if let Some(task) = self.running.remove(&id) {
            task.detach();
        }
        let Some((conv, ix)) = self.ws.locate(id) else { return };
        if self.ws.remove(conv, ix).is_none() {
            // Someone started a thread from it meanwhile: keep it, saying so.
            let msg = self.ws.message_mut(conv, ix);
            msg.set_source("_Nothing to add._".into());
            return self.finish_reply(id, None, cx);
        }
        match conv {
            Conv::Channel(c) => {
                if c == self.active {
                    self.main.list.splice(ix..ix + 1, 0);
                }
                if let Panel::NewThread { channel, msg: Some(msg) } = &mut self.panel
                    && *channel == c
                    && *msg > ix
                {
                    *msg -= 1;
                }
            }
            Conv::Thread(t) => {
                if self.panel == Panel::Thread(t) {
                    self.side.list.splice(ix..ix + 1, 0);
                }
                self.thread_changed(t);
            }
        }
        if self.hovered == Some(id) {
            self.hovered = None;
        }
        self.save();
        self.release_waiting(conv, cx);
        self.release_notes(conv, cx);
        cx.notify();
    }

    /// Send from the thread panel. In the new-thread form this is what
    /// actually creates the thread, as in Discord.
    fn send_in_thread(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        match self.panel {
            Panel::Closed => {}
            Panel::NewThread { channel, msg } => {
                let typed = self.thread_name.read(cx).text().trim().to_string();
                let name = match (typed.is_empty(), msg) {
                    (false, _) => typed,
                    (true, Some(msg)) => data::default_thread_name(&self.ws.channels[channel].messages[msg]),
                    (true, None) => NEW_THREAD.to_string(),
                };
                let t = match msg {
                    Some(msg) => {
                        let t = self.ws.create_thread(channel, msg, &name);
                        // The starter now carries a thread card.
                        if channel == self.active {
                            self.main.list.splice(msg..msg + 1, 1);
                        }
                        t
                    }
                    None => {
                        // Only announced: the starter is just the name, so
                        // nobody is asked to answer it.
                        let (starter, t) = self.ws.new_thread(channel, self.ws.me, &name);
                        self.message_added(Conv::Channel(channel), starter);
                        t
                    }
                };
                let ix = self.ws.post(Conv::Thread(t), self.ws.me, text);
                self.thread_name.update(cx, |c, cx| c.set_text("", cx));
                self.open_thread(t, window, cx);
                self.posted(Conv::Thread(t), ix, cx);
            }
            Panel::Thread(t) => {
                let ix = self.ws.post(Conv::Thread(t), self.ws.me, text);
                self.side.list.splice(ix..ix, 1);
                self.side.list.scroll_to_reveal_item(ix);
                self.posted(Conv::Thread(t), ix, cx);
                cx.notify();
            }
        }
    }

    fn toggle_reaction(&mut self, pane: PaneId, msg: usize, reaction: usize, cx: &mut Context<Self>) {
        let message = self.pane_message_mut(pane, msg);
        let r = &mut message.reactions[reaction];
        if r.mine {
            r.mine = false;
            r.count -= 1;
        } else {
            r.mine = true;
            r.count += 1;
        }
        message.reactions.retain(|r| r.count > 0);
        // Height can change when the last reaction disappears.
        let id = message.id;
        self.refresh_message(id);
        self.save();
        cx.notify();
    }

    // --- selection -------------------------------------------------------

    fn on_select_down(&mut self, pane: PaneId, e: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.pane(pane).focus, cx);
        let other = match pane {
            PaneId::Main => PaneId::Side,
            PaneId::Side => PaneId::Main,
        };
        self.pane(other).selection.borrow_mut().clear();
        let mut sel = self.pane(pane).selection.borrow_mut();
        let Some(pos) = sel.hit(e.position) else {
            sel.clear();
            cx.notify();
            return;
        };
        match e.click_count {
            2 | 3 => {
                if let Some((a, h)) = sel.expand(pos, e.click_count == 3) {
                    sel.anchor = Some(a);
                    sel.head = Some(h);
                }
            }
            _ if e.modifiers.shift && sel.anchor.is_some() => sel.head = Some(pos),
            _ => {
                sel.anchor = Some(pos);
                sel.head = Some(pos);
            }
        }
        sel.dragging = true;
        cx.notify();
    }

    fn on_select_move(&mut self, pane: PaneId, e: &MouseMoveEvent, cx: &mut Context<Self>) {
        let mut sel = self.pane(pane).selection.borrow_mut();
        if !sel.dragging {
            return;
        }
        if e.pressed_button != Some(MouseButton::Left) {
            sel.dragging = false;
            return;
        }
        let head = sel.hit(e.position);
        if head.is_some() && head != sel.head {
            sel.head = head;
            cx.notify();
        }
    }

    fn on_select_up(&mut self, pane: PaneId, e: &MouseUpEvent, cx: &mut Context<Self>) {
        let mut sel = self.pane(pane).selection.borrow_mut();
        let was_dragging = std::mem::replace(&mut sel.dragging, false);
        // A plain click (no text selected) on a link opens it.
        if was_dragging && e.click_count == 1 && sel.range().is_none()
            && let Some(url) = sel.link_at(e.position)
        {
            cx.open_url(&url);
        }
    }

    fn selected_text(&self, pane: PaneId) -> Option<String> {
        let (start, end) = self.pane(pane).selection.borrow().range()?;
        Some(selection_text(&self.ws, &self.pane_messages(pane), start, end))
    }

    fn copy_selection(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text(pane) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn select_all_messages(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        let len = self.pane_len(pane);
        if len == 0 {
            return;
        }
        let frags = markdown::fragments(&self.pane_message(pane, len - 1).blocks);
        let mut sel = self.pane(pane).selection.borrow_mut();
        sel.anchor = Some(Pos { msg: 0, frag: 0, offset: 0 });
        sel.head = Some(Pos {
            msg: len - 1,
            frag: frags.len().saturating_sub(1),
            offset: frags.last().map_or(0, |f| f.text.len()),
        });
        cx.notify();
    }

    fn clear_selection(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        self.pane(pane).selection.borrow_mut().clear();
        cx.notify();
    }

    /// Like Slack: typing while a message list has focus goes to its composer.
    fn on_messages_key(&mut self, pane: PaneId, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let m = &e.keystroke.modifiers;
        if m.platform || m.control || m.function {
            return;
        }
        let Some(text) = e.keystroke.key_char.clone() else { return };
        if text.chars().any(char::is_control) {
            return;
        }
        let composer = self.pane_composer(pane).clone();
        window.focus(&composer.read(cx).focus_handle(cx), cx);
        composer.update(cx, |c, cx| c.replace_text_in_range(None, &text, window, cx));
        cx.stop_propagation();
    }

    // --- rendering -------------------------------------------------------

    fn render_topbar(&self, cx: &Context<Self>) -> impl IntoElement {
        let search_box = div()
            .id("search-box")
            .w(px(SEARCH_WIDTH))
            .h(px(26.))
            // The top bar drags the window: a press here is for the box.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    if this.search.is_none() {
                        this.open_search(window, cx);
                    }
                }),
            );
        let search_box = match self.search {
            Some(_) => search_box
                // Up and Down pick a result rather than move the cursor.
                .capture_action(cx.listener(|this, _: &composer::Up, _, cx| {
                    if !this.search_box.read(cx).is_composing() {
                        cx.stop_propagation();
                        this.move_hit(-1, cx);
                    }
                }))
                .capture_action(cx.listener(|this, _: &composer::Down, _, cx| {
                    if !this.search_box.read(cx).is_composing() {
                        cx.stop_propagation();
                        this.move_hit(1, cx);
                    }
                }))
                .on_key_down(cx.listener(|this, e: &KeyDownEvent, window, cx| {
                    if e.keystroke.key == "escape" && !this.search_box.read(cx).is_composing() {
                        cx.stop_propagation();
                        this.close_search(window, cx);
                    }
                }))
                .child(self.search_box.clone()),
            None => search_box
                .rounded_md()
                .bg(theme::search_bg())
                .flex()
                .items_center()
                .px_3()
                .cursor_text()
                .text_size(px(13.))
                .text_color(theme::sidebar_text())
                .child(format!("🔍  Search {}", self.ws.name)),
        };
        div()
            .h(px(40.))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::topbar())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.close_search(window, cx);
                    window.start_window_move();
                }),
            )
            .child(search_box)
    }

    /// The search results, dropped down from the top bar's search box. A
    /// click anywhere below it closes the search.
    fn render_search(&self, search: &Search, cx: &Context<Self>) -> impl IntoElement {
        let query = self.search_box.read(cx).text().trim().to_string();
        let note = |text: String| div().px_4().py_3().text_size(px(13.)).text_color(theme::muted()).child(text);
        let heading = |text: &'static str| {
            div()
                .px_4()
                .pt_2()
                .pb_1()
                .text_size(px(11.))
                .font_weight(FontWeight::BOLD)
                .text_color(theme::muted())
                .child(text)
        };
        let row = |ix: usize| {
            div()
                .id(("search-hit", ix))
                .mx_2()
                .px_2()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .map(|d| match ix == search.selected {
                    true => d.bg(rgb(0xe8f5fa)),
                    false => d.hover(|s| s.bg(theme::hover())),
                })
                .on_click(cx.listener(move |this, _, window, cx| this.open_hit(ix, window, cx)))
        };

        let mut items: Vec<AnyElement> = Vec::new();
        if query.is_empty() {
            items.push(note("Search channel and thread names, and messages. ↑↓ to choose, Return to open.".into()).into_any_element());
        } else if search.hits.is_empty() {
            items.push(note(format!("No results for “{query}”.")).into_any_element());
        }
        let mut in_messages = false;
        for (ix, hit) in search.hits.iter().enumerate() {
            match hit {
                Hit::Channel(_) | Hit::Thread(_) if ix == 0 => {
                    items.push(heading("CHANNELS AND THREADS").into_any_element())
                }
                Hit::Message(_) if !in_messages => {
                    in_messages = true;
                    items.push(heading("MESSAGES").into_any_element());
                }
                _ => {}
            }
            let item = match hit {
                Hit::Channel(c) => match self.ws.channels.get(*c) {
                    Some(ch) => {
                        let (name, kind) = match ch.dm {
                            Some(_) => (ch.name.clone(), "Direct message"),
                            None => (format!("#{}", ch.name), "Channel"),
                        };
                        row(ix)
                            .flex()
                            .items_baseline()
                            .gap_2()
                            .text_size(px(14.))
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(name))
                            .child(div().text_size(px(12.)).text_color(theme::faint()).child(kind))
                    }
                    None => continue,
                },
                Hit::Thread(t) => match self.ws.threads.get(*t) {
                    Some(thread) => row(ix)
                        .flex()
                        .items_baseline()
                        .gap_2()
                        .text_size(px(14.))
                        .child(div().font_weight(FontWeight::SEMIBOLD).child(format!("🧵 {}", thread.name)))
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(theme::faint())
                                .child(format!("Thread in #{}", self.ws.channels[thread.channel].name)),
                        ),
                    None => continue,
                },
                Hit::Message(m) => row(ix).child(self.render_message_hit(m)),
            };
            items.push(item.into_any_element());
        }

        div()
            .id("search-backdrop")
            .absolute()
            .top(px(40.))
            .left_0()
            .right_0()
            .bottom_0()
            .occlude()
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| this.close_search(window, cx)))
            .flex()
            .justify_center()
            .child(
                div()
                    .id("search-results")
                    .w(px(SEARCH_WIDTH))
                    .max_h(px(440.))
                    .h_auto()
                    .mt_1()
                    .py_1()
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .track_scroll(&search.scroll)
                    .rounded_lg()
                    .border_1()
                    .border_color(theme::border())
                    .bg(theme::bg())
                    .shadow_lg()
                    .text_color(theme::text())
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .children(items),
            )
    }

    /// A message search result: who wrote it where and when, and the text
    /// around the match, with the match in bold.
    fn render_message_hit(&self, m: &MessageHit) -> impl IntoElement {
        let author = self.ws.users.get(m.author).map_or(String::new(), |u| u.name.clone());
        let place = match m.conv {
            Conv::Channel(c) => self.ws.channels.get(c).map(|ch| match ch.dm {
                Some(_) => ch.name.clone(),
                None => format!("#{}", ch.name),
            }),
            Conv::Thread(t) => self
                .ws
                .threads
                .get(t)
                .map(|th| format!("🧵 {} in #{}", th.name, self.ws.channels[th.channel].name)),
        };
        let bold = HighlightStyle { font_weight: Some(FontWeight::BOLD), ..Default::default() };
        div()
            .flex()
            .flex_col()
            .min_w_0()
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap_2()
                    .text_size(px(12.))
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(author))
                    .children(place.map(|p| div().text_color(theme::muted()).child(p)))
                    .child(div().text_color(theme::faint()).child(data::format_time(m.ts))),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .truncate()
                    .child(StyledText::new(m.snippet.clone()).with_highlights([(m.matched.clone(), bold)])),
            )
    }

    fn render_sidebar(&self, cx: &Context<Self>) -> impl IntoElement {
        let row = |ix: usize, selected: bool, unread: bool| {
            div()
                .id(("channel", ix))
                .mx_2()
                .px_3()
                .h(px(28.))
                .flex()
                .items_center()
                .gap_2()
                .rounded_md()
                .cursor_pointer()
                .text_size(px(15.))
                .map(|d| {
                    if selected {
                        d.bg(theme::sidebar_selected()).text_color(gpui::white())
                    } else if unread {
                        d.text_color(gpui::white())
                            .font_weight(FontWeight::BOLD)
                            .hover(|s| s.bg(theme::sidebar_hover()))
                    } else {
                        d.text_color(theme::sidebar_text())
                            .hover(|s| s.bg(theme::sidebar_hover()))
                    }
                })
                .on_click(cx.listener(move |this, _, window, cx| this.select_channel(ix, window, cx)))
        };
        let heading = |label: &'static str| {
            div()
                .px_5()
                .pt_4()
                .pb_1()
                .text_size(px(14.))
                .text_color(theme::sidebar_heading())
                .child(format!("▾  {label}"))
        };

        let channels = self.ws.channels.iter().enumerate().filter(|(_, c)| c.dm.is_none());
        let dms = self.ws.channels.iter().enumerate().filter_map(|(i, c)| c.dm.map(|u| (i, c, u)));

        div()
            .w(px(260.))
            .flex_none()
            .flex()
            .flex_col()
            .bg(theme::sidebar())
            .border_r_1()
            .border_color(rgb(0x522653))
            .child(
                div()
                    .h(px(50.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_5()
                    .border_b_1()
                    .border_color(rgb(0x522653))
                    .text_color(gpui::white())
                    .child(
                        div()
                            .id("workspace-menu")
                            .ml(px(-8.))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .text_size(px(18.))
                            .font_weight(FontWeight::BLACK)
                            .hover(|s| s.bg(theme::sidebar_hover()))
                            .when(self.menu, |d| d.bg(theme::sidebar_hover()))
                            .child(format!("{} ⌄", self.ws.name))
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(cx))),
                    )
                    .child(
                        div()
                            .size(px(32.))
                            .rounded_full()
                            .bg(gpui::white())
                            .text_color(theme::sidebar())
                            .flex()
                            .items_center()
                            .justify_center()
                            .child("✎"),
                    ),
            )
            .child(
                div()
                    .id("sidebar-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .pb_4()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .pr_3()
                            .child(heading("Channels"))
                            .child(
                                div()
                                    .id("new-channel")
                                    .mt_3()
                                    .size(px(24.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .text_size(px(18.))
                                    .text_color(theme::sidebar_heading())
                                    .hover(|s| s.bg(theme::sidebar_hover()).text_color(gpui::white()))
                                    .tooltip(|_, cx| Tooltip::view("Add a channel", cx))
                                    .child("+")
                                    .on_click(cx.listener(|this, _, window, cx| this.open_channel_dialog(None, window, cx))),
                            ),
                    )
                    .children(channels.map(|(i, c)| {
                        // Unlike Slack, Discord lists a channel's active threads under it.
                        let threads = self.ws.open_threads(i);
                        let last = threads.len().saturating_sub(1);
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                row(i, i == self.active, c.unread)
                                    .child(div().w(px(14.)).text_color(if i == self.active { gpui::white().into() } else { theme::sidebar_heading() }).child("#"))
                                    .child(c.name.clone()),
                            )
                            .children(
                                threads
                                    .into_iter()
                                    .enumerate()
                                    .map(|(k, t)| self.render_thread_row(t, k == last, cx)),
                            )
                    }))
                    .child(heading("Direct messages"))
                    .children(dms.map(|(i, c, u)| {
                        let user = &self.ws.users[u];
                        let agent = user.kind == UserKind::Agent;
                        let status = agent.then(|| self.agent_status(u)).flatten();
                        let spend = agent.then(|| self.agent_spend(u)).flatten();
                        let busy = status.is_some();
                        row(i, i == self.active, c.unread)
                            .child(avatar(&user.name, user.color, 20., Some(user.online)))
                            .child(div().flex_none().child(c.name.clone()))
                            // What the agent is doing, or what it has cost so far.
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .text_size(px(12.))
                                    .text_color(theme::sidebar_heading())
                                    .when_some(status, |d, s| d.text_color(theme::busy()).child(s)),
                            )
                            .when(!busy, |d| {
                                d.when_some(spend, |d, x| {
                                    d.child(div().flex_none().text_size(px(12.)).text_color(theme::sidebar_heading()).child(cost::dollars(x)))
                                })
                            })
                    })),
            )
    }

    /// A thread nested under its channel, joined by a tree connector.
    fn render_thread_row(&self, t: usize, last: bool, cx: &Context<Self>) -> impl IntoElement {
        let selected = self.panel == Panel::Thread(t);
        let line = theme::thread_line();
        let group: SharedString = format!("thread-row-{t}").into();
        let archive = div()
            .id(("archive-thread-row", t))
            .flex_none()
            .size(px(20.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_sm()
            .text_size(px(12.))
            .text_color(theme::sidebar_heading())
            .invisible()
            .group_hover(group.clone(), |s| s.visible())
            .hover(|s| s.text_color(gpui::white()))
            .tooltip(|_, cx| Tooltip::view("Archive thread", cx))
            .child("✓")
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.set_archived(t, true, cx);
            }));
        div()
            .id(("thread", t))
            .group(group)
            .relative()
            .h(px(28.))
            .mx_2()
            .flex()
            .items_center()
            .cursor_pointer()
            .when(!last, |d| {
                d.child(div().absolute().left(px(16.)).top_0().bottom_0().border_l_1().border_color(line))
            })
            .child(
                div()
                    .absolute()
                    .left(px(16.))
                    .top(px(-4.))
                    .w(px(10.))
                    .h(px(18.))
                    .border_l_1()
                    .border_b_1()
                    .rounded_bl(px(6.))
                    .border_color(line),
            )
            .child(
                div()
                    .ml(px(30.))
                    .flex_1()
                    .min_w_0()
                    .h(px(26.))
                    .px_2()
                    .flex()
                    .items_center()
                    .rounded_md()
                    .text_size(px(14.))
                    .map(|d| {
                        if selected {
                            d.bg(theme::sidebar_selected()).text_color(gpui::white())
                        } else {
                            d.text_color(theme::sidebar_text()).hover(|s| s.bg(theme::sidebar_hover()))
                        }
                    })
                    .pr_1()
                    .gap_1()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(self.ws.threads[t].name.clone()),
                    )
                    .child(archive),
            )
            .on_click(cx.listener(move |this, _, window, cx| this.select_thread(t, window, cx)))
    }

    fn render_header(&self, cx: &Context<Self>) -> impl IntoElement {
        let channel = &self.ws.channels[self.active];
        let active = self.active;
        let settings = channel.dm.is_none().then(|| {
            div()
                .id("edit-channel")
                .flex_none()
                .size(px(28.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .cursor_pointer()
                .text_color(theme::muted())
                .hover(|s| s.bg(theme::reaction_bg()).text_color(theme::text()))
                .tooltip(|_, cx| Tooltip::view("Channel settings", cx))
                .child("⚙")
                .on_click(cx.listener(move |this, _, window, cx| this.open_channel_dialog(Some(active), window, cx)))
        });
        let create_thread = div()
            .id("header-create-thread")
            .flex_none()
            .size(px(28.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .cursor_pointer()
            .text_size(px(15.))
            .hover(|s| s.bg(theme::reaction_bg()))
            .tooltip(|_, cx| Tooltip::view("Create Thread", cx))
            .child("🧵")
            .on_click(cx.listener(|this, _, window, cx| this.start_channel_thread(window, cx)));
        let title = match channel.dm {
            Some(_) => channel.name.clone(),
            None => format!("# {}", channel.name),
        };
        let topic = render_md::inline(
            &markdown::parse(&channel.topic)
                .into_iter()
                .find_map(|b| match b {
                    markdown::Block::Paragraph(p) => Some(p),
                    _ => None,
                })
                .unwrap_or_default(),
            "topic".into(),
            &gpui::TextStyle {
                color: theme::muted(),
                ..Default::default()
            },
        );
        let members = self.ws.users.len();
        // The repository this channel's tasks work on, home folder as `~`.
        let repo = channel.repo.clone().map(|repo| {
            let home = std::env::var("HOME").unwrap_or_default();
            let repo = match repo.strip_prefix(&home) {
                Some(rest) if !home.is_empty() && rest.starts_with('/') => format!("~{rest}"),
                _ => repo,
            };
            div()
                .flex_shrink(1.)
                .min_w(px(80.))
                .max_w(px(320.))
                .overflow_hidden()
                .text_ellipsis()
                .px_2()
                .py_1()
                .rounded_md()
                .bg(theme::code_bg())
                .font_family(theme::MONO)
                .text_size(px(12.))
                .text_color(theme::muted())
                .whitespace_nowrap()
                .child(repo)
        });
        div()
            .h(px(50.))
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .px_5()
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .gap_3()
                    .min_w_0()
                    .overflow_hidden()
                    .child(div().flex_none().text_size(px(18.)).font_weight(FontWeight::BOLD).child(title))
                    .children(repo)
                    .child(div().text_size(px(13.)).whitespace_nowrap().overflow_hidden().text_ellipsis().child(topic)),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_2()
                    .child(create_thread)
                    .children(settings)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(theme::border())
                            .text_size(px(13.))
                            .text_color(theme::muted())
                            .children(self.ws.users.iter().take(3).map(|u| avatar(&u.name, u.color, 20., None)))
                            .child(div().pl_1().child(members.to_string())),
                    ),
            )
    }

    /// A selectable message list with its key and mouse handling.
    fn render_pane(&self, id: PaneId, cx: &Context<Self>) -> impl IntoElement {
        let pane = self.pane(id);
        div()
            .id(match id {
                PaneId::Main => "main-messages",
                PaneId::Side => "thread-messages",
            })
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .relative()
            .key_context(MESSAGES_CONTEXT)
            .track_focus(&pane.focus)
            .on_action(cx.listener(move |this, _: &CopySelection, _, cx| this.copy_selection(id, cx)))
            .on_action(cx.listener(move |this, _: &SelectAllMessages, _, cx| this.select_all_messages(id, cx)))
            .on_action(cx.listener(move |this, _: &ClearSelection, _, cx| this.clear_selection(id, cx)))
            .on_key_down(cx.listener(move |this, e, window, cx| this.on_messages_key(id, e, window, cx)))
            .on_mouse_down(MouseButton::Left, cx.listener(move |this, e, window, cx| this.on_select_down(id, e, window, cx)))
            .on_mouse_move(cx.listener(move |this, e, _, cx| this.on_select_move(id, e, cx)))
            .on_mouse_up(MouseButton::Left, cx.listener(move |this, e, _, cx| this.on_select_up(id, e, cx)))
            .on_mouse_up_out(MouseButton::Left, cx.listener(move |this, e, _, cx| this.on_select_up(id, e, cx)))
            .when(id == PaneId::Main, |d| {
                d.on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                    if !hovered && this.hovered.take().is_some() {
                        cx.notify();
                    }
                }))
            })
            // Runs before the list prepaints: drop last frame's fragment geometry.
            .child({
                let selection = pane.selection.clone();
                canvas(move |_, _, _| selection.borrow_mut().begin_frame(), |_, _, _, _| {})
                    .absolute()
                    .size_0()
            })
            .child(
                list(
                    pane.list.clone(),
                    cx.processor(move |this, ix, window, cx| this.render_message(id, ix, window, cx)),
                )
                .flex_1()
                .pb_2(),
            )
    }

    fn render_message(&mut self, pane: PaneId, ix: usize, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let msg = self.pane_message(pane, ix);
        let user = &self.ws.users[msg.author];
        // In a thread, item 0 is the starter and the replies start fresh below it.
        let in_thread = pane == PaneId::Side && matches!(self.panel, Panel::Thread(_));
        let grouped = ix > 0 && !(pane == PaneId::Side && ix == 1) && {
            let prev = self.pane_message(pane, ix - 1);
            prev.author == msg.author && msg.ts - prev.ts < 5 * 60
        };
        let time = data::format_time(msg.ts);
        let group: SharedString = format!("msg-{}", msg.id).into();
        let id = msg.id;

        let gutter = if grouped {
            div()
                .w(px(36.))
                .flex_none()
                .pt(px(3.))
                .text_size(px(11.))
                .text_color(gpui::transparent_black())
                .group_hover(group.clone(), |s| s.text_color(theme::faint()))
                .child(time.split(' ').next().unwrap_or_default().to_string())
                .into_any_element()
        } else {
            div()
                .flex_none()
                .pt(px(2.))
                .child(avatar(&user.name, user.color, 36., None))
                .into_any_element()
        };

        let reactions = msg.reactions.iter().enumerate().map(|(r, reaction)| {
            div()
                .id(("reaction", msg.id * 64 + r))
                .flex()
                .items_center()
                .gap_1()
                .px_2()
                .h(px(24.))
                .rounded_full()
                .border_1()
                .cursor_pointer()
                .text_size(px(12.))
                .map(|d| {
                    if reaction.mine {
                        d.bg(theme::reaction_mine_bg())
                            .border_color(theme::reaction_mine_border())
                            .text_color(theme::link())
                    } else {
                        d.bg(theme::reaction_bg())
                            .border_color(theme::reaction_bg())
                            .text_color(theme::muted())
                            .hover(|s| s.border_color(theme::faint()))
                    }
                })
                .child(div().text_size(px(14.)).child(reaction.emoji.clone()))
                .child(div().font_weight(FontWeight::SEMIBOLD).child(reaction.count.to_string()))
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_reaction(pane, ix, r, cx)))
        });

        let thread_card = (pane == PaneId::Main)
            .then_some(msg.thread)
            .flatten()
            .map(|t| self.render_thread_card(t, cx));
        // Discord's hover action bar; threads can't be nested, so only in the channel.
        let toolbar = (pane == PaneId::Main && self.hovered == Some(id) && msg.thread.is_none())
            .then(|| render_toolbar(ix, cx));

        let message = div()
            .id(("message", msg.id))
            .group(group)
            .relative()
            .w_full()
            .flex()
            .gap_2()
            .px_5()
            .pt(if grouped { px(2.) } else { px(8.) })
            .pb(px(2.))
            .hover(|s| s.bg(theme::hover()))
            // Opened from search.
            .when(self.highlight == Some(id), |d| d.bg(rgb(0xfdf3d0)))
            .when(pane == PaneId::Main, |d| {
                d.on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                    // Leaving is handled by the next message or the list itself, so
                    // the action bar (which overhangs the message) stays reachable.
                    if *hovered && this.hovered != Some(id) {
                        this.hovered = Some(id);
                        cx.notify();
                    }
                }))
            })
            .child(gutter)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .text_size(px(15.))
                    .line_height(px(22.))
                    .text_color(theme::text())
                    .when(!grouped, |d| {
                        d.child(
                            div()
                                .flex()
                                .items_baseline()
                                .gap_2()
                                .child(div().font_weight(FontWeight::BOLD).child(user.name.clone()))
                                .when_some(badge(user.kind), |d, label| {
                                    d.child(
                                        div()
                                            .px_1()
                                            .rounded_sm()
                                            .bg(theme::reaction_bg())
                                            .text_size(px(10.))
                                            .text_color(theme::muted())
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(label),
                                    )
                                })
                                .child(div().text_size(px(12.)).text_color(theme::faint()).child(time)),
                        )
                    })
                    .children(render_md::blocks(
                        &msg.blocks,
                        &mut render_md::Ctx::new(ix, self.pane(pane).selection.clone()),
                    ))
                    .children(msg.card.as_ref().map(|card| div().w_full().flex().child(self.render_card(id, card, cx))))
                    .when(msg.pending, |d| {
                        let status = msg.activity.clone().unwrap_or_else(|| {
                            if msg.source.is_empty() { "Thinking…".into() } else { "Writing…".into() }
                        });
                        d.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .text_size(px(13.))
                                .child(div().italic().text_color(theme::faint()).child(status))
                                .when(self.running.contains_key(&id), |d| {
                                    d.child(
                                        div()
                                            .id(("stop", id))
                                            .px_2()
                                            .rounded_md()
                                            .border_1()
                                            .border_color(theme::border())
                                            .cursor_pointer()
                                            .text_size(px(12.))
                                            .text_color(theme::muted())
                                            .hover(|s| s.border_color(theme::error()).text_color(theme::error()))
                                            .child("■ Stop")
                                            .on_click(cx.listener(move |this, _, _, cx| this.stop_reply(id, cx))),
                                    )
                                }),
                        )
                    })
                    .when(!msg.pending && (msg.usage.is_some() || msg.worker_usage.is_some()), |d| {
                        d.child(div().text_size(px(11.)).text_color(theme::faint()).child(usage_line(msg)))
                    })
                    .when_some(msg.error.clone(), |d, error| {
                        d.child(div().text_size(px(13.)).text_color(theme::error()).child(format!("⚠ {error}")))
                    })
                    .when_some(msg.note.clone(), |d, note| {
                        d.child(div().text_size(px(13.)).italic().text_color(theme::muted()).child(note))
                    })
                    .when(!msg.reactions.is_empty(), |d| {
                        d.child(div().flex().flex_wrap().gap_1().pt_1().children(reactions))
                    })
                    .children(thread_card),
            )
            .children(toolbar);

        if in_thread && ix == 0 {
            div()
                .flex()
                .flex_col()
                .child(message)
                .child(div().mx_5().mt_2().mb_1().border_b_1().border_color(theme::border()))
                .into_any_element()
        } else {
            message.into_any_element()
        }
    }

    /// An agent's card: a question with option buttons, or a merge request.
    fn render_card(&self, id: usize, card: &Card, cx: &Context<Self>) -> AnyElement {
        // Card text is markdown too. It isn't part of the message's selectable
        // text, so it renders against a selection of its own.
        let md = |key: usize, src: &str| {
            let selection: SharedSelection = Rc::new(RefCell::new(Selection::default()));
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .children(render_md::blocks(&markdown::parse(src), &mut render_md::Ctx::new(usize::MAX - id * 4 - key, selection)))
        };
        let button = |key: usize, label: String, primary: bool| {
            div()
                .id(("card-button", id * 8 + key))
                .px_3()
                .py(px(3.))
                .rounded_md()
                .border_1()
                .cursor_pointer()
                .text_size(px(13.))
                .font_weight(FontWeight::SEMIBOLD)
                .map(|d| {
                    if primary {
                        d.bg(theme::send()).border_color(theme::send()).text_color(gpui::white()).hover(|s| s.bg(theme::send_hover()))
                    } else {
                        d.bg(theme::bg()).border_color(theme::border()).text_color(theme::text()).hover(|s| s.border_color(theme::faint()))
                    }
                })
                .child(label)
        };
        // 520px, or less when the column is narrower. Text inside is measured
        // against the width it's given, not a max_w, so a capped width would
        // wrap more when drawn than when measured and the card would come out
        // too short. As a shrinking item of the row it's put in, the card has
        // its final width before its height is worked out.
        let frame = div()
            .mt_1()
            .flex_basis(px(520.))
            .flex_shrink(1.)
            .min_w_0()
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .rounded_lg()
            .border_1()
            .border_color(theme::thread_card_border())
            .bg(theme::thread_card())
            .text_size(px(14.))
            .line_height(px(20.));
        let muted = |text: String| div().text_size(px(13.)).text_color(theme::muted()).child(text);

        match card {
            Card::Choice { question, options, recommended, answer } => frame
                .child(div().flex().gap_2().child(div().flex_none().child("❓")).child(md(0, &format!("**{question}**")).flex_1().min_w_0()))
                .child(match answer {
                    Some(a) => muted(format!("You chose: {}", options.get(*a).cloned().unwrap_or_default())).into_any_element(),
                    None => div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .children(options.iter().enumerate().map(|(i, option)| {
                            let label = if *recommended == Some(i) { format!("★ {option}") } else { option.clone() };
                            button(i, label, *recommended == Some(i))
                                .on_click(cx.listener(move |this, _, _, cx| this.answer_choice(id, i, cx)))
                        }))
                        .into_any_element(),
                })
                .into_any_element(),
            Card::Merge { branch, summary, commits, outcome, conflicts, .. } => {
                const SHOWN: usize = 8;
                let busy = self.merging.contains(&id);
                frame
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().font_weight(FontWeight::BOLD).child("🔀 Merge request"))
                            .child(div().font_family(theme::MONO).text_size(px(12.)).text_color(theme::code_fg()).child(branch.clone())),
                    )
                    .when(!summary.is_empty(), |d| d.child(md(1, summary)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .font_family(theme::MONO)
                            .text_size(px(12.))
                            .line_height(px(17.))
                            .text_color(theme::muted())
                            .children(commits.iter().take(SHOWN).map(|c| div().child(c.clone())))
                            .when(commits.len() > SHOWN, |d| d.child(format!("… and {} more", commits.len() - SHOWN))),
                    )
                    .when_some(conflicts.as_ref(), |d, c| {
                        d.child(md(3, &format!("⚠ **Conflicts with other work** (when it was requested):\n\n{c}")))
                    })
                    .map(|d| match outcome {
                        Some(MergeOutcome::Approved(tip)) => d.child(md(
                            2,
                            &format!("✓ Approved: `{branch}` (`{tip}`) is now a branch in the project repo. Merge it from there."),
                        )),
                        Some(MergeOutcome::Rejected) => d.child(muted("✗ Rejected".into())),
                        failed => d
                            .when_some(
                                match failed {
                                    Some(MergeOutcome::Failed(e)) => Some(e.clone()),
                                    _ => None,
                                },
                                |d, e| d.child(div().text_size(px(13.)).text_color(theme::error()).child(format!("⚠ Couldn't bring the branch in: {e}"))),
                            )
                            .child(if busy {
                                muted("Fetching the branch…".into()).into_any_element()
                            } else {
                                div()
                                    .flex()
                                    .gap_2()
                                    .child(button(0, "Approve".into(), true).on_click(cx.listener(move |this, _, _, cx| this.answer_merge(id, true, cx))))
                                    .child(button(1, "Reject".into(), false).on_click(cx.listener(move |this, _, _, cx| this.answer_merge(id, false, cx))))
                                    .into_any_element()
                            }),
                    })
                    .into_any_element()
            }
        }
    }

    /// Discord's thread preview under a starter message: name, reply count
    /// and the latest reply.
    fn render_thread_card(&self, t: usize, cx: &Context<Self>) -> impl IntoElement {
        let thread = &self.ws.threads[t];
        let count = thread.messages.len();
        let open = self.panel == Panel::Thread(t);
        let latest = match thread.messages.last() {
            Some(last) => {
                let u = &self.ws.users[last.author];
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .text_size(px(13.))
                    .line_height(px(18.))
                    .child(avatar(&u.name, u.color, 16., None))
                    .child(div().flex_none().font_weight(FontWeight::SEMIBOLD).text_color(rgb(u.color)).child(u.name.clone()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_color(theme::muted())
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(last.snippet(120)),
                    )
                    .child(div().flex_none().text_size(px(12.)).text_color(theme::faint()).child(data::format_time(last.ts)))
                    .into_any_element()
            }
            None => div()
                .text_size(px(13.))
                .text_color(theme::muted())
                .italic()
                .child("No recent messages")
                .into_any_element(),
        };
        div()
            .id(("thread-card", t))
            .mt_1()
            .max_w(px(480.))
            .px_3()
            .py_2()
            .flex()
            .flex_col()
            .gap_1()
            .rounded_lg()
            .border_1()
            .bg(theme::thread_card())
            .cursor_pointer()
            .map(|d| {
                if open {
                    d.border_color(theme::link())
                } else {
                    d.border_color(theme::thread_card_border()).hover(|s| s.border_color(theme::faint()))
                }
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .text_size(px(14.))
                    .line_height(px(20.))
                    .child(
                        div()
                            .min_w_0()
                            .font_weight(FontWeight::BOLD)
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(thread.name.clone()),
                    )
                    .when(thread.archived, |d| d.child(archived_tag()))
                    .child(
                        div()
                            .flex_none()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::link())
                            .child(format!("{count} {} ›", if count == 1 { "Message" } else { "Messages" })),
                    ),
            )
            .child(latest)
            .on_click(cx.listener(move |this, _, window, cx| this.open_thread(t, window, cx)))
    }

    /// The workspace menu, dropped down from the workspace name. A click
    /// anywhere else closes it.
    fn render_menu(&self, cx: &Context<Self>) -> impl IntoElement {
        let item = |id: &'static str, text: &'static str, shortcut: Option<&'static str>| {
            div()
                .id(id)
                .mx_2()
                .px_3()
                .h(px(30.))
                .flex()
                .items_center()
                .justify_between()
                .rounded_md()
                .cursor_pointer()
                .text_size(px(14.))
                .hover(|s| s.bg(theme::sidebar_selected()).text_color(gpui::white()))
                .child(text)
                .children(shortcut.map(|k| div().text_size(px(12.)).text_color(theme::faint()).child(k)))
        };
        div()
            .id("menu-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| this.toggle_menu(cx)))
            .child(
                div()
                    .id("workspace-menu-items")
                    .absolute()
                    .top(px(84.))
                    .left(px(12.))
                    .w(px(260.))
                    .py_2()
                    .flex()
                    .flex_col()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme::border())
                    .bg(theme::bg())
                    .shadow_lg()
                    .text_color(theme::text())
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .px_5()
                            .pt_1()
                            .pb_2()
                            .mb_1()
                            .border_b_1()
                            .border_color(theme::border())
                            .text_size(px(15.))
                            .font_weight(FontWeight::BOLD)
                            .child(self.ws.name.clone()),
                    )
                    .child(
                        item("menu-settings", "Settings…", Some("⌘,"))
                            .on_click(cx.listener(|this, _, window, cx| this.open_settings(window, cx))),
                    )
                    .child(item("menu-config", "Open config.toml", None).on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_menu(cx);
                        cx.open_with_system(&store::config_path());
                    }))),
            )
    }

    /// The new/edit channel dialog, over everything else.
    fn render_channel_dialog(&self, dialog: &ChannelDialog, cx: &Context<Self>) -> impl IntoElement {
        let title = match dialog.editing {
            Some(c) => format!("#{} settings", self.ws.channels[c].name),
            None => "Create a channel".to_string(),
        };
        // What config.toml sets for this channel comes back on every launch.
        let config_note = dialog
            .editing
            .and_then(|c| self.agents.channel_config(&self.ws, c))
            .filter(|cc| cc.repo.is_some() || cc.topic.is_some())
            .map(|cc| {
                let fields = match (cc.repo.is_some(), cc.topic.is_some()) {
                    (true, true) => "repo and topic",
                    (true, false) => "repo",
                    _ => "topic",
                };
                format!("config.toml sets this channel's {fields}: change it there, or it comes back when Pots restarts.")
            });
        let label = |text: &'static str| {
            div().text_size(px(12.)).font_weight(FontWeight::BOLD).text_color(theme::muted()).child(text)
        };
        let hint = |text: String| div().text_size(px(12.)).text_color(theme::muted()).child(text);
        let button = |id: &'static str, text: &'static str, primary: bool| {
            div()
                .id(id)
                .px_4()
                .py(px(6.))
                .rounded_md()
                .cursor_pointer()
                .text_size(px(14.))
                .font_weight(FontWeight::BOLD)
                .map(|d| match primary {
                    true => d.bg(theme::send()).text_color(gpui::white()).hover(|s| s.bg(theme::send_hover())),
                    false => d.border_1().border_color(theme::border()).hover(|s| s.bg(theme::hover())),
                })
                .child(text)
        };
        let repo_hint = match &self.agents.project {
            Some(_) => "Tasks in this channel clone this git repository. Leave it empty to use [project] from config.toml.",
            None => "Tasks in this channel clone this git repository. Without one, engineers here have no workspace.",
        };
        div()
            .id("channel-dialog")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::black().opacity(0.4))
            .occlude()
            .child(
                div()
                    .w(px(480.))
                    .flex()
                    .flex_col()
                    .gap_4()
                    .p_6()
                    .rounded_lg()
                    .bg(theme::bg())
                    .shadow_lg()
                    .on_key_down(cx.listener(|this, e: &KeyDownEvent, window, cx| {
                        if e.keystroke.key == "escape" {
                            this.close_channel_dialog(window, cx);
                        }
                    }))
                    .child(div().text_size(px(20.)).font_weight(FontWeight::BOLD).child(title))
                    .child(div().flex().flex_col().gap_1().child(label("NAME")).child(self.channel_name.clone()))
                    .child(div().flex().flex_col().gap_1().child(label("TOPIC")).child(self.channel_topic.clone()))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(label("REPOSITORY"))
                            .child(self.channel_repo.clone())
                            .child(hint(repo_hint.to_string())),
                    )
                    .children(config_note.map(hint))
                    .children(dialog.error.clone().map(|e| div().text_size(px(13.)).text_color(theme::error()).child(e)))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                button("channel-cancel", "Cancel", false)
                                    .on_click(cx.listener(|this, _, window, cx| this.close_channel_dialog(window, cx))),
                            )
                            .child(
                                button("channel-save", if dialog.editing.is_some() { "Save" } else { "Create" }, true)
                                    .on_click(cx.listener(|this, _, window, cx| this.save_channel(window, cx))),
                            ),
                    ),
            )
    }

    fn render_panel(&self, cx: &Context<Self>) -> impl IntoElement {
        let (title, channel) = match self.panel {
            Panel::NewThread { channel, .. } => ("New Thread".to_string(), channel),
            Panel::Thread(t) => (self.ws.threads[t].name.clone(), self.ws.threads[t].channel),
            Panel::Closed => unreachable!("render_panel called with the panel closed"),
        };
        let new = matches!(self.panel, Panel::NewThread { .. });
        // Archive ("close the case") or reopen the thread shown.
        let archive = match self.panel {
            Panel::Thread(t) => {
                let archived = self.ws.threads[t].archived;
                Some(
                    div()
                        .id("archive-thread")
                        .flex_none()
                        .h(px(28.))
                        .px_2()
                        .flex()
                        .items_center()
                        .rounded_md()
                        .cursor_pointer()
                        .text_size(px(13.))
                        .text_color(theme::muted())
                        .hover(|s| s.bg(theme::reaction_bg()).text_color(theme::text()))
                        .tooltip(move |_, cx| {
                            Tooltip::view(if archived { "Bring it back to the sidebar" } else { "Hide it from the sidebar" }, cx)
                        })
                        .child(if archived { "Unarchive" } else { "Archive" })
                        .on_click(cx.listener(move |this, _, _, cx| this.set_archived(t, !archived, cx))),
                )
            }
            _ => None,
        };
        let archived = matches!(self.panel, Panel::Thread(t) if self.ws.threads[t].archived);
        div()
            .w(px(400.))
            .flex_none()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(theme::panel_border())
            .child(
                div()
                    .h(px(50.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .pl_4()
                    .pr_2()
                    .border_b_1()
                    .border_color(theme::border())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .min_w_0()
                            .child(div().flex_none().text_size(px(16.)).child("🧵"))
                            .child(
                                div()
                                    .min_w_0()
                                    .text_size(px(16.))
                                    .font_weight(FontWeight::BOLD)
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .child(title),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(13.))
                                    .text_color(theme::muted())
                                    .child(format!("# {}", self.ws.channels[channel].name)),
                            )
                            .when(archived, |d| d.child(archived_tag())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap_1()
                            .children(archive)
                            .child(
                                div()
                                    .id("close-thread")
                                    .flex_none()
                                    .size(px(28.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .text_color(theme::muted())
                                    .hover(|s| s.bg(theme::reaction_bg()).text_color(theme::text()))
                                    .tooltip(|_, cx| Tooltip::view("Close", cx))
                                    .child("✕")
                                    .on_click(cx.listener(|this, _, window, cx| this.close_panel(window, cx))),
                            ),
                    ),
            )
            .when(new, |d| {
                d.child(
                    div()
                        .flex_none()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .px_4()
                        .pt_5()
                        .pb_3()
                        .child(
                            div()
                                .size(px(56.))
                                .rounded_full()
                                .bg(theme::thread_card())
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(px(26.))
                                .child("🧵"),
                        )
                        .child(
                            div()
                                .pt_2()
                                .text_size(px(12.))
                                .font_weight(FontWeight::BOLD)
                                .text_color(theme::muted())
                                .child("THREAD NAME"),
                        )
                        .child(self.thread_name.clone()),
                )
            })
            .child(self.render_pane(PaneId::Side, cx))
            .child(div().flex_none().px_4().pb_5().child(self.thread_composer.clone()))
    }
}

/// Shows an agent reply in message `id` as it runs.
struct UiHost<'a> {
    app: WeakEntity<ChatApp>,
    cx: &'a mut AsyncApp,
    id: usize,
    agent: usize,
}

impl UiHost<'_> {
    fn update<T>(&mut self, f: impl FnOnce(&mut ChatApp, &mut Context<ChatApp>) -> T) -> Result<T, String> {
        self.app.update(self.cx, f).map_err(|_| "The app closed the conversation.".to_string())
    }
}

impl Host for UiHost<'_> {
    fn progress(&mut self, text: &str, activity: Option<String>) -> Result<(), String> {
        let id = self.id;
        self.update(|app, cx| app.reply_progress(id, text, activity, cx))
    }

    fn usage(&mut self, total: Usage) -> Result<(), String> {
        let (id, agent) = (self.id, self.agent);
        self.update(|app, cx| app.reply_usage(id, agent, total, false, cx))
    }

    fn worker_usage(&mut self, total: Usage) -> Result<(), String> {
        let (id, agent) = (self.id, self.agent);
        self.update(|app, cx| app.reply_usage(id, agent, total, true, cx))
    }

    fn card(&mut self, card: Card) -> Result<(), String> {
        let id = self.id;
        self.update(|app, cx| app.attach_card(id, card, cx))
    }

    fn run_tool(&mut self, name: &str, input: &serde_json::Value) -> Result<(String, bool), String> {
        let agent = self.agent;
        match name {
            agent::tools::START_TASK => self.update(|app, cx| app.start_task(agent, input, cx)),
            agent::tools::POST_IN_THREAD => self.update(|app, cx| app.post_in_thread(agent, input, cx)),
            agent::tools::TASK_DONE => {
                let id = self.id;
                self.update(|app, cx| app.task_done(id, agent, input, cx))
            }
            agent::tools::RECORD_DECISION => {
                let id = self.id;
                self.update(|app, cx| app.record_decision(id, agent, input, cx))
            }
            _ => self.update(|app, _| agent::tools::run(&app.ws, name, input)),
        }
    }
}

/// What a reply used: "6.3k in · 1.2k out · Worker 30k in · 2k out · $0.01".
fn usage_line(msg: &Message) -> String {
    let mut parts = Vec::new();
    if let Some(u) = &msg.usage {
        parts.push(cost::summary(u, None));
    }
    if let Some(w) = &msg.worker_usage {
        parts.push(format!("Worker {}", cost::summary(w, None)));
    }
    if let Some(c) = msg.cost {
        parts.push(cost::dollars(c));
    }
    parts.join(" · ")
}

/// The small "Archived" label on an archived thread's card and panel.
fn archived_tag() -> impl IntoElement {
    div()
        .flex_none()
        .px(px(6.))
        .rounded_sm()
        .bg(theme::reaction_bg())
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::muted())
        .child("Archived")
}

/// The action bar that floats over the top-right of a hovered message.
fn render_toolbar(ix: usize, cx: &Context<ChatApp>) -> impl IntoElement {
    div()
        .id("message-actions")
        .absolute()
        .top(px(-14.))
        .right(px(20.))
        .flex()
        .p(px(2.))
        .rounded_md()
        .border_1()
        .border_color(theme::border())
        .bg(theme::toolbar_bg())
        .shadow_sm()
        // Keep the message underneath from stealing hover (and clicks) from the bar.
        .block_mouse_except_scroll()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(
            div()
                .id("create-thread")
                .size(px(30.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .cursor_pointer()
                .text_size(px(16.))
                .hover(|s| s.bg(theme::reaction_bg()))
                .tooltip(|_, cx| Tooltip::view("Create Thread", cx))
                .child("🧵")
                .on_click(cx.listener(move |this, _, window, cx| this.start_thread(ix, window, cx))),
        )
}

fn badge(kind: UserKind) -> Option<&'static str> {
    match kind {
        UserKind::Human => None,
        UserKind::Bot => Some("APP"),
        UserKind::Agent => Some("AI"),
    }
}

struct Tooltip(SharedString);

impl Tooltip {
    fn view(text: &'static str, cx: &mut gpui::App) -> AnyView {
        cx.new(|_| Tooltip(text.into())).into()
    }
}

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(theme::topbar())
            .text_color(gpui::white())
            .text_size(px(12.))
            .font_weight(FontWeight::SEMIBOLD)
            .font_family(".SystemUIFont")
            .child(self.0.clone())
    }
}

fn avatar(name: &str, color: u32, size: f32, online: Option<bool>) -> impl IntoElement {
    let initials: String = name
        .split_whitespace()
        .filter_map(|w| w.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase();
    div()
        .relative()
        .flex_none()
        .size(px(size))
        .rounded(px(size * 0.22))
        .bg(rgb(color))
        .flex()
        .items_center()
        .justify_center()
        .text_color(gpui::white())
        .text_size(px(size * 0.4))
        .font_weight(FontWeight::BOLD)
        .child(initials)
        .when_some(online, |d, online| {
            d.child(
                div()
                    .absolute()
                    .bottom(px(-2.))
                    .right(px(-2.))
                    .size(px(9.))
                    .rounded_full()
                    .border_2()
                    .border_color(theme::sidebar())
                    .bg(if online { theme::online() } else { theme::sidebar() })
                    .when(!online, |d| d.border_color(theme::sidebar_text())),
            )
        })
}

impl Render for ChatApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let panel = (self.panel != Panel::Closed).then(|| self.render_panel(cx));
        let dialog = self.dialog.as_ref().map(|d| self.render_channel_dialog(d, cx));
        let menu = self.menu.then(|| self.render_menu(cx));
        let settings = self.settings.as_ref().map(|(s, _)| s.clone());
        let search = self.search.as_ref().map(|s| self.render_search(s, cx));
        div()
            .relative()
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| this.open_settings(window, cx)))
            .on_action(cx.listener(|this, _: &OpenSearch, window, cx| this.open_search(window, cx)))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::bg())
            .font_family(".SystemUIFont")
            .child(self.render_topbar(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(self.render_sidebar(cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(self.render_header(cx))
                            .child(self.render_pane(PaneId::Main, cx))
                            .child(div().flex_none().px_5().pb_5().child(self.composer.clone())),
                    )
                    .children(panel),
            )
            .children(search)
            .children(dialog)
            .children(menu)
            .children(settings)
    }
}

/// Plain-text rendition of a selection, rebuilt from message data: table
/// cells are tab-separated, list/quote markers are kept, and a selection
/// spanning several messages includes each author line.
fn selection_text(ws: &Workspace, messages: &[&Message], start: Pos, end: Pos) -> String {
    let multi = start.msg != end.msg;
    let mut out = String::new();
    for m in start.msg..=end.msg.min(messages.len().saturating_sub(1)) {
        let msg = messages[m];
        if multi {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            let author = &ws.users[msg.author].name;
            out.push_str(&format!("{author}  {}\n", data::format_time(msg.ts)));
        }
        let mut first = true;
        for (f, frag) in markdown::fragments(&msg.blocks).iter().enumerate() {
            let key = (m, f);
            if key < (start.msg, start.frag) || key > (end.msg, end.frag) {
                continue;
            }
            let len = frag.text.len();
            let from = if key == (start.msg, start.frag) { start.offset.min(len) } else { 0 };
            let to = if key == (end.msg, end.frag) { end.offset.min(len) } else { len };
            if !first {
                out.push(if frag.sep == Sep::Tab { '\t' } else { '\n' });
            }
            if from == 0 {
                out.push_str(&frag.prefix);
            }
            out.push_str(frag.text.get(from..to).unwrap_or_default());
            first = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(msg: usize, frag: usize, offset: usize) -> Pos {
        Pos { msg, frag, offset }
    }

    #[test]
    fn copies_paragraph_table_and_list() {
        let ws = data::seed();
        let eng = ws.channels.iter().position(|c| c.name == "engineering").unwrap();
        // Message 4: paragraph (frag 0), 4x4 table (1..=16), "Follow-ups:" (17), list (18..=20).
        let text = selection_text(&ws, &ws.messages(Conv::Channel(eng)), pos(4, 0, 5), pos(4, 19, 8));
        assert_eq!(
            text,
            "fixed it 🎉 Benchmarks before/after:\n\
             Batch size\tDocs/sec\tLock waits\tResult\n\
             10,000\t0\t312\tstuck\n\
             5,000\t4,120\t18\tslow\n\
             2,500\t9,870\t0\t✅ done\n\
             Follow-ups:\n\
             1. Make 2500 the default\n\
             2. Add an a"
        );
    }

    #[test]
    fn multi_message_copy_includes_authors() {
        let ws = data::seed();
        let general = ws.channels.iter().position(|c| c.name == "general").unwrap();
        let text = selection_text(&ws, &ws.messages(Conv::Channel(general)), pos(3, 0, 0), pos(4, 0, 100));
        assert!(text.starts_with("Priya Nair  9:41 AM\nOffice update ✨\n"), "{text}");
        assert!(text.contains("• Espresso"), "{text}");
        assert!(text.ends_with("Leo Okafor  9:44 AM\n> Please descale it on Fridays"), "{text}");
    }
}
