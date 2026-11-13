//! The frame every artifact opens in: a title bar (title, renaming, delete,
//! close) over an editor for the artifact's kind, and when to save.
//!
//! The frame knows nothing about what an artifact holds. Each kind brings
//! an editor implementing [`ArtifactEditor`]; [`editor_for`] is the one
//! place that maps content to its editor. A kind's editor reports changes
//! with [`EditorEvent::Changed`]; the frame decides when they're saved.

use std::time::Duration;

use gpui::{
    AnyView, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, KeyDownEvent, Subscription,
    Task, Window, div, prelude::*, px,
};

use pots_agent::data::{ArtifactContent, ArtifactKind};

use crate::board_editor::BoardEditor;
use crate::checklist_editor::ChecklistEditor;
use crate::composer::{Composer, ComposerEvent};
use crate::document_editor::DocumentEditor;
use crate::page_view::PageView;
use crate::table_editor::TableEditor;
use crate::whiteboard_editor::WhiteboardEditor;
use crate::theme;

/// How long after a change the frame saves, so a burst of edits is one write.
const SAVE_AFTER: Duration = Duration::from_millis(400);

pub enum EditorEvent {
    /// The content changed and should be saved.
    Changed,
}

/// An editor for one kind of artifact, shown under the frame's title bar.
pub trait ArtifactEditor: Render + Focusable + EventEmitter<EditorEvent> {
    /// The content as edited so far, including an edit still in progress
    /// (a cell being typed in): saving must not need a window, since the
    /// app may be quitting.
    fn content(&self, cx: &App) -> ArtifactContent;
    /// Whether an edit in progress would change `content`; it emits no
    /// `Changed` until it is committed.
    fn has_pending_edit(&self, cx: &App) -> bool;
    /// Finish any edit in progress (a cell being typed in), so `content`
    /// has it. Called before saving on close. Returns whether that changed
    /// the content (its `Changed` would only arrive after the save).
    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool;
    /// Take the keyboard focus, where typing should go.
    fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>);
    /// Show `content` (of the editor's kind), written by someone else: an
    /// edit in progress is dropped, so it can't later write over it, unless
    /// the new content still has what it typed there (rows were only added,
    /// say), where typing goes on. No window needed: an agent's tool call
    /// has none.
    fn replace(&mut self, content: ArtifactContent, cx: &mut Context<Self>);
    /// Whether something of the app's (a dialog, a menu) is drawn over
    /// the editor. Only an editor showing a native view on top of the
    /// window (a page's web view) has to care.
    fn set_covered(&mut self, _covered: bool, _cx: &mut Context<Self>) {}
    /// What `set_covered` last said, for an editor that keeps it.
    #[cfg(test)]
    fn covered(&self) -> bool {
        false
    }
}

/// An editor of any kind, as the frame holds it.
trait AnyEditor {
    fn view(&self) -> AnyView;
    fn content(&self, cx: &App) -> ArtifactContent;
    fn has_pending_edit(&self, cx: &App) -> bool;
    fn commit(&self, window: &mut Window, cx: &mut App) -> bool;
    fn focus(&self, window: &mut Window, cx: &mut App);
    fn replace(&self, content: ArtifactContent, cx: &mut App);
    fn set_covered(&self, covered: bool, cx: &mut App);
    #[cfg(test)]
    fn covered(&self, cx: &App) -> bool;
}

impl<E: ArtifactEditor> AnyEditor for Entity<E> {
    fn view(&self) -> AnyView {
        self.clone().into()
    }

    fn content(&self, cx: &App) -> ArtifactContent {
        self.read(cx).content(cx)
    }

    fn has_pending_edit(&self, cx: &App) -> bool {
        self.read(cx).has_pending_edit(cx)
    }

    fn commit(&self, window: &mut Window, cx: &mut App) -> bool {
        self.update(cx, |e, cx| e.commit(window, cx))
    }

    fn focus(&self, window: &mut Window, cx: &mut App) {
        self.update(cx, |e, cx| e.focus(window, cx));
    }

    fn replace(&self, content: ArtifactContent, cx: &mut App) {
        self.update(cx, |e, cx| e.replace(content, cx));
    }

    fn set_covered(&self, covered: bool, cx: &mut App) {
        self.update(cx, |e, cx| e.set_covered(covered, cx));
    }

    #[cfg(test)]
    fn covered(&self, cx: &App) -> bool {
        self.read(cx).covered()
    }
}

