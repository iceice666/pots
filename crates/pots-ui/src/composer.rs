//! A multi-line, soft-wrapping text input used as the message composer.
//! Enter sends, Shift+Enter inserts a newline.

use std::ops::Range;
use std::time::{Duration, Instant};

use gpui::{
    App, AvailableSpace, Bounds, ClipboardItem, Context, CursorStyle, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, GlobalElementId, KeyBinding,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    SharedString, Style, TextAlign, TextRun, UTF16Selection, Window, WrappedLine, actions, div,
    fill, point, prelude::*, px, relative, size,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::theme;

actions!(
    composer,
    [
        Backspace, Delete, Left, Right, Up, Down, SelectLeft, SelectRight, SelectUp, SelectDown,
        SelectAll, Home, End, Paste, Cut, Copy, Newline, Send, Undo, Redo,
    ]
);

const CONTEXT: &str = "Composer";

pub fn bind_keys(cx: &mut App) {
    let c = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, c),
        KeyBinding::new("delete", Delete, c),
        KeyBinding::new("left", Left, c),
        KeyBinding::new("right", Right, c),
        KeyBinding::new("up", Up, c),
        KeyBinding::new("down", Down, c),
        KeyBinding::new("shift-left", SelectLeft, c),
        KeyBinding::new("shift-right", SelectRight, c),
        KeyBinding::new("shift-up", SelectUp, c),
        KeyBinding::new("shift-down", SelectDown, c),
        KeyBinding::new("secondary-a", SelectAll, c),
        KeyBinding::new("secondary-v", Paste, c),
        KeyBinding::new("secondary-c", Copy, c),
        KeyBinding::new("secondary-x", Cut, c),
        KeyBinding::new("cmd-z", Undo, c),
        KeyBinding::new("ctrl-z", Undo, c),
        KeyBinding::new("cmd-shift-z", Redo, c),
        KeyBinding::new("ctrl-shift-z", Redo, c),
        KeyBinding::new("home", Home, c),
        KeyBinding::new("end", End, c),
        KeyBinding::new("cmd-left", Home, c),
        KeyBinding::new("cmd-right", End, c),
        KeyBinding::new("shift-enter", Newline, c),
        KeyBinding::new("enter", Send, c),
    ]);
}

pub enum ComposerEvent {
    Submit(String),
    /// The text changed: typing, deleting, pasting, undo/redo, or an input
    /// method composing (see `is_composing`). Not sent by `set_text`.
    Changed,
}

pub struct Composer {
    focus_handle: FocusHandle,
    content: String,
    placeholder: SharedString,
    /// Single-line input without the formatting/send chrome. Enter emits
    /// `Submit` with the current text but keeps it.
    compact: bool,
    /// A compact field sized for the top bar (the search box).
    small: bool,
    /// Shows `*`s instead of the text (an API key), and won't copy it.
    secret: bool,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    is_selecting: bool,
    layout: Option<TextLayoutCache>,
    undo_stack: Vec<Snapshot>,
    redo_stack: Vec<Snapshot>,
    /// The kind of the last recorded edit, used to coalesce typing into one undo step.
    last_edit: Option<(EditKind, Instant)>,
}

