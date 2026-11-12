//! The table artifact's editor: gpui-component's `DataTable` with cells
//! and column names edited in place.
//!
//! The table only selects cells and moves the selection with the arrow
//! keys. Editing is added here: Enter, F2 or a double-click puts an `Input`
//! in the selected cell (or, for a column name, in its header). While
//! editing, Enter/Tab/Up/Down keep the text and move on, Escape drops it,
//! and Left/Right move the caret. Keys an input method is composing with
//! never commit or move: see `composing`.

use gpui::{
    App, Context, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, FontWeight, KeyBinding,
    SharedString, Subscription, WeakEntity, Window, actions, div, prelude::*, px,
};
use gpui_component::Sizable;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::table::{Column, DataTable, TableDelegate, TableEvent, TableState};

use pots_agent::data::{ArtifactContent, Table};

use crate::artifact::{ArtifactEditor, EditorEvent};
use crate::theme;

actions!(
    sheet,
    [
        StartEdit, ClearCell, NextCell, PrevCell, CommitDown, CommitUp, CommitAndGoDown, CommitAndGoUp, CancelEdit,
    ]
);

/// On the editor, around the table.
const CONTEXT: &str = "Sheet";
/// Around the input of the cell or column name being edited.
const EDIT_CONTEXT: &str = "SheetEdit";
const COLUMN_WIDTH: f32 = 160.;

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys(bindings());
}

/// Must be bound after `gpui_component::init`: at the same depth as the
/// input's own bindings, the later binding wins.
pub(crate) fn bindings() -> Vec<KeyBinding> {
    let table = Some(CONTEXT);
    let edit = Some("SheetEdit > Input");
    vec![
        KeyBinding::new("enter", StartEdit, table),
        KeyBinding::new("f2", StartEdit, table),
        KeyBinding::new("backspace", ClearCell, table),
        KeyBinding::new("delete", ClearCell, table),
        KeyBinding::new("tab", NextCell, table),
        KeyBinding::new("shift-tab", PrevCell, table),
        KeyBinding::new("enter", CommitDown, edit),
        KeyBinding::new("shift-enter", CommitUp, edit),
        KeyBinding::new("tab", NextCell, edit),
        KeyBinding::new("shift-tab", PrevCell, edit),
        KeyBinding::new("down", CommitAndGoDown, edit),
        KeyBinding::new("up", CommitAndGoUp, edit),
        KeyBinding::new("escape", CancelEdit, edit),
    ]
}

/// What's being edited in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Editing {
    Cell(usize, usize),
    Column(usize),
}

/// The table's data, and how the table draws it.
pub struct SheetDelegate {
    table: Table,
    editing: Option<Editing>,
    input: Entity<InputState>,
    editor: WeakEntity<TableEditor>,
}

impl TableDelegate for SheetDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.table.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.table.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        Column::new(SharedString::from(format!("c{col_ix}")), self.table.columns[col_ix].clone())
            .width(px(COLUMN_WIDTH))
            .min_width(px(60.))
            .movable(false)
    }

    fn render_th(&mut self, col_ix: usize, _: &mut Window, _: &mut Context<TableState<Self>>) -> impl IntoElement {
        if self.editing == Some(Editing::Column(col_ix)) {
            return self.render_input().into_any_element();
        }
        let editor = self.editor.clone();
        div()
            .id(("sheet-th", col_ix))
            .w_full()
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .font_weight(FontWeight::SEMIBOLD)
            .child(self.table.columns[col_ix].clone())
            .on_click(move |e, window, cx| {
                if e.click_count() == 2 {
                    // Not from inside the table's own update.
                    let editor = editor.clone();
                    window.defer(cx, move |window, cx| {
                        editor.update(cx, |e, cx| e.start_column_edit(col_ix, window, cx)).ok();
                    });
                }
            })
            .into_any_element()
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        if self.editing == Some(Editing::Cell(row_ix, col_ix)) {
            return self.render_input().into_any_element();
        }
        div()
            .w_full()
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .child(self.table.cell(row_ix, col_ix).to_string())
            .into_any_element()
    }

    fn cell_label(&self, row_ix: usize, col_ix: usize, _: &App) -> Option<SharedString> {
        Some(self.table.cell(row_ix, col_ix).to_string().into())
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _: &App) -> String {
        self.table.cell(row_ix, col_ix).to_string()
    }
}

