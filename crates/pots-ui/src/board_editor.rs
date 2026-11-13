//! The board artifact's editor: a kanban board, its columns side by side,
//! each with its cards top to bottom.
//!
//! Clicking a card's text or a column's name edits it in place. While
//! editing a card, Enter keeps the text and starts a new card below it,
//! Escape drops the edit, and a card left blank is removed. Cards move to
//! the next column and back with their ◀ ▶ buttons. Keys an input method
//! is composing with never commit: see `composing`.

use gpui::{
    App, Context, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, FontWeight, KeyBinding,
    Subscription, Window, actions, div, prelude::*, px,
};
use gpui_component::Sizable;
use gpui_component::input::{Input, InputEvent, InputState};

use pots_agent::data::{ArtifactContent, Board};

use crate::artifact::{ArtifactEditor, EditorEvent};
use crate::theme;

actions!(board, [CommitBoardEdit, CancelBoardEdit]);

/// Around the input of the card or column name being edited.
const EDIT_CONTEXT: &str = "BoardEdit";
const COLUMN_WIDTH: f32 = 260.;

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys(bindings());
}

/// Must be bound after `gpui_component::init`: at the same depth as the
/// input's own bindings, the later binding wins.
pub(crate) fn bindings() -> Vec<KeyBinding> {
    let edit = Some("BoardEdit > Input");
    vec![KeyBinding::new("enter", CommitBoardEdit, edit), KeyBinding::new("escape", CancelBoardEdit, edit)]
}

/// What's being edited in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Editing {
    /// Card `.1` of column `.0`.
    Card(usize, usize),
    Column(usize),
}

/// Where card `ix` of column `col` is after the card `removed` (if any) was
/// taken out; `None` if it was that one.
fn shifted((col, ix): (usize, usize), removed: Option<(usize, usize)>) -> Option<(usize, usize)> {
    match removed {
        Some(r) if r == (col, ix) => None,
        Some((c, r)) if c == col && r < ix => Some((col, ix - 1)),
        _ => Some((col, ix)),
    }
}