/// Editor state captured before an edit, restored by undo/redo.
#[derive(Clone)]
struct Snapshot {
    content: String,
    selected_range: Range<usize>,
    selection_reversed: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum EditKind {
    Typing,
    Deleting,
    Other,
}

const UNDO_LIMIT: usize = 200;
const COALESCE_WINDOW: Duration = Duration::from_millis(1000);

/// Shaped lines from the last paint, used for hit-testing and cursor movement.
struct TextLayoutCache {
    bounds: Bounds<Pixels>,
    line_height: Pixels,
    /// (byte offset where the hard line starts, y offset, shaped+wrapped line)
    lines: Vec<(usize, Pixels, WrappedLine)>,
}

impl EventEmitter<ComposerEvent> for Composer {}

impl Composer {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: String::new(),
            placeholder: "Message".into(),
            compact: false,
            small: false,
            secret: false,
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            is_selecting: false,
            layout: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            last_edit: None,
        }
    }

    /// A single-line field (e.g. a thread name).
    pub fn compact(cx: &mut Context<Self>) -> Self {
        Self {
            compact: true,
            ..Self::new(cx)
        }
    }

    /// A single-line field small enough for the top bar (the search box).
    pub fn small(cx: &mut Context<Self>) -> Self {
        Self {
            small: true,
            ..Self::compact(cx)
        }
    }

    /// A single-line field whose text isn't shown (an API key).
    pub fn secret(cx: &mut Context<Self>) -> Self {
        Self {
            secret: true,
            ..Self::compact(cx)
        }
    }

    /// The text as drawn: masked for a secret field, one `*` per byte so
    /// offsets into it are offsets into the real text.
    fn display_text(&self) -> String {
        if self.secret {
            "*".repeat(self.content.len())
        } else {
            self.content.clone()
        }
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.content = text.into();
        self.selected_range = self.content.len()..self.content.len();
        self.selection_reversed = false;
        self.marked_range = None;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.last_edit = None;
        cx.notify();
    }

    pub fn set_placeholder(&mut self, placeholder: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.placeholder = placeholder.into();
        cx.notify();
    }

    pub fn submit(&mut self, cx: &mut Context<Self>) {
        if self.compact {
            cx.emit(ComposerEvent::Submit(self.content.trim().to_string()));
            return;
        }
        let text = self.content.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.content.clear();
        self.selected_range = 0..0;
        self.selection_reversed = false;
        self.marked_range = None;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.last_edit = None;
        cx.emit(ComposerEvent::Submit(text));
        cx.notify();
    }

    /// An input method is composing text that isn't final yet.
    pub fn is_composing(&self) -> bool {
        self.marked_range.is_some()
    }

    fn is_empty(&self) -> bool {
        self.content.is_empty()
    }

    // --- undo / redo -----------------------------------------------------

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            content: self.content.clone(),
            selected_range: self.selected_range.clone(),
            selection_reversed: self.selection_reversed,
        }
    }

    fn restore(&mut self, snapshot: Snapshot) {
        self.content = snapshot.content;
        self.selected_range = snapshot.selected_range;
        self.selection_reversed = snapshot.selection_reversed;
        self.marked_range = None;
        self.last_edit = None;
    }

    /// Record the pre-edit state. Consecutive single-character typing (or
    /// deleting) with a collapsed cursor is merged into one undo step; a
    /// space, newline, paste, cursor move or pause starts a new one.
    fn record_edit(&mut self, new_text: &str) {
        let kind = if new_text.is_empty() {
            EditKind::Deleting
        } else if new_text.chars().count() == 1 && !new_text.contains(char::is_whitespace) {
            EditKind::Typing
        } else {
            EditKind::Other
        };
        let now = Instant::now();
        let coalesce = kind != EditKind::Other
            && self.selected_range.is_empty()
            && self
                .last_edit
                .is_some_and(|(last, at)| last == kind && now - at < COALESCE_WINDOW);
        if !coalesce {
            self.undo_stack.push(self.snapshot());
            if self.undo_stack.len() > UNDO_LIMIT {
                self.undo_stack.remove(0);
            }
        }
        self.redo_stack.clear();
        self.last_edit = Some((kind, now));
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(snapshot) = self.undo_stack.pop() {
            self.redo_stack.push(self.snapshot());
            self.restore(snapshot);
            cx.emit(ComposerEvent::Changed);
            cx.notify();
        }
    }

    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(snapshot) = self.redo_stack.pop() {
            self.undo_stack.push(self.snapshot());
            self.restore(snapshot);
            cx.emit(ComposerEvent::Changed);
            cx.notify();
        }
    }

    // --- actions ---------------------------------------------------------

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx)
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx)
        }
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.vertical_target(-1);
        self.move_to(target, cx);
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.vertical_target(1);
        self.move_to(target, cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.vertical_target(-1);
        self.select_to(target, cx);
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.vertical_target(1);
        self.select_to(target, cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx)
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        let line_start = self.content[..self.cursor_offset()]
            .rfind('\n')
            .map_or(0, |i| i + 1);
        self.move_to(line_start, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        let cursor = self.cursor_offset();
        let line_end = self.content[cursor..]
            .find('\n')
            .map_or(self.content.len(), |i| cursor + i);
        self.move_to(line_end, cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        // Pass an explicit range (rather than selecting first) so the cursor
        // stays collapsed and repeated deletes coalesce into one undo step.
        let range = if self.selected_range.is_empty() {
            let cursor = self.cursor_offset();
            Some(self.range_to_utf16(&(self.previous_boundary(cursor)..cursor)))
        } else {
            None
        };
        self.replace_text_in_range(range, "", window, cx)
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        let range = if self.selected_range.is_empty() {
            let cursor = self.cursor_offset();
            Some(self.range_to_utf16(&(cursor..self.next_boundary(cursor))))
        } else {
            None
        };
        self.replace_text_in_range(range, "", window, cx)
    }

    fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        if self.compact {
            return;
        }
        self.replace_text_in_range(None, "\n", window, cx)
    }

    fn send(&mut self, _: &Send, _: &mut Window, cx: &mut Context<Self>) {
        self.submit(cx);
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let text = text.replace("\r\n", "\n");
            let text = if self.compact { text.replace('\n', " ") } else { text };
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() && !self.secret {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() && !self.secret {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx)
        }
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle);
        self.is_selecting = true;
        let index = self.index_for_position(event.position);
        if event.modifiers.shift {
            self.select_to(index, cx);
        } else {
            self.move_to(index, cx)
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_position(event.position), cx);
        }
    }

    // --- helpers ---------------------------------------------------------

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selected_range != (offset..offset) {
            self.last_edit = None;
        }
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        cx.notify()
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset
        } else {
            self.selected_range.end = offset
        };
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify()
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }

    /// Position of a byte offset relative to the text origin.
    fn position_for_offset(&self, offset: usize) -> Option<Point<Pixels>> {
        let layout = self.layout.as_ref()?;
        let (start, y, line) = layout
            .lines
            .iter()
            .rev()
            .find(|(start, _, _)| *start <= offset)?;
        let p = line.position_for_index(offset - start, layout.line_height)?;
        Some(point(p.x, p.y + *y))
    }

    /// Byte offset for a point relative to the text origin.
    fn offset_for_local_point(&self, p: Point<Pixels>) -> usize {
        let Some(layout) = self.layout.as_ref() else {
            return 0;
        };
        if p.y < px(0.) {
            return 0;
        }
        for (start, y, line) in &layout.lines {
            let height = line.size(layout.line_height).height;
            if p.y < *y + height {
                let local = point(p.x.max(px(0.)), p.y - *y);
                let ix = match line.closest_index_for_position(local, layout.line_height) {
                    Ok(ix) | Err(ix) => ix,
                };
                let mut ix = (start + ix).min(self.content.len());
                // A secret field draws one `*` per byte: land on a character.
                while !self.content.is_char_boundary(ix) {
                    ix -= 1;
                }
                return ix;
            }
        }
        self.content.len()
    }

    fn index_for_position(&self, position: Point<Pixels>) -> usize {
        match self.layout.as_ref() {
            Some(layout) => self.offset_for_local_point(position - layout.bounds.origin),
            None => 0,
        }
    }

    fn vertical_target(&self, direction: i32) -> usize {
        let cursor = self.cursor_offset();
        let (Some(layout), Some(p)) = (self.layout.as_ref(), self.position_for_offset(cursor)) else {
            return cursor;
        };
        let y = p.y + layout.line_height * (direction as f32) + layout.line_height / 2.;
        if y < px(0.) {
            return 0;
        }
        self.offset_for_local_point(point(p.x, y))
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }
}