impl SheetDelegate {
    fn render_input(&self) -> impl IntoElement {
        div()
            .key_context(EDIT_CONTEXT)
            .w_full()
            .child(Input::new(&self.input).appearance(false).xsmall())
    }
}

pub struct TableEditor {
    state: Entity<TableState<SheetDelegate>>,
    input: Entity<InputState>,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<EditorEvent> for TableEditor {}

impl TableEditor {
    pub fn new(table: Table, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx));
        let delegate = SheetDelegate { table, editing: None, input: input.clone(), editor: cx.entity().downgrade() };
        let state = cx.new(|cx| {
            TableState::new(delegate, window, cx)
                .cell_selectable(true)
                .row_selectable(false)
                .col_selectable(false)
                .col_movable(false)
                .sortable(false)
                .loop_selection(false)
        });
        let subscriptions = vec![
            cx.subscribe_in(&state, window, |this, _, event: &TableEvent, window, cx| {
                if let TableEvent::DoubleClickedCell(row, col) = event {
                    this.start_cell_edit(*row, *col, window, cx);
                }
            }),
            // Clicking elsewhere keeps what was typed, as a spreadsheet does.
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::Blur = event {
                    this.commit(window, cx);
                }
            }),
        ];
        Self { state, input, focus: cx.focus_handle(), _subscriptions: subscriptions }
    }

    fn table(&self, cx: &App) -> Table {
        self.state.read(cx).delegate().table.clone()
    }

    fn editing(&self, cx: &App) -> Option<Editing> {
        self.state.read(cx).delegate().editing
    }

    fn selected(&self, cx: &App) -> Option<(usize, usize)> {
        self.state.read(cx).selected_cell()
    }

    /// Change the table's data and redraw it; `changed` says whether that
    /// changed anything worth saving.
    fn update_table(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut Table) -> bool) -> bool {
        let changed = self.state.update(cx, |state, cx| {
            let changed = f(&mut state.delegate_mut().table);
            cx.notify();
            changed
        });
        if changed {
            cx.emit(EditorEvent::Changed);
        }
        changed
    }

    /// The columns changed: the table lays them out again.
    fn refresh_columns(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.refresh(cx);
            cx.notify();
        });
    }

    fn select(&mut self, row: usize, col: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            let rows = state.delegate().table.rows.len();
            let cols = state.delegate().table.columns.len();
            if rows == 0 || cols == 0 {
                return state.clear_selection(cx);
            }
            let (row, col) = (row.min(rows - 1), col.min(cols - 1));
            state.set_selected_cell(row, col, cx);
            state.scroll_to_row(row, cx);
            state.scroll_to_col(col, cx);
        });
        self.focus_table(window, cx);
    }

    fn focus_table(&self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = self.state.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
    }

    // --- editing in place --------------------------------------------------

    fn start_cell_edit(&mut self, row: usize, col: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.commit(window, cx);
        let text = self.state.read(cx).delegate().table.cell(row, col).to_string();
        self.state.update(cx, |state, cx| {
            if state.selected_cell() != Some((row, col)) {
                state.set_selected_cell(row, col, cx);
            }
        });
        self.start_edit(Editing::Cell(row, col), text, window, cx);
    }

    fn start_column_edit(&mut self, col: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.commit(window, cx);
        let Some(name) = self.state.read(cx).delegate().table.columns.get(col).cloned() else { return };
        self.start_edit(Editing::Column(col), name, window, cx);
    }

    /// A cell's text is edited from its end, as in a spreadsheet; a column's
    /// name is selected whole, to be typed over.
    fn start_edit(&mut self, editing: Editing, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| {
            input.set_value(text, window, cx);
            if let Editing::Column(_) = editing {
                input.select_all(window, cx);
            }
            input.focus(window, cx);
        });
        self.state.update(cx, |state, cx| {
            state.delegate_mut().editing = Some(editing);
            cx.notify();
        });
    }

    /// Keep what's typed in the cell or column name being edited, if any.
    /// Returns where the edit was, and whether it changed the table.
    fn commit_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<(Editing, bool)> {
        let editing = self.editing(cx)?;
        let text = self.input.read(cx).value().to_string();
        self.state.update(cx, |state, _| state.delegate_mut().editing = None);
        let changed = match editing {
            Editing::Cell(row, col) => self.update_table(cx, |t| t.set_cell(row, col, &text)),
            Editing::Column(col) => {
                let changed = self.update_table(cx, |t| t.rename_column(col, &text));
                if changed {
                    self.refresh_columns(cx);
                }
                changed
            }
        };
        self.input.update(cx, |input, cx| input.set_value("", window, cx));
        cx.notify();
        Some((editing, changed))
    }

    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<Editing> {
        self.commit_edit(window, cx).map(|(editing, _)| editing)
    }

    /// Whether an input method is composing in the cell being edited: the
    /// keys it uses to pick and confirm characters must not commit or move.
    fn composing(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.editing(cx).is_some() && self.input.update(cx, |input, cx| input.marked_text_range(window, cx).is_some())
    }

    /// Commit, then select the cell `drow` rows and `dcol` columns away from
    /// where the edit was (or from the selected cell, if nothing was edited).
    /// Moving by column past either end goes on in the next or previous row.
    fn commit_and_move(&mut self, drow: isize, dcol: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.composing(window, cx) {
            return;
        }
        let from = match self.commit(window, cx) {
            Some(Editing::Cell(row, col)) => Some((row, col)),
            // A column name: back to its first cell.
            Some(Editing::Column(col)) => return self.select(0, col, window, cx),
            None => self.selected(cx),
        };
        let Some((row, col)) = from else {
            return self.select(0, 0, window, cx);
        };
        let table = self.table(cx);
        let (rows, cols) = (table.rows.len(), table.columns.len());
        let (mut row, mut col) = (row as isize + drow, col as isize + dcol);
        if col >= cols as isize && row + 1 < rows as isize {
            (row, col) = (row + 1, 0);
        } else if col < 0 && row > 0 {
            (row, col) = (row - 1, cols as isize - 1);
        }
        self.select(row.max(0) as usize, col.max(0) as usize, window, cx);
    }

    fn on_start_edit(&mut self, _: &StartEdit, window: &mut Window, cx: &mut Context<Self>) {
        match self.selected(cx) {
            Some((row, col)) => self.start_cell_edit(row, col, window, cx),
            None => self.select(0, 0, window, cx),
        }
    }

    fn on_clear_cell(&mut self, _: &ClearCell, _: &mut Window, cx: &mut Context<Self>) {
        if let Some((row, col)) = self.selected(cx) {
            self.update_table(cx, |t| t.set_cell(row, col, ""));
        }
    }

    fn on_cancel(&mut self, _: &CancelEdit, window: &mut Window, cx: &mut Context<Self>) {
        if self.composing(window, cx) {
            // Escape is the input method's, to drop what it is composing.
            return cx.propagate();
        }
        let Some(editing) = self.editing(cx) else { return };
        self.state.update(cx, |state, cx| {
            state.delegate_mut().editing = None;
            cx.notify();
        });
        self.input.update(cx, |input, cx| input.set_value("", window, cx));
        match editing {
            Editing::Cell(row, col) => self.select(row, col, window, cx),
            Editing::Column(col) => self.select(0, col, window, cx),
        }
    }

    // --- rows and columns --------------------------------------------------

    fn insert_row(&mut self, below: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.commit(window, cx);
        let (row, col) = self.selected(cx).unwrap_or((self.table(cx).rows.len(), 0));
        let at = if below { row + 1 } else { row };
        let mut new = 0;
        self.update_table(cx, |t| {
            new = t.insert_row(at);
            true
        });
        self.select(new, col, window, cx);
    }

    fn remove_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.commit(window, cx);
        let Some((row, col)) = self.selected(cx) else { return };
        if self.update_table(cx, |t| t.remove_row(row)) {
            self.select(row, col, window, cx);
        }
    }

    fn insert_column(&mut self, right: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.commit(window, cx);
        let (row, col) = self.selected(cx).unwrap_or((0, self.table(cx).columns.len()));
        let at = if right { col + 1 } else { col };
        let mut new = 0;
        self.update_table(cx, |t| {
            let name = t.next_column_name();
            new = t.insert_column(at, &name);
            true
        });
        self.refresh_columns(cx);
        self.select(row, new, window, cx);
    }

    fn remove_column(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.commit(window, cx);
        let Some((row, col)) = self.selected(cx) else { return };
        if self.update_table(cx, |t| t.remove_column(col)) {
            self.refresh_columns(cx);
            self.select(row, col, window, cx);
        }
    }

    fn rename_column(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let col = self.selected(cx).map_or(0, |(_, col)| col);
        self.start_column_edit(col, window, cx);
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected(cx);
        let columns = self.state.read(cx).delegate().table.columns.len();
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
        let sep = || div().flex_none().w(px(1.)).h(px(16.)).mx_1().bg(theme::border());
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
                button("row-above", "+ Row above", true)
                    .on_click(cx.listener(|this, _, window, cx| this.insert_row(false, window, cx))),
            )
            .child(
                button("row-below", "+ Row below", true)
                    .on_click(cx.listener(|this, _, window, cx| this.insert_row(true, window, cx))),
            )
            .child(
                button("row-delete", "Delete row", selected.is_some())
                    .on_click(cx.listener(|this, _, window, cx| this.remove_row(window, cx))),
            )
            .child(sep())
            .child(
                button("col-left", "+ Column left", true)
                    .on_click(cx.listener(|this, _, window, cx| this.insert_column(false, window, cx))),
            )
            .child(
                button("col-right", "+ Column right", true)
                    .on_click(cx.listener(|this, _, window, cx| this.insert_column(true, window, cx))),
            )
            .child(
                button("col-rename", "Rename column", columns > 0)
                    .on_click(cx.listener(|this, _, window, cx| this.rename_column(window, cx))),
            )
            .child(
                button("col-delete", "Delete column", selected.is_some() && columns > 1)
                    .on_click(cx.listener(|this, _, window, cx| this.remove_column(window, cx))),
            )
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
                    .child("Enter or double-click to edit · Tab to move on · Esc to undo the edit"),
            )
    }
}