pub struct BoardEditor {
    board: Board,
    editing: Option<Editing>,
    input: Entity<InputState>,
    focus: FocusHandle,
    /// The board was replaced while something was being edited: once drawn
    /// without its input, the keyboard goes back to the board.
    refocus: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<EditorEvent> for BoardEditor {}

impl BoardEditor {
    pub fn new(board: Board, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx));
        let subscriptions = vec![
            // Clicking elsewhere keeps what was typed.
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::Blur = event {
                    this.commit_edit(window, cx);
                }
            }),
        ];
        Self { board, editing: None, input, focus: cx.focus_handle(), refocus: false, _subscriptions: subscriptions }
    }

    /// Apply `text`, typed while editing `editing`, to `board`, as
    /// committing does: a blank card is removed, a blank column name
    /// refused. Returns whether it changed, and the card removed.
    fn apply(board: &mut Board, editing: Editing, text: &str) -> (bool, Option<(usize, usize)>) {
        match editing {
            Editing::Card(col, ix) if text.trim().is_empty() => (board.remove_card(col, ix), Some((col, ix))),
            Editing::Card(col, ix) => (board.set_card(col, ix, text), None),
            Editing::Column(col) => (board.rename_column(col, text), None),
        }
    }

    fn board_with_edit(&self, cx: &App) -> Board {
        let mut board = self.board.clone();
        if let Some(editing) = self.editing {
            Self::apply(&mut board, editing, &self.input.read(cx).value());
        }
        board
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Keep what's typed, if anything is being edited. Returns whether
    /// that changed the board, and the card it removed (left blank).
    fn commit_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<(bool, Option<(usize, usize)>)> {
        let editing = self.editing.take()?;
        let text = self.input.read(cx).value().to_string();
        let (changed, removed) = Self::apply(&mut self.board, editing, &text);
        self.input.update(cx, |input, cx| input.set_value("", window, cx));
        if changed {
            self.changed(cx);
        }
        cx.notify();
        Some((changed, removed))
    }

    fn removed_by_commit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<(usize, usize)> {
        self.commit_edit(window, cx).and_then(|(_, removed)| removed)
    }

    fn start(&mut self, editing: Editing, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| {
            input.set_value(text, window, cx);
            if let Editing::Column(_) = editing {
                input.select_all(window, cx);
            }
            input.focus(window, cx);
        });
        self.editing = Some(editing);
        cx.notify();
    }

    fn start_card_edit(&mut self, col: usize, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let removed = self.removed_by_commit(window, cx);
        let Some((col, ix)) = shifted((col, ix), removed) else { return };
        let Some(text) = self.board.card(col, ix).map(str::to_string) else { return };
        self.start(Editing::Card(col, ix), text, window, cx);
    }

    fn start_column_edit(&mut self, col: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_edit(window, cx);
        let Some(name) = self.board.columns.get(col).map(|c| c.name.clone()) else { return };
        self.start(Editing::Column(col), name, window, cx);
    }

    /// A new, empty card in column `col` before card `at`, being edited.
    fn add_card(&mut self, col: usize, at: usize, window: &mut Window, cx: &mut Context<Self>) {
        let at = match self.removed_by_commit(window, cx) {
            Some((c, r)) if c == col && r < at => at - 1,
            _ => at,
        };
        let Some(at) = self.board.insert_card(col, at) else { return };
        self.changed(cx);
        self.start_card_edit(col, at, window, cx);
    }

    fn remove_card(&mut self, col: usize, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let removed = self.removed_by_commit(window, cx);
        let Some((col, ix)) = shifted((col, ix), removed) else { return };
        if self.board.remove_card(col, ix) {
            self.changed(cx);
        }
    }

    /// Move a card one column left (`-1`) or right (`1`).
    fn move_card(&mut self, col: usize, ix: usize, by: isize, window: &mut Window, cx: &mut Context<Self>) {
        let removed = self.removed_by_commit(window, cx);
        let Some((col, ix)) = shifted((col, ix), removed) else { return };
        let Some(to) = col.checked_add_signed(by) else { return };
        if self.board.move_card(col, ix, to).is_some() {
            self.changed(cx);
        }
    }

    fn add_column(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_edit(window, cx);
        let name = self.board.next_column_name();
        let col = self.board.insert_column(usize::MAX, &name);
        self.changed(cx);
        self.start_column_edit(col, window, cx);
    }

    fn remove_column(&mut self, col: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_edit(window, cx);
        if self.board.remove_column(col) {
            self.changed(cx);
        }
    }

    /// Whether an input method is composing in what's being edited: the
    /// keys it uses to pick and confirm characters must not commit.
    fn composing(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.editing.is_some() && self.input.update(cx, |input, cx| input.marked_text_range(window, cx).is_some())
    }

    /// Enter: keep the text; on a card, go on with a new card below it.
    fn on_commit(&mut self, _: &CommitBoardEdit, window: &mut Window, cx: &mut Context<Self>) {
        if self.composing(window, cx) {
            return;
        }
        let editing = self.editing;
        match (editing, self.commit_edit(window, cx)) {
            (Some(Editing::Card(col, ix)), Some((_, None))) => self.add_card(col, ix + 1, window, cx),
            _ => window.focus(&self.focus, cx),
        }
    }

    /// Escape: drop the edit; a card never written in goes too.
    fn on_cancel(&mut self, _: &CancelBoardEdit, window: &mut Window, cx: &mut Context<Self>) {
        if self.composing(window, cx) {
            return cx.propagate();
        }
        let Some(editing) = self.editing.take() else { return };
        self.input.update(cx, |input, cx| input.set_value("", window, cx));
        if let Editing::Card(col, ix) = editing
            && self.board.card(col, ix).is_some_and(|c| c.trim().is_empty())
        {
            self.board.remove_card(col, ix);
            self.changed(cx);
        }
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn render_input(&self) -> impl IntoElement {
        div().key_context(EDIT_CONTEXT).w_full().child(Input::new(&self.input).appearance(false).small())
    }

    fn small_button(id: (&'static str, usize), label: &'static str) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .flex_none()
            .px_1()
            .rounded_sm()
            .cursor_pointer()
            .text_size(px(11.))
            .text_color(theme::muted())
            .hover(|s| s.bg(theme::reaction_bg()).text_color(theme::text()))
            .child(label)
    }

    fn render_card(&self, col: usize, ix: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let key = col * 10_000 + ix;
        let columns = self.board.columns.len();
        let text = self.board.columns[col].cards[ix].clone();
        let body = match self.editing == Some(Editing::Card(col, ix)) {
            true => self.render_input().into_any_element(),
            false => div()
                .id(("card-text", key))
                .w_full()
                .cursor_text()
                .text_size(px(14.))
                .map(|d| match text.is_empty() {
                    true => d.text_color(theme::faint()).child("Empty card"),
                    false => d.text_color(theme::text()).child(text),
                })
                .on_click(cx.listener(move |this, _, window, cx| this.start_card_edit(col, ix, window, cx)))
                .into_any_element(),
        };
        let actions = div()
            .flex()
            .items_center()
            .justify_end()
            .gap_1()
            .opacity(0.)
            .group_hover("board-card", |s| s.opacity(1.))
            .when(col > 0, |d| {
                d.child(
                    Self::small_button(("card-left", key), "◀")
                        .on_click(cx.listener(move |this, _, window, cx| this.move_card(col, ix, -1, window, cx))),
                )
            })
            .when(col + 1 < columns, |d| {
                d.child(
                    Self::small_button(("card-right", key), "▶")
                        .on_click(cx.listener(move |this, _, window, cx| this.move_card(col, ix, 1, window, cx))),
                )
            })
            .child(
                Self::small_button(("card-delete", key), "✕")
                    .on_click(cx.listener(move |this, _, window, cx| this.remove_card(col, ix, window, cx))),
            );
        div()
            .id(("board-card", key))
            .group("board-card")
            .w_full()
            .px_3()
            .pt_2()
            .pb_1()
            .flex()
            .flex_col()
            .gap_1()
            .rounded_md()
            .border_1()
            .border_color(theme::border())
            .bg(theme::bg())
            .shadow_sm()
            .child(body)
            .child(actions)
    }

    fn render_column(&self, col: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let column = &self.board.columns[col];
        let can_delete = self.board.columns.len() > 1;
        let header = match self.editing == Some(Editing::Column(col)) {
            true => self.render_input().into_any_element(),
            false => div()
                .id(("column-name", col))
                .flex_1()
                .min_w_0()
                .px_1()
                .rounded_sm()
                .cursor_pointer()
                .whitespace_nowrap()
                .overflow_hidden()
                .text_ellipsis()
                .text_size(px(14.))
                .font_weight(FontWeight::SEMIBOLD)
                .hover(|s| s.bg(theme::reaction_bg()))
                .child(column.name.clone())
                .on_click(cx.listener(move |this, _, window, cx| this.start_column_edit(col, window, cx)))
                .into_any_element(),
        };
        let cards: Vec<_> = (0..column.cards.len()).map(|ix| self.render_card(col, ix, cx).into_any_element()).collect();
        let count = column.cards.len();
        div()
            .id(("board-column", col))
            .group("board-column")
            .flex_none()
            .w(px(COLUMN_WIDTH))
            .max_h_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .rounded_lg()
            .bg(theme::code_block_bg())
            .border_1()
            .border_color(theme::code_block_border())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(28.))
                    .child(header)
                    .child(div().flex_none().text_size(px(12.)).text_color(theme::faint()).child(count.to_string()))
                    .when(can_delete, |d| {
                        d.child(
                            Self::small_button(("column-delete", col), "✕")
                                .opacity(0.)
                                .group_hover("board-column", |s| s.opacity(1.))
                                .on_click(cx.listener(move |this, _, window, cx| this.remove_column(col, window, cx))),
                        )
                    }),
            )
            .child(
                div()
                    .id(("column-cards", col))
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(cards),
            )
            .child(
                div()
                    .id(("card-add", col))
                    .h(px(30.))
                    .px_2()
                    .flex()
                    .items_center()
                    .rounded_md()
                    .cursor_pointer()
                    .text_size(px(13.))
                    .text_color(theme::muted())
                    .hover(|s| s.bg(theme::reaction_bg()).text_color(theme::text()))
                    .child("+ Add a card")
                    .on_click(cx.listener(move |this, _, window, cx| this.add_card(col, usize::MAX, window, cx))),
            )
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let cards = self.board.cards();
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap_1()
            .px_4()
            .py_1()
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .id("board-add-column")
                    .flex_none()
                    .h(px(26.))
                    .px_2()
                    .flex()
                    .items_center()
                    .rounded_md()
                    .cursor_pointer()
                    .text_size(px(13.))
                    .text_color(theme::text())
                    .hover(|s| s.bg(theme::reaction_bg()))
                    .child("+ Column")
                    .on_click(cx.listener(|this, _, window, cx| this.add_column(window, cx))),
            )
            .child(div().flex_none().w(px(1.)).h(px(16.)).mx_1().bg(theme::border()))
            .child(div().text_size(px(13.)).text_color(theme::muted()).child(if cards == 1 {
                "1 card".to_string()
            } else {
                format!("{cards} cards")
            }))
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
                    .child("Click to edit · Enter for the next card · ◀ ▶ to move a card"),
            )
    }
}