impl EntityInputHandler for Composer {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range.as_ref().map(|r| self.range_to_utf16(r))
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        // What was being composed stays, as final text.
        if self.marked_range.take().is_some() {
            cx.emit(ComposerEvent::Changed);
        }
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        if self.marked_range.is_none() && (!range.is_empty() || !new_text.is_empty()) {
            self.record_edit(new_text);
        }
        self.content.replace_range(range.clone(), new_text);
        let cursor = range.start + new_text.len();
        self.selected_range = cursor..cursor;
        self.selection_reversed = false;
        self.marked_range.take();
        cx.emit(ComposerEvent::Changed);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        if self.marked_range.is_none() {
            // Start of an IME composition: the whole composition is one undo step.
            self.undo_stack.push(self.snapshot());
            self.redo_stack.clear();
            self.last_edit = Some((EditKind::Other, Instant::now()));
        }
        self.content.replace_range(range.clone(), new_text);
        self.marked_range = (!new_text.is_empty()).then(|| range.start..range.start + new_text.len());
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .map(|r| r.start + range.start..r.end + range.start)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        cx.emit(ComposerEvent::Changed);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.range_from_utf16(&range_utf16);
        let line_height = self.layout.as_ref()?.line_height;
        let start = self.position_for_offset(range.start)?;
        let end = self.position_for_offset(range.end)?;
        Some(Bounds::from_corners(
            bounds.origin + start,
            bounds.origin + point(end.x, end.y + line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let offset = self.index_for_position(point);
        Some(self.offset_to_utf16(offset))
    }
}

impl Focusable for Composer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);
        let can_send = !self.content.trim().is_empty();
        div()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::send))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .flex()
            .flex_col()
            .w_full()
            .rounded_lg()
            .border_1()
            .border_color(if focused { theme::composer_focus() } else { theme::border() })
            .bg(theme::bg())
            // Formatting hint bar.
            .when(!self.compact, |d| d.child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_x_3()
                    .px_3()
                    .py_1()
                    .rounded_t_lg()
                    .bg(theme::composer_toolbar())
                    .text_xs()
                    .text_color(theme::muted())
                    .children(["**bold**", "_italic_", "~strike~", "`code`", "```block```", "| table |", "> quote"]
                        .map(|hint| div().font_family(theme::MONO).child(hint))),
            ))
            .child(
                div()
                    .id("composer-text")
                    .px_3()
                    .py_2()
                    .min_h(px(if self.compact { 38. } else { 44. }))
                    // A long single-line value (a path) wraps: up to three lines show.
                    .max_h(px(if self.compact { 82. } else { 240. }))
                    .overflow_y_scroll()
                    .cursor(CursorStyle::IBeam)
                    .text_size(px(15.))
                    .line_height(px(22.))
                    // Fits the 26 px top bar box with the border.
                    .when(self.small, |d| {
                        d.px_2().py(px(2.)).min_h(px(24.)).max_h(px(24.)).text_size(px(13.)).line_height(px(20.))
                    })
                    .text_color(theme::text())
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_move(cx.listener(Self::on_mouse_move))
                    .child(ComposerText { input: cx.entity() }),
            )
            .when(!self.compact, |d| d.child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .pb_2()
                    .text_xs()
                    .text_color(theme::muted())
                    .child("Shift + Return for a new line")
                    .child(
                        div()
                            .id("send")
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .when(can_send, |d| {
                                d.bg(theme::send())
                                    .text_color(gpui::white())
                                    .cursor_pointer()
                                    .hover(|s| s.bg(theme::send_hover()))
                            })
                            .when(!can_send, |d| {
                                d.bg(theme::composer_toolbar()).text_color(theme::faint())
                            })
                            .child("Send ➤")
                            .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                    ),
            ))
    }
}