/// The editor for `content`'s kind. A new kind adds its arm here.
fn editor_for(content: ArtifactContent, window: &mut Window, cx: &mut Context<ArtifactFrame>) -> (Box<dyn AnyEditor>, Subscription) {
    fn attach<E: ArtifactEditor>(editor: Entity<E>, cx: &mut Context<ArtifactFrame>) -> (Box<dyn AnyEditor>, Subscription) {
        let sub = cx.subscribe(&editor, |frame, _, event: &EditorEvent, cx| match event {
            EditorEvent::Changed => frame.changed(cx),
        });
        (Box::new(editor), sub)
    }
    match content {
        ArtifactContent::Table(table) => attach(cx.new(|cx| TableEditor::new(table, window, cx)), cx),
        ArtifactContent::Document(document) => attach(cx.new(|cx| DocumentEditor::new(document, window, cx)), cx),
        ArtifactContent::Checklist(list) => attach(cx.new(|cx| ChecklistEditor::new(list, window, cx)), cx),
        ArtifactContent::Board(board) => attach(cx.new(|cx| BoardEditor::new(board, window, cx)), cx),
        ArtifactContent::Whiteboard(board) => attach(cx.new(|cx| WhiteboardEditor::new(board, window, cx)), cx),
        ArtifactContent::Page(page) => attach(cx.new(|cx| PageView::new(page, window, cx)), cx),
    }
}

/// The emoji shown for each kind, in tabs and the title bar.
pub fn icon(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::Table => "▦",
        ArtifactKind::Document => "¶",
        ArtifactKind::Checklist => "☑",
        ArtifactKind::Board => "▥",
        ArtifactKind::Whiteboard => "◩",
        ArtifactKind::Page => "⧉",
    }
}

pub enum FrameEvent {
    /// Save this content (the frame has waited for edits to settle).
    Save(ArtifactContent),
    /// The user renamed it to this (not yet cleaned up).
    Rename(String),
    /// The user asked to delete it; the app confirms first.
    Delete,
    Close,
}

pub struct ArtifactFrame {
    pub id: usize,
    kind: ArtifactKind,
    title: String,
    editor: Box<dyn AnyEditor>,
    /// The title field, while renaming.
    renaming: Option<Entity<Composer>>,
    /// Changed since last saved.
    dirty: bool,
    save_task: Option<Task<()>>,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<FrameEvent> for ArtifactFrame {}

impl ArtifactFrame {
    pub fn new(id: usize, title: String, content: ArtifactContent, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let kind = content.kind();
        let (editor, sub) = editor_for(content, window, cx);
        Self {
            id,
            kind,
            title,
            editor,
            renaming: None,
            dirty: false,
            save_task: None,
            focus: cx.focus_handle(),
            _subscriptions: vec![sub],
        }
    }

    pub fn set_title(&mut self, title: String, cx: &mut Context<Self>) {
        self.title = title;
        cx.notify();
    }

    #[cfg(test)]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// What the editor shows, an edit in progress included.
    #[cfg(test)]
    pub fn content(&self, cx: &App) -> ArtifactContent {
        self.editor.content(cx)
    }

    /// Whether one of the app's overlays is drawn over the editor.
    pub fn set_covered(&self, covered: bool, cx: &mut App) {
        self.editor.set_covered(covered, cx);
    }

    /// Whether the editor was last told it is covered.
    #[cfg(test)]
    pub fn covered(&self, cx: &App) -> bool {
        self.editor.covered(cx)
    }