impl ArtifactEditor for TableEditor {
    fn content(&self, cx: &App) -> ArtifactContent {
        ArtifactContent::Table(self.table(cx))
    }

    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.commit_edit(window, cx).is_some_and(|(_, changed)| changed)
    }

    fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected(cx).is_none() {
            self.select(0, 0, window, cx);
        } else {
            self.focus_table(window, cx);
        }
    }
}

impl Focusable for TableEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for TableEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .on_action(cx.listener(Self::on_start_edit))
            .on_action(cx.listener(Self::on_clear_cell))
            .on_action(cx.listener(|this, _: &NextCell, window, cx| this.commit_and_move(0, 1, window, cx)))
            .on_action(cx.listener(|this, _: &PrevCell, window, cx| this.commit_and_move(0, -1, window, cx)))
            .on_action(cx.listener(|this, _: &CommitDown, window, cx| this.commit_and_move(1, 0, window, cx)))
            .on_action(cx.listener(|this, _: &CommitUp, window, cx| this.commit_and_move(-1, 0, window, cx)))
            .on_action(cx.listener(|this, _: &CommitAndGoDown, window, cx| this.commit_and_move(1, 0, window, cx)))
            .on_action(cx.listener(|this, _: &CommitAndGoUp, window, cx| this.commit_and_move(-1, 0, window, cx)))
            .on_action(cx.listener(Self::on_cancel))
            .child(self.render_toolbar(cx))
            .child(div().flex_1().min_h_0().p_4().child(DataTable::new(&self.state).stripe(true).small()))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::{EntityInputHandler, TestAppContext, VisualTestContext};

    use super::*;

    /// A table editor in a test window, under gpui-component's `Root` as in
    /// the app (Root binds Tab to move the focus), focused like the app does
    /// on open; and how many times it said its content changed.
    fn open(cx: &mut TestAppContext, table: Table) -> (Entity<TableEditor>, Rc<RefCell<usize>>, &mut VisualTestContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            bind_keys(cx);
        });
        let mut editor = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| TableEditor::new(table, window, cx));
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

    fn table(editor: &Entity<TableEditor>, cx: &mut VisualTestContext) -> Table {
        editor.read_with(cx, |e, cx| e.table(cx))
    }

    fn selected(editor: &Entity<TableEditor>, cx: &mut VisualTestContext) -> Option<(usize, usize)> {
        editor.read_with(cx, |e, cx| e.selected(cx))
    }

    fn editing(editor: &Entity<TableEditor>, cx: &mut VisualTestContext) -> Option<Editing> {
        editor.read_with(cx, |e, cx| e.editing(cx))
    }

    #[gpui::test]
    fn enter_edits_then_keeps_the_text_and_moves_down(cx: &mut TestAppContext) {
        let (editor, changes, cx) = open(cx, Table::blank(3, 3));
        assert_eq!(selected(&editor, cx), Some((0, 0)), "opening selects the first cell");

        cx.simulate_keystrokes("enter");
        assert_eq!(editing(&editor, cx), Some(Editing::Cell(0, 0)));
        cx.simulate_input("apples");
        assert_eq!(table(&editor, cx).cell(0, 0), "", "nothing is kept while typing");
        cx.simulate_keystrokes("enter");
        assert_eq!(editing(&editor, cx), None);
        assert_eq!(table(&editor, cx).cell(0, 0), "apples");
        assert_eq!(selected(&editor, cx), Some((1, 0)), "Enter moves down");
        assert_eq!(*changes.borrow(), 1);

        // Up and Down keep the edit too, and move.
        cx.simulate_keystrokes("enter");
        cx.simulate_input("pears");
        cx.simulate_keystrokes("up");
        assert_eq!(table(&editor, cx).cell(1, 0), "pears");
        assert_eq!(selected(&editor, cx), Some((0, 0)));

        // Editing starts from the cell's text.
        cx.simulate_keystrokes("f2");
        cx.simulate_input("!");
        cx.simulate_keystrokes("enter");
        assert_eq!(table(&editor, cx).cell(0, 0), "apples!");
    }

    #[gpui::test]
    fn tab_moves_across_and_on_to_the_next_row(cx: &mut TestAppContext) {
        let (editor, _, cx) = open(cx, Table::blank(2, 2));
        cx.simulate_keystrokes("enter");
        cx.simulate_input("a");
        cx.simulate_keystrokes("tab");
        assert_eq!(selected(&editor, cx), Some((0, 1)));
        cx.simulate_keystrokes("enter");
        cx.simulate_input("b");
        cx.simulate_keystrokes("tab");
        assert_eq!(selected(&editor, cx), Some((1, 0)), "past the last column: the next row");
        assert_eq!(table(&editor, cx).rows[0], ["a", "b"]);
        // Without editing, too; and back with Shift-Tab.
        cx.simulate_keystrokes("shift-tab");
        assert_eq!(selected(&editor, cx), Some((0, 1)));
        cx.simulate_keystrokes("tab tab tab");
        assert_eq!(selected(&editor, cx), Some((1, 1)), "stops at the last cell");
    }

    #[gpui::test]
    fn escape_drops_the_edit(cx: &mut TestAppContext) {
        let (editor, changes, cx) = open(cx, Table::blank(2, 2));
        cx.simulate_keystrokes("enter");
        cx.simulate_input("oops");
        cx.simulate_keystrokes("escape");
        assert_eq!(editing(&editor, cx), None);
        assert_eq!(table(&editor, cx).cell(0, 0), "");
        assert_eq!(selected(&editor, cx), Some((0, 0)));
        assert_eq!(*changes.borrow(), 0);
        // Arrow keys move the selection again, through the table's own bindings.
        cx.simulate_keystrokes("right down");
        assert_eq!(selected(&editor, cx), Some((1, 1)));
    }

    #[gpui::test]
    fn keys_an_input_method_is_using_never_commit_or_move(cx: &mut TestAppContext) {
        let (editor, _, cx) = open(cx, Table::blank(2, 2));
        cx.simulate_keystrokes("enter");
        // Zhuyin/pinyin composing, not yet confirmed.
        let input = editor.read_with(cx, |e, _| e.input.clone());
        cx.update(|window, cx| {
            input.update(cx, |i, cx| i.replace_and_mark_text_in_range(None, "ㄋㄧˇ", None, window, cx));
        });
        for key in ["enter", "tab", "down", "up", "shift-tab"] {
            cx.simulate_keystrokes(key);
            assert_eq!(editing(&editor, cx), Some(Editing::Cell(0, 0)), "{key} while composing");
            assert_eq!(selected(&editor, cx), Some((0, 0)), "{key} while composing");
        }
        assert_eq!(table(&editor, cx).cell(0, 0), "");
        // Confirmed: now Enter keeps it.
        cx.update(|window, cx| input.update(cx, |i, cx| i.replace_text_in_range(None, "你", window, cx)));
        cx.simulate_keystrokes("enter");
        assert_eq!(table(&editor, cx).cell(0, 0), "你");
        assert_eq!(selected(&editor, cx), Some((1, 0)));
    }

    #[gpui::test]
    fn delete_clears_the_selected_cell(cx: &mut TestAppContext) {
        let mut t = Table::blank(1, 1);
        t.set_cell(0, 0, "x");
        let (editor, changes, cx) = open(cx, t);
        cx.simulate_keystrokes("backspace");
        assert_eq!(table(&editor, cx).cell(0, 0), "");
        assert_eq!(*changes.borrow(), 1);
    }

    #[gpui::test]
    fn rows_and_columns_are_added_renamed_and_removed_where_the_selection_is(cx: &mut TestAppContext) {
        let (editor, changes, cx) = open(cx, Table::blank(2, 2));
        cx.simulate_keystrokes("right");
        cx.update(|window, cx| editor.update(cx, |e, cx| e.insert_column(true, window, cx)));
        let t = table(&editor, cx);
        assert_eq!(t.columns, ["Column 1", "Column 2", "Column 3"]);
        assert_eq!(selected(&editor, cx), Some((0, 2)), "the new column is selected");

        cx.update(|window, cx| editor.update(cx, |e, cx| e.rename_column(window, cx)));
        assert_eq!(editing(&editor, cx), Some(Editing::Column(2)));
        // The old name is selected: typing replaces it.
        cx.simulate_input("Owner");
        cx.simulate_keystrokes("enter");
        assert_eq!(table(&editor, cx).columns[2], "Owner");
        assert_eq!(selected(&editor, cx), Some((0, 2)));

        cx.update(|window, cx| editor.update(cx, |e, cx| e.insert_row(true, window, cx)));
        assert_eq!(table(&editor, cx).rows.len(), 3);
        assert_eq!(selected(&editor, cx), Some((1, 2)));
        cx.update(|window, cx| editor.update(cx, |e, cx| e.remove_row(window, cx)));
        cx.update(|window, cx| editor.update(cx, |e, cx| e.remove_column(window, cx)));
        let t = table(&editor, cx);
        assert_eq!((t.columns.len(), t.rows.len()), (2, 2));
        assert!(t.rows.iter().all(|r| r.len() == 2));
        assert_eq!(*changes.borrow(), 5);
    }

    #[gpui::test]
    fn clicking_away_keeps_the_edit(cx: &mut TestAppContext) {
        let (editor, _, cx) = open(cx, Table::blank(2, 2));
        // Focus changes only reach listeners in the active window.
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        assert!(cx.update(|window, _| window.is_window_active()));
        cx.simulate_keystrokes("enter");
        cx.simulate_input("kept");
        // As clicking another cell does: the table takes the focus.
        cx.update(|window, cx| {
            let table = editor.read(cx).state.read(cx).focus_handle(cx);
            window.focus(&table, cx);
            window.draw(cx).clear(cx);
        });
        cx.run_until_parked();
        assert_eq!(editing(&editor, cx), None);
        assert_eq!(table(&editor, cx).cell(0, 0), "kept");
        assert_eq!(editor.read_with(cx, |e, cx| e.content(cx)), ArtifactContent::Table(table(&editor, cx)));
    }
}