impl ArtifactEditor for BoardEditor {
    fn content(&self, cx: &App) -> ArtifactContent {
        ArtifactContent::Board(self.board_with_edit(cx))
    }

    fn has_pending_edit(&self, cx: &App) -> bool {
        self.editing.is_some() && self.board_with_edit(cx) != self.board
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

    /// What's being edited stays open if the new board still has it with
    /// the text typed so far, so typing goes on; otherwise the agent's
    /// board wins.
    fn replace(&mut self, content: ArtifactContent, cx: &mut Context<Self>) {
        let ArtifactContent::Board(board) = content else { return };
        let text = self.input.read(cx).value().to_string();
        let keep = match self.editing {
            Some(Editing::Card(col, ix)) => board.card(col, ix) == Some(text.as_str()),
            Some(Editing::Column(col)) => board.columns.get(col).is_some_and(|c| c.name == text),
            None => false,
        };
        if !keep && self.editing.take().is_some() {
            self.refocus = true;
        }
        self.board = board;
        cx.notify();
    }
}

impl Focusable for BoardEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for BoardEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.refocus) && self.input.focus_handle(cx).is_focused(window) {
            let focus = self.focus.clone();
            window.defer(cx, move |window, cx| window.focus(&focus, cx));
        }
        let columns: Vec<_> = (0..self.board.columns.len()).map(|col| self.render_column(col, cx).into_any_element()).collect();
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
                    .id("board-columns")
                    .flex_1()
                    .min_h_0()
                    .overflow_x_scroll()
                    .p_4()
                    .flex()
                    .items_start()
                    .gap_3()
                    .children(columns),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::{TestAppContext, VisualTestContext};

    use super::*;

    fn open(cx: &mut TestAppContext, board: Board) -> (Entity<BoardEditor>, Rc<RefCell<usize>>, &mut VisualTestContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            bind_keys(cx);
        });
        let mut editor = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| BoardEditor::new(board, window, cx));
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

    fn board(editor: &Entity<BoardEditor>, cx: &mut VisualTestContext) -> Board {
        editor.read_with(cx, |e, _| e.board.clone())
    }

    fn cards(editor: &Entity<BoardEditor>, cx: &mut VisualTestContext) -> Vec<Vec<String>> {
        board(editor, cx).columns.into_iter().map(|c| c.cards).collect()
    }

    #[gpui::test]
    fn cards_are_added_with_enter_and_moved_along(cx: &mut TestAppContext) {
        let (editor, changes, cx) = open(cx, Board::blank());
        cx.update(|window, cx| editor.update(cx, |e, cx| e.add_card(0, usize::MAX, window, cx)));
        cx.simulate_input("Login");
        cx.simulate_keystrokes("enter");
        assert_eq!(editor.read_with(cx, |e, _| e.editing), Some(Editing::Card(0, 1)), "on to a new card");
        cx.simulate_input("Signup");
        cx.simulate_keystrokes("enter");
        cx.simulate_keystrokes("enter");
        assert_eq!(editor.read_with(cx, |e, _| e.editing), None, "a blank card ends it");
        assert_eq!(cards(&editor, cx), [vec!["Login".to_string(), "Signup".into()], vec![], vec![]]);

        cx.update(|window, cx| editor.update(cx, |e, cx| e.move_card(0, 0, 1, window, cx)));
        cx.update(|window, cx| editor.update(cx, |e, cx| e.move_card(1, 0, 1, window, cx)));
        cx.update(|window, cx| editor.update(cx, |e, cx| e.move_card(2, 0, 1, window, cx)));
        assert_eq!(cards(&editor, cx), [vec!["Signup".to_string()], vec![], vec!["Login".into()]], "no column past the last");
        cx.update(|window, cx| editor.update(cx, |e, cx| e.remove_card(0, 0, window, cx)));
        assert_eq!(board(&editor, cx).cards(), 1);
        assert!(*changes.borrow() >= 6);
    }

    #[gpui::test]
    fn columns_are_added_renamed_and_removed(cx: &mut TestAppContext) {
        let (editor, _, cx) = open(cx, Board::blank());
        cx.update(|window, cx| editor.update(cx, |e, cx| e.add_column(window, cx)));
        assert_eq!(editor.read_with(cx, |e, _| e.editing), Some(Editing::Column(3)));
        // The name is selected: typing replaces it.
        cx.simulate_input("Review");
        cx.simulate_keystrokes("enter");
        assert_eq!(board(&editor, cx).columns[3].name, "Review");
        cx.update(|window, cx| editor.update(cx, |e, cx| e.start_column_edit(0, window, cx)));
        cx.simulate_input("  ");
        cx.simulate_keystrokes("enter");
        assert_eq!(board(&editor, cx).columns[0].name, "To do", "a blank name is refused");
        cx.update(|window, cx| editor.update(cx, |e, cx| e.remove_column(1, window, cx)));
        let names: Vec<String> = board(&editor, cx).columns.into_iter().map(|c| c.name).collect();
        assert_eq!(names, ["To do", "Done", "Review"]);
    }

    #[gpui::test]
    fn escape_drops_the_edit_and_a_new_empty_card(cx: &mut TestAppContext) {
        let mut b = Board::blank();
        b.columns[1].cards.push("Keep".into());
        let (editor, _, cx) = open(cx, b);
        cx.update(|window, cx| editor.update(cx, |e, cx| e.start_card_edit(1, 0, window, cx)));
        cx.simulate_input("oops");
        cx.simulate_keystrokes("escape");
        cx.update(|window, cx| editor.update(cx, |e, cx| e.add_card(1, 0, window, cx)));
        cx.simulate_keystrokes("escape");
        assert_eq!(cards(&editor, cx)[1], ["Keep"]);
    }

    #[gpui::test]
    fn an_agents_board_keeps_the_edit_only_if_it_still_has_it(cx: &mut TestAppContext) {
        let mut b = Board::blank();
        b.columns[0].cards.push("a".into());
        let (editor, _, cx) = open(cx, b);
        cx.update(|window, cx| editor.update(cx, |e, cx| e.start_card_edit(0, 0, window, cx)));
        cx.simulate_input("!");
        let ArtifactContent::Board(mut saved) = editor.read_with(cx, |e, cx| e.content(cx)) else { panic!() };
        assert_eq!(saved.card(0, 0), Some("a!"), "an edit in progress is saved");
        saved.columns[2].cards.push("b".into());
        editor.update(cx, |e, cx| e.replace(ArtifactContent::Board(saved), cx));
        assert_eq!(editor.read_with(cx, |e, _| e.editing), Some(Editing::Card(0, 0)));
        editor.update(cx, |e, cx| e.replace(ArtifactContent::Board(Board::blank()), cx));
        assert_eq!(editor.read_with(cx, |e, _| e.editing), None);
        assert_eq!(board(&editor, cx), Board::blank());
    }
}