    pub fn focus_editor(&self, window: &mut Window, cx: &mut App) {
        self.editor.focus(window, cx);
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        self.dirty = true;
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_AFTER).await;
            this.update(cx, |this, cx| {
                if let Some(content) = this.take_unsaved(cx) {
                    cx.emit(FrameEvent::Save(content));
                }
            })
            .ok();
        }));
    }

    /// The content, if it changed since it was last handed out to save.
    fn take_unsaved(&mut self, cx: &App) -> Option<ArtifactContent> {
        self.save_task = None;
        std::mem::take(&mut self.dirty).then(|| self.editor.content(cx))
    }

    /// Finish the edit in progress and return what hasn't been saved yet,
    /// for the app to save now (it is closing the frame, or quitting).
    pub fn flush(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<ArtifactContent> {
        if self.editor.commit(window, cx) {
            self.dirty = true;
        }
        self.take_unsaved(cx)
    }

    /// What isn't saved yet, including an edit still in progress, without
    /// a window to finish that edit with (when the app quits). The edit
    /// stays open; committing it later saves again.
    pub fn unsaved(&mut self, cx: &mut Context<Self>) -> Option<ArtifactContent> {
        if self.editor.has_pending_edit(cx) {
            self.dirty = true;
        }
        self.take_unsaved(cx)
    }

    /// Show `content`, now saved, in place of what the editor has: an
    /// agent changed the artifact. Edits not saved yet are dropped (see
    /// `ArtifactEditor::replace`), and so is a save waiting to happen, so
    /// neither brings the old content back.
    /// (The app saves what the frame had before letting the agent at it.)
    pub fn replace(&mut self, content: ArtifactContent, cx: &mut Context<Self>) {
        self.save_task = None;
        self.dirty = false;
        self.editor.replace(content, cx);
        cx.notify();
    }

    /// The title, now saved, set by someone else (an agent). A rename in
    /// progress gives way, so it can't put its old text back on blur.
    pub fn replace_title(&mut self, title: String, cx: &mut Context<Self>) {
        if self.renaming.take().is_some() {
            self._subscriptions.truncate(1);
        }
        self.set_title(title, cx);
    }

    // --- renaming ----------------------------------------------------------

    pub fn start_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.renaming.is_some() {
            return;
        }
        let field = cx.new(Composer::compact);
        field.update(cx, |f, cx| {
            f.set_text(self.title.clone(), cx);
            f.set_placeholder(format!("{} name", self.kind.label()), cx);
            f.select_all_text(cx);
        });
        let submit = cx.subscribe_in(&field, window, |this, _, event: &ComposerEvent, window, cx| {
            if let ComposerEvent::Submit(_) = event {
                this.end_rename(true, window, cx);
            }
        });
        // Clicking away keeps the new name.
        let blur = cx.on_blur(&field.read(cx).focus_handle(cx), window, |this, window, cx| this.end_rename(true, window, cx));
        window.focus(&field.read(cx).focus_handle(cx), cx);
        self.renaming = Some(field);
        self._subscriptions.truncate(1);
        self._subscriptions.extend([submit, blur]);
        cx.notify();
    }

    fn end_rename(&mut self, keep: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(field) = self.renaming.take() else { return };
        self._subscriptions.truncate(1);
        if keep {
            let text = field.read(cx).text().to_string();
            cx.emit(FrameEvent::Rename(text));
        }
        self.editor.focus(window, cx);
        cx.notify();
    }

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let icon_button = |id: &'static str, label: &'static str| {
            div()
                .id(id)
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
                .child(label)
        };
        let title = match &self.renaming {
            Some(field) => div()
                .w(px(320.))
                .on_key_down(cx.listener(|this, e: &KeyDownEvent, window, cx| {
                    if e.keystroke.key == "escape" {
                        cx.stop_propagation();
                        this.end_rename(false, window, cx);
                    }
                }))
                .child(field.clone())
                .into_any_element(),
            None => div()
                .id("artifact-title")
                .min_w_0()
                .px_1()
                .rounded_md()
                .cursor_pointer()
                .text_size(px(16.))
                .font_weight(FontWeight::BOLD)
                .whitespace_nowrap()
                .overflow_hidden()
                .text_ellipsis()
                .hover(|s| s.bg(theme::reaction_bg()))
                .tooltip(|_, cx| crate::chat::tooltip("Rename", cx))
                .child(self.title.clone())
                .on_click(cx.listener(|this, _, window, cx| this.start_rename(window, cx)))
                .into_any_element(),
        };
        div()
            .h(px(44.))
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
                    .child(div().flex_none().text_size(px(16.)).text_color(theme::muted()).child(icon(self.kind)))
                    .child(title)
                    .child(
                        div()
                            .flex_none()
                            .px(px(6.))
                            .rounded_sm()
                            .bg(theme::reaction_bg())
                            .text_size(px(11.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::muted())
                            .child(self.kind.label()),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(icon_button("artifact-rename", "Rename").on_click(cx.listener(|this, _, window, cx| this.start_rename(window, cx))))
                    .child(icon_button("artifact-delete", "Delete").on_click(cx.listener(|_, _, _, cx| cx.emit(FrameEvent::Delete))))
                    .child(
                        icon_button("artifact-close", "✕")
                            .tooltip(|_, cx| crate::chat::tooltip("Back to messages", cx))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(FrameEvent::Close))),
                    ),
            )
    }
}

impl Focusable for ArtifactFrame {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ArtifactFrame {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus)
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(self.render_title_bar(cx))
            .child(div().flex_1().min_h_0().child(self.editor.view()))
    }
}
