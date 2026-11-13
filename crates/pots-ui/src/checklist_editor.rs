//! The checklist artifact's editor: items with a box to tick each off.
//!
//! Clicking an item's text edits it in place. While editing, Enter keeps
//! the text and starts a new item below, Escape drops the edit, and an item
//! left blank is removed. Keys an input method is composing with never
//! commit: see `composing`.

use gpui::{
    App, Context, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, KeyBinding, Subscription,
    Window, actions, div, prelude::*, px,
};
use gpui_component::Sizable;
use gpui_component::input::{Input, InputEvent, InputState};

use pots_agent::data::{ArtifactContent, Checklist};

use crate::artifact::{ArtifactEditor, EditorEvent};
use crate::theme;

actions!(checklist, [CommitItem, CancelItem]);

/// Around the input of the item being edited.
const EDIT_CONTEXT: &str = "ChecklistEdit";

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys(bindings());
}

/// Must be bound after `gpui_component::init`: at the same depth as the
/// input's own bindings, the later binding wins.
pub(crate) fn bindings() -> Vec<KeyBinding> {
    let edit = Some("ChecklistEdit > Input");
    vec![KeyBinding::new("enter", CommitItem, edit), KeyBinding::new("escape", CancelItem, edit)]
}

pub struct ChecklistEditor {
    list: Checklist,
    /// The item being edited.
    editing: Option<usize>,
    input: Entity<InputState>,
    focus: FocusHandle,
    /// The list was replaced while an item was being edited: once drawn
    /// without the item's input, the keyboard goes back to the list.
    refocus: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<EditorEvent> for ChecklistEditor {}

/// Where item `ix` is after item `removed` (if any) was taken out; `None`
/// if it was that one.
fn shifted(ix: usize, removed: Option<usize>) -> Option<usize> {
    match removed {
        Some(r) if r == ix => None,
        Some(r) if r < ix => Some(ix - 1),
        _ => Some(ix),
    }
}

impl ChecklistEditor {
    pub fn new(list: Checklist, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Something to do"));
        let subscriptions = vec![
            // Clicking elsewhere keeps what was typed.
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::Blur = event {
                    this.commit_edit(window, cx);
                }
            }),
        ];
        Self { list, editing: None, input, focus: cx.focus_handle(), refocus: false, _subscriptions: subscriptions }
    }

    /// The list with the edit in progress, if any, applied as committing
    /// it would.
    fn list_with_edit(&self, cx: &App) -> Checklist {
        let mut list = self.list.clone();
        if let Some(ix) = self.editing {
            let text = self.input.read(cx).value().to_string();
            match text.trim().is_empty() {
                true => list.remove(ix),
                false => list.set_text(ix, &text),
            };
        }
        list
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    fn start_edit(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = shifted(ix, self.commit_edit(window, cx).and_then(|(_, removed)| removed)) else { return };
        let Some(item) = self.list.items.get(ix) else { return };
        let text = item.text.clone();
        self.input.update(cx, |input, cx| {
            input.set_value(text, window, cx);
            input.focus(window, cx);
        });
        self.editing = Some(ix);
        cx.notify();
    }

    /// Keep what's typed in the item being edited, if any; a blank item is
    /// removed. Returns whether that changed the list, and the index of the
    /// item removed.
    fn commit_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<(bool, Option<usize>)> {
        let ix = self.editing.take()?;
        let text = self.input.read(cx).value().to_string();
        let (changed, removed) = match text.trim().is_empty() {
            true => (self.list.remove(ix), Some(ix)),
            false => (self.list.set_text(ix, &text), None),
        };
        self.input.update(cx, |input, cx| input.set_value("", window, cx));
        if changed {
            self.changed(cx);
        }
        cx.notify();
        Some((changed, removed))
    }

    /// Whether an input method is composing in the item being edited: the
    /// keys it uses to pick and confirm characters must not commit.
    fn composing(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.editing.is_some() && self.input.update(cx, |input, cx| input.marked_text_range(window, cx).is_some())
    }

    /// Enter: keep the text and go on with a new item below it.
    fn on_commit(&mut self, _: &CommitItem, window: &mut Window, cx: &mut Context<Self>) {
        if self.composing(window, cx) {
            return;
        }
        let Some(ix) = self.editing else { return };
        match self.commit_edit(window, cx) {
            Some((_, None)) => self.add_item(ix + 1, window, cx),
            _ => window.focus(&self.focus, cx),
        }
    }

    /// Escape: drop the edit; an item never written in goes too.
    fn on_cancel(&mut self, _: &CancelItem, window: &mut Window, cx: &mut Context<Self>) {
        if self.composing(window, cx) {
            return cx.propagate();
        }
        let Some(ix) = self.editing.take() else { return };
        self.input.update(cx, |input, cx| input.set_value("", window, cx));
        if self.list.items.get(ix).is_some_and(|i| i.text.trim().is_empty()) {
            self.list.remove(ix);
            self.changed(cx);
        }
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// A new, empty item before item `at`, being edited.
    fn add_item(&mut self, at: usize, window: &mut Window, cx: &mut Context<Self>) {
        let removed = self.commit_edit(window, cx).and_then(|(_, removed)| removed);
        let at = match removed {
            Some(r) if r < at => at - 1,
            _ => at,
        };
        let at = self.list.insert(at);
        self.changed(cx);
        self.start_edit(at, window, cx);
    }

    fn add_at_end(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.add_item(usize::MAX, window, cx);
    }

    fn toggle(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.list.toggle(ix) {
            self.changed(cx);
        }
    }

    fn remove(&mut self, ix: usize, cx: &mut Context<Self>) {
        match self.editing {
            Some(e) if e == ix => self.editing = None,
            Some(e) if e > ix => self.editing = Some(e - 1),
            _ => {}
        }
        if self.list.remove(ix) {
            self.changed(cx);
        }
    }

    fn remove_done(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_edit(window, cx);
        let before = self.list.items.len();
        self.list.items.retain(|i| !i.done);
        if self.list.items.len() != before {
            self.changed(cx);
        }
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (done, total) = (self.list.done(), self.list.items.len());
        let button = |id: &'static str, label: &'static str, enabled: bool| {
            div()
                .id(id)
                .flex_none()
                .h(px(26.))
                .px_2()
                .flex()
                .items_center()
                .rounded_md()
                .text_size(px(13.))
                .map(|d| match enabled {
                    true => d.cursor_pointer().text_color(theme::text()).hover(|s| s.bg(theme::reaction_bg())),
                    false => d.text_color(theme::faint()),
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
            .child(button("check-add", "+ Item", true).on_click(cx.listener(|this, _, window, cx| this.add_at_end(window, cx))))
            .child(
                button("check-clear", "Remove done", done > 0)
                    .on_click(cx.listener(|this, _, window, cx| this.remove_done(window, cx))),
            )
            .child(div().flex_none().w(px(1.)).h(px(16.)).mx_1().bg(theme::border()))
            .child(div().text_size(px(13.)).text_color(theme::muted()).child(format!("{done} of {total} done")))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_align(gpui::TextAlign::Right)
                    .text_size(px(12.))
                    .text_color(theme::faint())
                    .child("Click an item to edit · Enter for the next · Esc to undo the edit"),
            )
    }

    fn render_item(&self, ix: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let item = &self.list.items[ix];
        let done = item.done;
        let check = div()
            .id(("check-box", ix))
            .flex_none()
            .size(px(18.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_sm()
            .border_1()
            .cursor_pointer()
            .text_size(px(12.))
            .map(|d| match done {
                true => d.bg(theme::checkbox_on()).border_color(theme::checkbox_on()).text_color(gpui::white()).child("✓"),
                false => d.border_color(theme::faint()).hover(|s| s.border_color(theme::muted())),
            })
            .on_click(cx.listener(move |this, _, _, cx| this.toggle(ix, cx)));
        let text = match self.editing == Some(ix) {
            true => div()
                .key_context(EDIT_CONTEXT)
                .flex_1()
                .min_w_0()
                .child(Input::new(&self.input).appearance(false).small())
                .into_any_element(),
            false => div()
                .id(("check-text", ix))
                .flex_1()
                .min_w_0()
                .cursor_text()
                .text_size(px(15.))
                .map(|d| match (done, item.text.is_empty()) {
                    (_, true) => d.text_color(theme::faint()).child("Empty item"),
                    (true, false) => d.text_color(theme::faint()).line_through().child(item.text.clone()),
                    (false, false) => d.text_color(theme::text()).child(item.text.clone()),
                })
                .on_click(cx.listener(move |this, _, window, cx| this.start_edit(ix, window, cx)))
                .into_any_element(),
        };
        let delete = div()
            .id(("check-delete", ix))
            .flex_none()
            .px_1()
            .rounded_sm()
            .cursor_pointer()
            .text_size(px(12.))
            .text_color(theme::faint())
            .opacity(0.)
            .group_hover("check-item", |s| s.opacity(1.))
            .hover(|s| s.bg(theme::reaction_bg()).text_color(theme::text()))
            .child("✕")
            .on_click(cx.listener(move |this, _, _, cx| this.remove(ix, cx)));
        div()
            .id(("check-item", ix))
            .group("check-item")
            .min_h(px(34.))
            .px_2()
            .flex()
            .items_center()
            .gap_3()
            .rounded_md()
            .hover(|s| s.bg(theme::hover()))
            .child(check)
            .child(text)
            .child(delete)
    }
}

impl ArtifactEditor for ChecklistEditor {
    fn content(&self, cx: &App) -> ArtifactContent {
        ArtifactContent::Checklist(self.list_with_edit(cx))
    }

    fn has_pending_edit(&self, cx: &App) -> bool {
        self.editing.is_some() && self.list_with_edit(cx) != self.list
    }

    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.commit_edit(window, cx).is_some_and(|(changed, _)| changed)
    }

    fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.editing {
            Some(_) => self.input.update(cx, |input, cx| input.focus(window, cx)),
            None => window.focus(&self.focus, cx),
        }
    }

    /// The item being edited stays open if the new list still has it with
    /// the text typed so far, so typing goes on; otherwise the agent's
    /// list wins.
    fn replace(&mut self, content: ArtifactContent, cx: &mut Context<Self>) {
        let ArtifactContent::Checklist(list) = content else { return };
        let text = self.input.read(cx).value().to_string();
        let keep = self.editing.is_some_and(|ix| list.items.get(ix).is_some_and(|i| i.text == text));
        if !keep && self.editing.take().is_some() {
            self.refocus = true;
        }
        self.list = list;
        cx.notify();
    }
}

impl Focusable for ChecklistEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ChecklistEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.refocus) && self.input.focus_handle(cx).is_focused(window) {
            let focus = self.focus.clone();
            window.defer(cx, move |window, cx| window.focus(&focus, cx));
        }
        let items: Vec<_> = (0..self.list.items.len()).map(|ix| self.render_item(ix, cx).into_any_element()).collect();
        div()
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .on_action(cx.listener(Self::on_commit))
            .on_action(cx.listener(Self::on_cancel))
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .id("checklist-items")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_4()
                    .py_3()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .children(items)
                    .child(
                        div()
                            .id("check-add-row")
                            .h(px(34.))
                            .px_2()
                            .flex()
                            .items_center()
                            .rounded_md()
                            .cursor_pointer()
                            .text_size(px(14.))
                            .text_color(theme::muted())
                            .hover(|s| s.bg(theme::hover()).text_color(theme::text()))
                            .child("+ Add an item")
                            .on_click(cx.listener(|this, _, window, cx| this.add_at_end(window, cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::{EntityInputHandler, TestAppContext, VisualTestContext};
    use pots_agent::data::ChecklistItem;

    use super::*;

    fn open(cx: &mut TestAppContext, list: Checklist) -> (Entity<ChecklistEditor>, Rc<RefCell<usize>>, &mut VisualTestContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            bind_keys(cx);
        });
        let mut editor = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| ChecklistEditor::new(list, window, cx));
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

    fn texts(editor: &Entity<ChecklistEditor>, cx: &mut VisualTestContext) -> Vec<(String, bool)> {
        editor.read_with(cx, |e, _| e.list.items.iter().map(|i| (i.text.clone(), i.done)).collect())
    }

    fn list(items: &[&str]) -> Checklist {
        Checklist { items: items.iter().map(|t| ChecklistItem::new(t)).collect() }
    }

    #[gpui::test]
    fn enter_keeps_the_item_and_starts_the_next(cx: &mut TestAppContext) {
        let (editor, changes, cx) = open(cx, Checklist::default());
        cx.update(|window, cx| editor.update(cx, |e, cx| e.add_at_end(window, cx)));
        assert_eq!(editor.read_with(cx, |e, _| e.editing), Some(0));
        cx.simulate_input("milk");
        assert_eq!(texts(&editor, cx), [("".into(), false)], "nothing kept while typing");
        assert!(editor.read_with(cx, |e, cx| e.has_pending_edit(cx)));
        cx.simulate_keystrokes("enter");
        assert_eq!(editor.read_with(cx, |e, _| e.editing), Some(1), "on to a new item");
        cx.simulate_input("eggs");
        cx.simulate_keystrokes("enter");
        // Enter on the blank item at the end: it goes, and editing stops.
        cx.simulate_keystrokes("enter");
        assert_eq!(editor.read_with(cx, |e, _| e.editing), None);
        assert_eq!(texts(&editor, cx), [("milk".into(), false), ("eggs".into(), false)]);
        assert!(*changes.borrow() >= 3);
    }

    #[gpui::test]
    fn escape_drops_the_edit_and_a_new_empty_item(cx: &mut TestAppContext) {
        let (editor, _, cx) = open(cx, list(&["milk"]));
        cx.update(|window, cx| editor.update(cx, |e, cx| e.start_edit(0, window, cx)));
        cx.simulate_input("oops");
        cx.simulate_keystrokes("escape");
        assert_eq!(texts(&editor, cx), [("milk".into(), false)]);
        cx.update(|window, cx| editor.update(cx, |e, cx| e.add_at_end(window, cx)));
        cx.simulate_keystrokes("escape");
        assert_eq!(texts(&editor, cx), [("milk".into(), false)], "the empty item goes");
    }

    #[gpui::test]
    fn items_are_ticked_and_removed(cx: &mut TestAppContext) {
        let (editor, changes, cx) = open(cx, list(&["a", "b", "c"]));
        editor.update(cx, |e, cx| {
            e.toggle(0, cx);
            e.toggle(2, cx);
        });
        assert_eq!(editor.read_with(cx, |e, _| e.list.done()), 2);
        editor.update(cx, |e, cx| e.remove(1, cx));
        assert_eq!(texts(&editor, cx), [("a".into(), true), ("c".into(), true)]);
        cx.update(|window, cx| editor.update(cx, |e, cx| e.remove_done(window, cx)));
        assert!(texts(&editor, cx).is_empty());
        assert_eq!(*changes.borrow(), 4);
    }

    #[gpui::test]
    fn keys_an_input_method_is_using_never_commit(cx: &mut TestAppContext) {
        let (editor, _, cx) = open(cx, list(&[""]));
        cx.update(|window, cx| editor.update(cx, |e, cx| e.start_edit(0, window, cx)));
        let input = editor.read_with(cx, |e, _| e.input.clone());
        cx.update(|window, cx| input.update(cx, |i, cx| i.replace_and_mark_text_in_range(None, "ㄋㄧˇ", None, window, cx)));
        cx.simulate_keystrokes("enter");
        assert_eq!(editor.read_with(cx, |e, _| e.editing), Some(0), "enter while composing");
        cx.update(|window, cx| input.update(cx, |i, cx| i.replace_text_in_range(None, "你", window, cx)));
        cx.simulate_keystrokes("enter");
        assert_eq!(texts(&editor, cx)[0], ("你".into(), false));
    }

    #[gpui::test]
    fn an_agents_list_keeps_the_edit_only_if_it_still_has_it(cx: &mut TestAppContext) {
        let (editor, _, cx) = open(cx, list(&["a"]));
        cx.update(|window, cx| editor.update(cx, |e, cx| e.start_edit(0, window, cx)));
        cx.simulate_input("!");
        // The app saved the edit first; the agent added an item.
        let saved = editor.read_with(cx, |e, cx| e.content(cx));
        let ArtifactContent::Checklist(mut new) = saved else { panic!() };
        assert_eq!(new.items[0].text, "a!");
        new.items.push(ChecklistItem::new("b"));
        editor.update(cx, |e, cx| e.replace(ArtifactContent::Checklist(new), cx));
        assert_eq!(editor.read_with(cx, |e, _| e.editing), Some(0), "typing goes on");
        // Now it rewrites that item: the edit gives way.
        editor.update(cx, |e, cx| e.replace(ArtifactContent::Checklist(list(&["z"])), cx));
        assert_eq!(editor.read_with(cx, |e, _| e.editing), None);
        assert_eq!(texts(&editor, cx), [("z".into(), false)]);
    }
}
