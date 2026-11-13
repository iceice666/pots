//! The document artifact's editor: a text area for its markdown, and a
//! preview of how it reads.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{App, Context, Entity, EventEmitter, FocusHandle, Focusable, Subscription, Window, div, prelude::*, px, relative};
use gpui_component::input::{InputEvent, Textarea, TextareaState};

use pots_agent::data::{ArtifactContent, Document};
use pots_agent::markdown;

use crate::artifact::{ArtifactEditor, EditorEvent};
use crate::render_md;
use crate::selection::{Selection, SharedSelection};
use crate::theme;

pub struct DocumentEditor {
    input: Entity<TextareaState>,
    /// Text written by someone else (an agent), waiting to be put in the
    /// text area, which needs a window for it: done on the next draw.
    incoming: Option<String>,
    /// Showing how the markdown reads, not the text area.
    preview: bool,
    selection: SharedSelection,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<EditorEvent> for DocumentEditor {}

impl DocumentEditor {
    pub fn new(document: Document, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| TextareaState::new(window, cx).soft_wrap(true).placeholder("Write in markdown…"));
        input.update(cx, |input, cx| input.set_value(document.text, window, cx));
        let subscriptions = vec![cx.subscribe(&input, |_, _, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                cx.emit(EditorEvent::Changed);
            }
        })];
        Self {
            input,
            incoming: None,
            preview: false,
            selection: Rc::new(RefCell::new(Selection::default())),
            focus: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    fn text(&self, cx: &App) -> String {
        match &self.incoming {
            Some(text) => text.clone(),
            None => self.input.read(cx).value().to_string(),
        }
    }

    fn set_preview(&mut self, preview: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.preview = preview;
        ArtifactEditor::focus(self, window, cx);
        cx.notify();
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let text = self.text(cx);
        let words = text.split_whitespace().count();
        let tab = |id: &'static str, label: &'static str, on: bool| {
            div()
                .id(id)
                .flex_none()
                .h(px(26.))
                .px_2()
                .flex()
                .items_center()
                .rounded_md()
                .cursor_pointer()
                .text_size(px(13.))
                .map(|d| match on {
                    true => d.bg(theme::reaction_bg()).text_color(theme::text()),
                    false => d.text_color(theme::muted()).hover(|s| s.bg(theme::reaction_bg())),
                })
                .child(label)
        };
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap_1()
            .px_4()
            .py_1()
            .border_b_1()
            .border_color(theme::border())
            .child(tab("doc-edit", "Edit", !self.preview).on_click(cx.listener(|this, _, window, cx| this.set_preview(false, window, cx))))
            .child(tab("doc-preview", "Preview", self.preview).on_click(cx.listener(|this, _, window, cx| this.set_preview(true, window, cx))))
            .child(
                div()
                    .flex_1()
                    .text_align(gpui::TextAlign::Right)
                    .text_size(px(12.))
                    .text_color(theme::faint())
                    .child(if words == 1 { "1 word".to_string() } else { format!("{words} words") }),
            )
    }
}

impl ArtifactEditor for DocumentEditor {
    fn content(&self, cx: &App) -> ArtifactContent {
        ArtifactContent::Document(Document { text: self.text(cx) })
    }

    /// Every keystroke is a change already: nothing waits to be committed.
    fn has_pending_edit(&self, _: &App) -> bool {
        false
    }

    fn commit(&mut self, _: &mut Window, _: &mut Context<Self>) -> bool {
        false
    }

    fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.preview {
            true => window.focus(&self.focus, cx),
            false => self.input.update(cx, |input, cx| input.focus(window, cx)),
        }
    }

    /// The agent's text wins; the caret goes back to the start.
    fn replace(&mut self, content: ArtifactContent, cx: &mut Context<Self>) {
        let ArtifactContent::Document(document) = content else { return };
        self.incoming = Some(document.text);
        cx.notify();
    }
}

impl Focusable for DocumentEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for DocumentEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(text) = self.incoming.take() {
            self.input.update(cx, |input, cx| input.set_value(text, window, cx));
        }
        let body = match self.preview {
            true => {
                let text = self.text(cx);
                let blocks = markdown::parse(&text);
                div()
                    .id("doc-preview-body")
                    .size_full()
                    .overflow_y_scroll()
                    .px_6()
                    .py_4()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .when(blocks.is_empty(), |d| d.text_color(theme::faint()).child("Nothing written yet."))
                    .children(render_md::blocks(&blocks, &mut render_md::Ctx::new(usize::MAX / 2, self.selection.clone())))
                    .into_any_element()
            }
            false => div()
                .size_full()
                .px_4()
                .py_2()
                .child(Textarea::new(&self.input).appearance(false).h(relative(1.)))
                .into_any_element(),
        };
        div()
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_toolbar(cx))
            .child(div().flex_1().min_h_0().child(body))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::{TestAppContext, VisualTestContext};

    use super::*;

    fn open<'a>(cx: &'a mut TestAppContext, text: &str) -> (Entity<DocumentEditor>, Rc<RefCell<usize>>, &'a mut VisualTestContext) {
        cx.update(gpui_component::init);
        let mut editor = None;
        let document = Document { text: text.to_string() };
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| DocumentEditor::new(document, window, cx));
            editor = Some(view.clone());
            gpui_component::Root::new(view, window, cx)
        });
        let editor = editor.expect("the window was built");
        let changes = Rc::new(RefCell::new(0));
        let counter = changes.clone();
        cx.update(|window, cx| {
            cx.subscribe(&editor, move |_, _: &EditorEvent, _| *counter.borrow_mut() += 1).detach();
            editor.update(cx, |e, cx| ArtifactEditor::focus(e, window, cx));
        });
        cx.run_until_parked();
        (editor, changes, cx)
    }

    fn text(editor: &Entity<DocumentEditor>, cx: &mut VisualTestContext) -> String {
        match editor.read_with(cx, |e, cx| e.content(cx)) {
            ArtifactContent::Document(d) => d.text,
            other => panic!("not a document: {other:?}"),
        }
    }

    #[gpui::test]
    fn typing_changes_the_text(cx: &mut TestAppContext) {
        let (editor, changes, cx) = open(cx, "# Notes");
        assert_eq!(text(&editor, cx), "# Notes");
        assert_eq!(*changes.borrow(), 0, "opening is no change");
        cx.simulate_input("!");
        assert!(text(&editor, cx).contains('!'), "{}", text(&editor, cx));
        assert!(*changes.borrow() >= 1);
    }

    #[gpui::test]
    fn an_agents_text_replaces_it_and_previews(cx: &mut TestAppContext) {
        let (editor, changes, cx) = open(cx, "old");
        editor.update(cx, |e, cx| e.replace(ArtifactContent::Document(Document { text: "**new**".into() }), cx));
        assert_eq!(text(&editor, cx), "**new**", "even before it is drawn");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        assert_eq!(text(&editor, cx), "**new**");
        assert_eq!(editor.read_with(cx, |e, cx| e.input.read(cx).value().to_string()), "**new**");
        assert_eq!(*changes.borrow(), 0, "someone else's text is no change of ours");

        cx.update(|window, cx| editor.update(cx, |e, cx| e.set_preview(true, window, cx)));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(text(&editor, cx), "**new**");
    }
}