// ---------------------------------------------------------------------------

struct ComposerText {
    input: Entity<Composer>,
}

struct PrepaintState {
    lines: Vec<(usize, Pixels, WrappedLine)>,
    line_height: Pixels,
    cursor: Option<PaintQuad>,
    selections: Vec<PaintQuad>,
}

fn shape(
    text: &str,
    placeholder: bool,
    width: Pixels,
    window: &mut Window,
) -> (Vec<(usize, Pixels, WrappedLine)>, Pixels) {
    let style = window.text_style();
    let font_size = style.font_size.to_pixels(window.rem_size());
    let line_height = window.line_height();
    let run = TextRun {
        len: text.len(),
        font: style.font(),
        color: if placeholder { theme::faint() } else { style.color },
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let shaped = window
        .text_system()
        .shape_text(SharedString::from(text.to_string()), font_size, &[run], Some(width), None)
        .unwrap_or_default();
    let mut lines = Vec::new();
    let mut offset = 0;
    let mut y = px(0.);
    for line in shaped {
        let height = line.size(line_height).height;
        let len = line.len();
        lines.push((offset, y, line));
        offset += len + 1; // + '\n'
        y += height;
    }
    (lines, line_height)
}

impl IntoElement for ComposerText {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for ComposerText {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let input = self.input.read(cx);
        let (text, placeholder) = if input.is_empty() {
            (input.placeholder.to_string(), true)
        } else {
            (input.display_text(), false)
        };
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        let layout_id = window.request_measured_layout(style, move |known, available, window, _| {
            let width = known.width.unwrap_or(match available.width {
                AvailableSpace::Definite(w) => w,
                _ => px(10_000.),
            });
            let (lines, line_height) = shape(&text, placeholder, width, window);
            let height = lines
                .iter()
                .map(|(_, _, l)| l.size(line_height).height)
                .fold(px(0.), |a, b| a + b)
                .max(line_height);
            size(width, height)
        });
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> PrepaintState {
        let input = self.input.read(cx);
        let empty = input.is_empty();
        let text = if empty {
            input.placeholder.to_string()
        } else {
            input.display_text()
        };
        let selected = input.selected_range.clone();
        let cursor_offset = input.cursor_offset();
        let (lines, line_height) = shape(&text, empty, bounds.size.width, window);

        let pos = |offset: usize| -> Point<Pixels> {
            if empty {
                return point(px(0.), px(0.));
            }
            lines
                .iter()
                .rev()
                .find(|(start, _, _)| *start <= offset)
                .and_then(|(start, y, line)| {
                    line.position_for_index(offset - start, line_height)
                        .map(|p| point(p.x, p.y + *y))
                })
                .unwrap_or_default()
        };

        let mut selections = Vec::new();
        let mut cursor = None;
        if selected.is_empty() {
            let p = pos(cursor_offset);
            cursor = Some(fill(
                Bounds::new(bounds.origin + p, size(px(2.), line_height)),
                theme::cursor(),
            ));
        } else {
            let (a, b) = (pos(selected.start), pos(selected.end));
            let left = bounds.left();
            let right = bounds.right();
            let color = theme::selection();
            if a.y == b.y {
                selections.push(fill(
                    Bounds::from_corners(
                        point(left + a.x, bounds.top() + a.y),
                        point(left + b.x, bounds.top() + a.y + line_height),
                    ),
                    color,
                ));
            } else {
                selections.push(fill(
                    Bounds::from_corners(
                        point(left + a.x, bounds.top() + a.y),
                        point(right, bounds.top() + a.y + line_height),
                    ),
                    color,
                ));
                if b.y > a.y + line_height {
                    selections.push(fill(
                        Bounds::from_corners(
                            point(left, bounds.top() + a.y + line_height),
                            point(right, bounds.top() + b.y),
                        ),
                        color,
                    ));
                }
                selections.push(fill(
                    Bounds::from_corners(
                        point(left, bounds.top() + b.y),
                        point(left + b.x, bounds.top() + b.y + line_height),
                    ),
                    color,
                ));
            }
        }

        PrepaintState {
            lines,
            line_height,
            cursor,
            selections,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        prepaint: &mut PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        for selection in prepaint.selections.drain(..) {
            window.paint_quad(selection);
        }
        let line_height = prepaint.line_height;
        for (_, y, line) in &prepaint.lines {
            line.paint(
                bounds.origin + point(px(0.), *y),
                line_height,
                TextAlign::Left,
                None,
                window,
                cx,
            )
            .ok();
        }
        if focus_handle.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }

        let empty = self.input.read(cx).is_empty();
        let lines = std::mem::take(&mut prepaint.lines);
        self.input.update(cx, |input, _| {
            input.layout = Some(TextLayoutCache {
                bounds,
                line_height,
                // While the placeholder is showing, there is no real text to hit-test.
                lines: if empty { Vec::new() } else { lines },
            });
        });
    }
}
