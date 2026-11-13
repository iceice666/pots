//! The whiteboard artifact's editor: notes, boxes and text on an endless
//! board, moved and sized with the mouse.
//!
//! The board is panned by dragging empty space (or with space held, the
//! hand tool, or two fingers on a trackpad) and zoomed with a pinch or
//! ⌘/Ctrl and the scroll wheel, around the pointer. A tool from the
//! toolbar (or its key: V, N, R, O, T, H) puts a new item where the board
//! is clicked; double-clicking empty board puts a note there.
//!
//! Hit testing is done here, in board units, rather than by each item's
//! element: one press decides between selecting, moving, resizing and
//! panning, and a drag goes on wherever the pointer goes, so moves and
//! releases are listened to on the whole window (see `render`).
//!
//! While an item's text is edited, a text area lies over it, outside the
//! board's key context, so typing never reaches the board's one-key
//! shortcuts. Escape or ⌘/Ctrl-Enter (or clicking away) keeps the text;
//! text left with nothing in it is removed.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    App, Bounds, Context, DispatchPhase, Entity, EventEmitter, FocusHandle, Focusable, Hsla, KeyBinding, KeyDownEvent,
    KeyUpEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PinchEvent, Pixels,
    ScrollEvent, Subscription, Window, actions, canvas, div, fill, point, prelude::*, px, relative, rgb, size,
};
use gpui_component::input::{InputEvent, RopeExt, Textarea, TextareaState};

use pots_agent::data::{ArtifactContent, Whiteboard};
use pots_agent::whiteboard::{self, Color, Shape, WhiteboardItem};

use crate::artifact::{ArtifactEditor, EditorEvent};
use crate::theme;

actions!(
    whiteboard,
    [
        ToolSelect, ToolNote, ToolBox, ToolRoundBox, ToolText, ToolHand, DeleteSelected, Duplicate, BringToFront,
        SendToBack, EditSelected, Deselect, ZoomIn, ZoomOut, ZoomFit, StopEditing,
    ]
);

/// On the board itself, where the one-key shortcuts are.
const CONTEXT: &str = "Whiteboard";
/// Around the text area of the item being edited.
const EDIT_CONTEXT: &str = "WhiteboardEdit";
/// The resize handle's side, in screen pixels.
const HANDLE: f32 = 10.;
/// Board units between the grid's dots.
const GRID: f32 = 24.;
const MIN_ZOOM: f32 = 0.2;
const MAX_ZOOM: f32 = 4.;
/// Space left around what the view is fitted to, in screen pixels.
const MARGIN: f32 = 40.;

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys(bindings());
}

/// Must be bound after `gpui_component::init`: at the same depth as the
/// text area's own bindings, the later binding wins.
pub(crate) fn bindings() -> Vec<KeyBinding> {
    let board = Some(CONTEXT);
    let edit = Some("WhiteboardEdit > Input");
    vec![
        KeyBinding::new("v", ToolSelect, board),
        KeyBinding::new("n", ToolNote, board),
        KeyBinding::new("r", ToolBox, board),
        KeyBinding::new("o", ToolRoundBox, board),
        KeyBinding::new("t", ToolText, board),
        KeyBinding::new("h", ToolHand, board),
        KeyBinding::new("backspace", DeleteSelected, board),
        KeyBinding::new("delete", DeleteSelected, board),
        KeyBinding::new(secondary!("d"), Duplicate, board),
        KeyBinding::new("]", BringToFront, board),
        KeyBinding::new("[", SendToBack, board),
        KeyBinding::new("enter", EditSelected, board),
        KeyBinding::new("escape", Deselect, board),
        KeyBinding::new(secondary!("="), ZoomIn, board),
        KeyBinding::new(secondary!("-"), ZoomOut, board),
        KeyBinding::new("shift-1", ZoomFit, board),
        KeyBinding::new("escape", StopEditing, edit),
        KeyBinding::new(secondary!("enter"), StopEditing, edit),
    ]
}

/// What a press on the board does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Select,
    /// Put a new item of this shape where the board is clicked.
    Add(Shape),
    /// Drag the board around.
    Hand,
}

/// A drag in progress: what the pointer moving does until it's released.
#[derive(Clone, Debug)]
enum Drag {
    /// From this screen point, the view's pan then.
    Pan { from: (f32, f32), pan: (f32, f32) },
    /// The selected items, from this board point, each from where it was.
    Move { from: (f32, f32), start: Vec<(u64, f32, f32)>, moved: bool },
    /// An item by its bottom-right corner, from this board point and size.
    Resize { id: u64, from: (f32, f32), size: (f32, f32), resized: bool },
}

/// A note's paper, or the tint of a box.
fn paper(color: Color) -> Hsla {
    rgb(match color {
        Color::Yellow => 0xfff1a6,
        Color::Blue => 0xcfe6ff,
        Color::Green => 0xd3f2d6,
        Color::Pink => 0xffd5e5,
        Color::Purple => 0xe4d8ff,
        Color::Gray => 0xe8e8e8,
    })
    .into()
}

/// A box's border, and a swatch's ring.
fn ink(color: Color) -> Hsla {
    rgb(match color {
        Color::Yellow => 0xd4a800,
        Color::Blue => 0x2f7fd6,
        Color::Green => 0x2f9e44,
        Color::Pink => 0xd6336c,
        Color::Purple => 0x7048e8,
        Color::Gray => 0x868e96,
    })
    .into()
}

fn xy(p: gpui::Point<Pixels>) -> (f32, f32) {
    (f32::from(p.x), f32::from(p.y))
}

pub struct WhiteboardEditor {
    board: Whiteboard,
    /// Ids of the selected items.
    selected: Vec<u64>,
    tool: Tool,
    /// The board point at the view's top left.
    pan: (f32, f32),
    zoom: f32,
    drag: Option<Drag>,
    /// The item whose text is being edited.
    editing: Option<u64>,
    input: Entity<TextareaState>,
    /// Where the board was last drawn, in the window.
    bounds: Rc<Cell<Bounds<Pixels>>>,
    /// Space is held: pressing drags the board around.
    space: bool,
    focus: FocusHandle,
    /// The board was replaced while an item was being edited: once drawn
    /// without its text area, the keyboard goes back to the board.
    refocus: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<EditorEvent> for WhiteboardEditor {}

impl WhiteboardEditor {
    pub fn new(board: Whiteboard, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| TextareaState::new(window, cx).soft_wrap(true));
        let subscriptions = vec![
            // Clicking away keeps what was typed.
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::Blur = event {
                    this.commit_edit(window, cx);
                }
            }),
        ];
        // Open with the items' top left near the view's.
        let pan = board.bounds().map_or((-MARGIN, -MARGIN), |b| (b.x - MARGIN, b.y - MARGIN));
        Self {
            board,
            selected: Vec::new(),
            tool: Tool::Select,
            pan,
            zoom: 1.,
            drag: None,
            editing: None,
            input,
            bounds: Rc::new(Cell::new(Bounds::default())),
            space: false,
            focus: cx.focus_handle(),
            refocus: false,
            _subscriptions: subscriptions,
        }
    }

    // --- where things are --------------------------------------------------

    fn origin(&self) -> (f32, f32) {
        xy(self.bounds.get().origin)
    }

    /// The board point under window point `screen`.
    fn to_board(&self, (sx, sy): (f32, f32)) -> (f32, f32) {
        let (ox, oy) = self.origin();
        ((sx - ox) / self.zoom + self.pan.0, (sy - oy) / self.zoom + self.pan.1)
    }

    /// Where board point `p` is, from the view's top left.
    fn to_view(&self, (bx, by): (f32, f32)) -> (f32, f32) {
        ((bx - self.pan.0) * self.zoom, (by - self.pan.1) * self.zoom)
    }

    /// Whether board point `p` is on the resize handle of the one item
    /// selected; that item's id if so.
    fn on_handle(&self, (x, y): (f32, f32)) -> Option<u64> {
        let [id] = self.selected[..] else { return None };
        let item = self.board.item(id)?;
        let reach = HANDLE / self.zoom;
        ((x - (item.x + item.w)).abs() <= reach && (y - (item.y + item.h)).abs() <= reach).then_some(id)
    }

    // --- the board as it is, edits in progress included -----------------------

    fn board_with_edit(&self, cx: &App) -> Whiteboard {
        let mut board = self.board.clone();
        if let Some(id) = self.editing {
            Self::apply_text(&mut board, id, &self.input.read(cx).value());
        }
        board
    }

    /// Set item `id`'s text, as committing does: text with nothing in it
    /// is removed. Returns whether that changed the board.
    fn apply_text(board: &mut Whiteboard, id: u64, text: &str) -> bool {
        let Some(item) = board.item_mut(id) else { return false };
        if item.shape == Shape::Text && text.trim().is_empty() {
            return board.remove(id);
        }
        if item.text == text {
            return false;
        }
        item.text = text.to_string();
        true
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    // --- editing text --------------------------------------------------------

    fn start_edit(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_edit(window, cx);
        let Some(text) = self.board.item(id).map(|i| i.text.clone()) else { return };
        self.input.update(cx, |input, cx| {
            input.set_value(text, window, cx);
            // Writing goes on from the end of what's there.
            let end = input.text().offset_to_position(input.text().len());
            input.set_cursor_position(end, window, cx);
        });
        self.selected = vec![id];
        self.editing = Some(id);
        cx.notify();
    }

    /// Keep what's typed in the item being edited, if any. Returns whether
    /// that changed the board.
    fn commit_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(id) = self.editing.take() else { return false };
        let text = self.input.read(cx).value().to_string();
        let changed = Self::apply_text(&mut self.board, id, &text);
        if self.board.item(id).is_none() {
            self.selected.retain(|&s| s != id);
        }
        self.input.update(cx, |input, cx| input.set_value("", window, cx));
        if changed {
            self.changed(cx);
        }
        cx.notify();
        changed
    }

    fn on_stop_editing(&mut self, _: &StopEditing, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_edit(window, cx);
        window.focus(&self.focus, cx);
    }

    // --- the pointer -----------------------------------------------------------

    fn mouse_down(&mut self, screen: (f32, f32), shift: bool, clicks: usize, window: &mut Window, cx: &mut Context<Self>) {
        // The focus is decided here: the board would otherwise take it
        // after this, away from a text area just opened.
        window.prevent_default();
        self.commit_edit(window, cx);
        window.focus(&self.focus, cx);
        let p = self.to_board(screen);
        if self.space || self.tool == Tool::Hand {
            self.drag = Some(Drag::Pan { from: screen, pan: self.pan });
            return cx.notify();
        }
        if let Tool::Add(shape) = self.tool {
            self.tool = Tool::Select;
            return self.add_at(shape, p, window, cx);
        }
        if let Some(id) = self.on_handle(p) {
            let item = self.board.item(id).expect("selected");
            self.drag = Some(Drag::Resize { id, from: p, size: (item.w, item.h), resized: false });
            return cx.notify();
        }
        match self.board.hit(p.0, p.1) {
            Some(id) if clicks >= 2 => self.start_edit(id, window, cx),
            Some(id) => {
                if shift {
                    match self.selected.iter().position(|&s| s == id) {
                        Some(ix) => {
                            self.selected.remove(ix);
                        }
                        None => self.selected.push(id),
                    }
                } else if !self.selected.contains(&id) {
                    self.selected = vec![id];
                }
                let start = self.selected.iter().filter_map(|&s| self.board.item(s)).map(|i| (i.id, i.x, i.y)).collect();
                self.drag = Some(Drag::Move { from: p, start, moved: false });
                cx.notify();
            }
            None if clicks >= 2 => self.add_at(Shape::Note, p, window, cx),
            None => {
                if !shift {
                    self.selected.clear();
                }
                self.drag = Some(Drag::Pan { from: screen, pan: self.pan });
                cx.notify();
            }
        }
    }

    fn mouse_move(&mut self, screen: (f32, f32), cx: &mut Context<Self>) {
        let p = self.to_board(screen);
        let Some(drag) = &mut self.drag else { return };
        match drag {
            Drag::Pan { from, pan } => {
                self.pan = (pan.0 - (screen.0 - from.0) / self.zoom, pan.1 - (screen.1 - from.1) / self.zoom);
            }
            Drag::Move { from, start, moved } => {
                let (dx, dy) = (p.0 - from.0, p.1 - from.1);
                *moved |= dx != 0. || dy != 0.;
                for &(id, x, y) in start.iter() {
                    if let Some(item) = self.board.item_mut(id) {
                        item.x = whiteboard::clamp_coord(x + dx);
                        item.y = whiteboard::clamp_coord(y + dy);
                    }
                }
            }
            Drag::Resize { id, from, size, resized } => {
                let (w, h) = whiteboard::clamp_size(size.0 + p.0 - from.0, size.1 + p.1 - from.1);
                if let Some(item) = self.board.item_mut(*id) {
                    *resized |= item.w != w || item.h != h;
                    (item.w, item.h) = (w, h);
                }
            }
        }
        cx.notify();
    }

    fn mouse_up(&mut self, cx: &mut Context<Self>) {
        match self.drag.take() {
            Some(Drag::Move { moved: true, .. } | Drag::Resize { resized: true, .. }) => self.changed(cx),
            Some(_) => cx.notify(),
            None => {}
        }
    }

    fn scroll(&mut self, e: &ScrollEvent, cx: &mut Context<Self>) {
        let (dx, dy) = xy(e.delta.pixel_delta(px(20.)));
        if e.modifiers.platform || e.modifiers.control {
            self.zoom_at(xy(e.position), (1. + dy * 0.01).clamp(0.5, 2.));
        } else {
            self.pan = (self.pan.0 - dx / self.zoom, self.pan.1 - dy / self.zoom);
        }
        cx.notify();
    }

    /// Zoom by `factor`, keeping the board point under window point `screen` there.
    fn zoom_at(&mut self, screen: (f32, f32), factor: f32) {
        let p = self.to_board(screen);
        self.zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let (ox, oy) = self.origin();
        self.pan = (p.0 - (screen.0 - ox) / self.zoom, p.1 - (screen.1 - oy) / self.zoom);
    }

    /// Zoom by `factor` around the view's middle.
    fn zoom_by(&mut self, factor: f32, cx: &mut Context<Self>) {
        let b = self.bounds.get();
        let middle = xy(b.center());
        self.zoom_at(middle, factor);
        cx.notify();
    }

    /// Pan and zoom so every item is in view (at most 100%).
    fn fit(&mut self, cx: &mut Context<Self>) {
        let (vw, vh) = xy(self.bounds.get().size.into());
        match self.board.bounds() {
            Some(b) if vw > 0. && vh > 0. => {
                self.zoom = ((vw - 2. * MARGIN) / b.w).min((vh - 2. * MARGIN) / b.h).clamp(MIN_ZOOM, 1.);
                self.pan = (b.x - (vw / self.zoom - b.w) / 2., b.y - (vh / self.zoom - b.h) / 2.);
            }
            _ => {
                self.zoom = 1.;
                self.pan = (-MARGIN, -MARGIN);
            }
        }
        cx.notify();
    }

    // --- changing items --------------------------------------------------------

    /// A new item of `shape` centered on board point `p`, selected and
    /// being edited.
    fn add_at(&mut self, shape: Shape, p: (f32, f32), window: &mut Window, cx: &mut Context<Self>) {
        let (w, h) = shape.default_size();
        let color = match shape {
            Shape::Note => Color::Yellow,
            _ => Color::Blue,
        };
        let id = self.board.add(shape, p.0 - w / 2., p.1 - h / 2., "", color);
        self.changed(cx);
        self.start_edit(id, window, cx);
    }

    fn set_tool(&mut self, tool: Tool, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_edit(window, cx);
        self.tool = tool;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn delete_selected(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        for id in std::mem::take(&mut self.selected) {
            changed |= self.board.remove(id);
        }
        if changed {
            self.changed(cx);
        }
    }

    fn duplicate(&mut self, cx: &mut Context<Self>) {
        let copies: Vec<u64> = self.selected.clone().into_iter().filter_map(|id| self.board.duplicate(id)).collect();
        if !copies.is_empty() {
            self.selected = copies;
            self.changed(cx);
        }
    }

    fn restack(&mut self, front: bool, cx: &mut Context<Self>) {
        if self.board.restack(&self.selected, front) {
            self.changed(cx);
        }
    }

    fn set_color(&mut self, color: Color, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_edit(window, cx);
        let mut changed = false;
        for &id in &self.selected {
            if let Some(item) = self.board.item_mut(id) {
                changed |= item.color != color;
                item.color = color;
            }
        }
        if changed {
            self.changed(cx);
        }
    }

    fn on_edit_selected(&mut self, _: &EditSelected, window: &mut Window, cx: &mut Context<Self>) {
        if let [id] = self.selected[..] {
            self.start_edit(id, window, cx);
        }
    }

    fn on_deselect(&mut self, _: &Deselect, _: &mut Window, cx: &mut Context<Self>) {
        self.selected.clear();
        self.tool = Tool::Select;
        cx.notify();
    }

    // --- drawing ---------------------------------------------------------------

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let button = |id: &'static str, label: &'static str, on: bool, enabled: bool| {
            div()
                .id(id)
                .flex_none()
                .h(px(26.))
                .px_2()
                .flex()
                .items_center()
                .rounded_md()
                .text_size(px(13.))
                .map(|d| match (on, enabled) {
                    (true, _) => d.cursor_pointer().bg(theme::reaction_bg()).text_color(theme::text()),
                    (false, true) => d.cursor_pointer().text_color(theme::text()).hover(|s| s.bg(theme::reaction_bg())),
                    (false, false) => d.text_color(theme::faint()),
                })
                .child(label)
        };
        let tool = |id: &'static str, label: &'static str, tooltip: &'static str, t: Tool| {
            button(id, label, self.tool == t, true)
                .tooltip(move |_, cx| crate::chat::tooltip(tooltip, cx))
                .on_click(cx.listener(move |this, _, window, cx| this.set_tool(t, window, cx)))
        };
        let sep = || div().flex_none().w(px(1.)).h(px(16.)).mx_1().bg(theme::border());
        let some = !self.selected.is_empty();
        let colorable = self.selected.iter().filter_map(|&id| self.board.item(id)).any(|i| i.shape != Shape::Text);
        let current = match &self.selected[..] {
            [id] => self.board.item(*id).map(|i| i.color),
            _ => None,
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
            .child(tool("wb-select", "Select", "Select and move (V)", Tool::Select))
            .child(tool("wb-note", "Note", "Sticky note (N)", Tool::Add(Shape::Note)))
            .child(tool("wb-box", "Box", "Box (R)", Tool::Add(Shape::Box)))
            .child(tool("wb-round", "Round", "Round box (O)", Tool::Add(Shape::RoundBox)))
            .child(tool("wb-text", "Text", "Text (T)", Tool::Add(Shape::Text)))
            .child(tool("wb-hand", "Hand", "Move the board (H, or hold Space)", Tool::Hand))
            .child(sep())
            .when(colorable, |d| {
                d.children(Color::ALL.into_iter().map(|color| {
                    div()
                        .id(("wb-color", color as usize))
                        .flex_none()
                        .size(px(18.))
                        .rounded_full()
                        .cursor_pointer()
                        .bg(paper(color))
                        .border_2()
                        .border_color(if current == Some(color) { ink(color) } else { paper(color) })
                        .hover(|s| s.border_color(ink(color)))
                        .on_click(cx.listener(move |this, _, window, cx| this.set_color(color, window, cx)))
                }))
                .child(sep())
            })
            .child(button("wb-front", "Front", false, some).on_click(cx.listener(|this, _, _, cx| this.restack(true, cx))))
            .child(button("wb-back", "Back", false, some).on_click(cx.listener(|this, _, _, cx| this.restack(false, cx))))
            .child(button("wb-delete", "Delete", false, some).on_click(cx.listener(|this, _, _, cx| this.delete_selected(cx))))
            .child(div().flex_1())
            .child(button("wb-zoom-out", "−", false, true).on_click(cx.listener(|this, _, _, cx| this.zoom_by(0.8, cx))))
            .child(
                div()
                    .flex_none()
                    .w(px(44.))
                    .text_align(gpui::TextAlign::Center)
                    .text_size(px(12.))
                    .text_color(theme::muted())
                    .child(format!("{}%", (self.zoom * 100.).round())),
            )
            .child(button("wb-zoom-in", "+", false, true).on_click(cx.listener(|this, _, _, cx| this.zoom_by(1.25, cx))))
            .child(button("wb-fit", "Fit", false, true).on_click(cx.listener(|this, _, _, cx| this.fit(cx))))
    }

    /// Where item `i` is drawn, from the view's top left: left, top, width, height.
    fn view_rect(&self, i: &WhiteboardItem) -> (f32, f32, f32, f32) {
        let (x, y) = self.to_view((i.x, i.y));
        (x, y, i.w * self.zoom, i.h * self.zoom)
    }

    fn render_item(&self, i: &WhiteboardItem) -> impl IntoElement {
        let z = self.zoom;
        let (x, y, w, h) = self.view_rect(i);
        let base = div()
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(w))
            .h(px(h))
            .overflow_hidden()
            .text_color(theme::text());
        let placeholder = i.text.is_empty();
        let text = div()
            .when(placeholder, |d| d.text_color(theme::faint()))
            .child(if placeholder { "Double-click to write".to_string() } else { i.text.clone() });
        match i.shape {
            Shape::Note => base
                .bg(paper(i.color))
                .rounded(px(2. * z))
                .shadow_md()
                .p(px(10. * z))
                .text_size(px(14. * z))
                .child(text),
            Shape::Box | Shape::RoundBox => base
                .bg(theme::bg())
                .border(px((2. * z).max(1.)))
                .border_color(ink(i.color))
                .when(i.shape == Shape::RoundBox, |d| d.rounded(px(14. * z)))
                .p(px(8. * z))
                .flex()
                .items_center()
                .justify_center()
                .text_align(gpui::TextAlign::Center)
                .text_size(px(14. * z))
                .child(text),
            Shape::Text => base.text_size(px(16. * z)).child(text),
        }
    }

    /// The selected items' outlines, and the resize handle of one.
    fn render_selection(&self) -> Vec<gpui::AnyElement> {
        let mut out = Vec::new();
        for &id in &self.selected {
            let Some(i) = self.board.item(id) else { continue };
            let (x, y, w, h) = self.view_rect(i);
            out.push(
                div()
                    .absolute()
                    .left(px(x - 3.))
                    .top(px(y - 3.))
                    .w(px(w + 6.))
                    .h(px(h + 6.))
                    .border_2()
                    .border_color(theme::sidebar_selected())
                    .rounded_sm()
                    .into_any_element(),
            );
            if self.selected.len() == 1 && self.editing.is_none() {
                out.push(
                    div()
                        .absolute()
                        .left(px(x + w - HANDLE / 2.))
                        .top(px(y + h - HANDLE / 2.))
                        .size(px(HANDLE))
                        .bg(theme::bg())
                        .border_2()
                        .border_color(theme::sidebar_selected())
                        .into_any_element(),
                );
            }
        }
        out
    }

    /// The text area over the item being edited.
    fn render_edit(&self) -> Option<impl IntoElement> {
        let i = self.board.item(self.editing?)?;
        let (x, y, w, h) = self.view_rect(i);
        let z = self.zoom;
        Some(
            div()
                .id("wb-edit")
                .key_context(EDIT_CONTEXT)
                .absolute()
                .left(px(x))
                .top(px(y))
                .w(px(w.max(80.)))
                .h(px(h.max(32.)))
                .occlude()
                .p(px(6. * z))
                .rounded_sm()
                .text_size(px(if i.shape == Shape::Text { 16. } else { 14. } * z))
                .map(|d| match i.shape {
                    Shape::Note => d.bg(paper(i.color)),
                    _ => d.bg(theme::bg()),
                })
                .border_2()
                .border_color(theme::sidebar_selected())
                .child(Textarea::new(&self.input).appearance(false).h(relative(1.))),
        )
    }
}

impl ArtifactEditor for WhiteboardEditor {
    fn content(&self, cx: &App) -> ArtifactContent {
        ArtifactContent::Whiteboard(self.board_with_edit(cx))
    }

    /// Typed text not kept yet, or items moved or sized by a drag not yet released.
    fn has_pending_edit(&self, cx: &App) -> bool {
        let dragged = matches!(self.drag, Some(Drag::Move { moved: true, .. } | Drag::Resize { resized: true, .. }));
        dragged || (self.editing.is_some() && self.board_with_edit(cx) != self.board)
    }

    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let dragged = matches!(self.drag.take(), Some(Drag::Move { moved: true, .. } | Drag::Resize { resized: true, .. }));
        self.commit_edit(window, cx) || dragged
    }

    fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.editing {
            Some(_) => self.input.update(cx, |input, cx| input.focus(window, cx)),
            None => window.focus(&self.focus, cx),
        }
    }

    /// A drag gives way to the agent's board. The item being edited stays
    /// open if the new board still has it with the text typed so far, so
    /// typing goes on. The selection keeps the items still there.
    fn replace(&mut self, content: ArtifactContent, cx: &mut Context<Self>) {
        let ArtifactContent::Whiteboard(board) = content else { return };
        self.drag = None;
        let text = self.input.read(cx).value().to_string();
        let keep = self.editing.is_some_and(|id| board.item(id).is_some_and(|i| i.text == text));
        if !keep && self.editing.take().is_some() {
            self.refocus = true;
        }
        self.selected.retain(|&id| board.item(id).is_some());
        self.board = board;
        cx.notify();
    }
}

impl Focusable for WhiteboardEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for WhiteboardEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.refocus) && self.input.focus_handle(cx).is_focused(window) {
            let focus = self.focus.clone();
            window.defer(cx, move |window, cx| window.focus(&focus, cx));
        }
        let this = cx.entity().downgrade();
        let cell = self.bounds.clone();
        let (pan, zoom) = (self.pan, self.zoom);
        // The grid's dots, and listening to the pointer wherever a drag takes it.
        let grid = canvas(
            move |bounds, _, _| {
                cell.set(bounds);
                let step = GRID * zoom;
                let (w, h) = xy(bounds.size.into());
                if step < 8. || (w / step) * (h / step) > 20_000. {
                    return Vec::new();
                }
                let first = |p: f32| ((p / GRID).ceil() * GRID - p) * zoom;
                let (x0, y0) = (first(pan.0), first(pan.1));
                let mut dots = Vec::new();
                let mut y = y0;
                while y < h {
                    let mut x = x0;
                    while x < w {
                        dots.push(point(bounds.origin.x + px(x), bounds.origin.y + px(y)));
                        x += step;
                    }
                    y += step;
                }
                dots
            },
            move |_, dots, window, _| {
                for dot in dots {
                    window.paint_quad(fill(Bounds::new(dot, size(px(1.5), px(1.5))), theme::border()));
                }
                let moved = this.clone();
                window.on_mouse_event(move |e: &MouseMoveEvent, phase, _, cx| {
                    if phase == DispatchPhase::Bubble {
                        moved.update(cx, |this, cx| this.mouse_move(xy(e.position), cx)).ok();
                    }
                });
                let released = this.clone();
                window.on_mouse_event(move |e: &MouseUpEvent, phase, _, cx| {
                    if phase == DispatchPhase::Bubble && e.button == MouseButton::Left {
                        released.update(cx, |this, cx| this.mouse_up(cx)).ok();
                    }
                });
            },
        )
        .absolute()
        .size_full();

        let panning = self.space || self.tool == Tool::Hand || matches!(self.drag, Some(Drag::Pan { .. }));
        let board = div()
            .id("wb-board")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .absolute()
            .size_full()
            .bg(theme::hover())
            .map(|d| match (panning, self.tool) {
                (true, _) if self.drag.is_some() => d.cursor_grabbing(),
                (true, _) => d.cursor_grab(),
                (false, Tool::Add(_)) => d.cursor_crosshair(),
                _ => d,
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, window, cx| {
                    this.mouse_down(xy(e.position), e.modifiers.shift, e.click_count, window, cx);
                }),
            )
            .on_scroll(cx.listener(|this, e: &ScrollEvent, _, cx| this.scroll(e, cx)))
            .on_pinch(cx.listener(|this, e: &PinchEvent, _, cx| {
                this.zoom_at(xy(e.position), 1. + e.delta);
                cx.notify();
            }))
            .on_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| {
                if e.keystroke.key == "space" && !this.space {
                    this.space = true;
                    cx.notify();
                }
            }))
            .on_key_up(cx.listener(|this, e: &KeyUpEvent, _, cx| {
                if e.keystroke.key == "space" {
                    this.space = false;
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &ToolSelect, window, cx| this.set_tool(Tool::Select, window, cx)))
            .on_action(cx.listener(|this, _: &ToolNote, window, cx| this.set_tool(Tool::Add(Shape::Note), window, cx)))
            .on_action(cx.listener(|this, _: &ToolBox, window, cx| this.set_tool(Tool::Add(Shape::Box), window, cx)))
            .on_action(cx.listener(|this, _: &ToolRoundBox, window, cx| this.set_tool(Tool::Add(Shape::RoundBox), window, cx)))
            .on_action(cx.listener(|this, _: &ToolText, window, cx| this.set_tool(Tool::Add(Shape::Text), window, cx)))
            .on_action(cx.listener(|this, _: &ToolHand, window, cx| this.set_tool(Tool::Hand, window, cx)))
            .on_action(cx.listener(|this, _: &DeleteSelected, _, cx| this.delete_selected(cx)))
            .on_action(cx.listener(|this, _: &Duplicate, _, cx| this.duplicate(cx)))
            .on_action(cx.listener(|this, _: &BringToFront, _, cx| this.restack(true, cx)))
            .on_action(cx.listener(|this, _: &SendToBack, _, cx| this.restack(false, cx)))
            .on_action(cx.listener(Self::on_edit_selected))
            .on_action(cx.listener(Self::on_deselect))
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.zoom_by(1.25, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.zoom_by(0.8, cx)))
            .on_action(cx.listener(|this, _: &ZoomFit, _, cx| this.fit(cx)))
            .child(grid)
            .children(self.board.items.iter().map(|i| self.render_item(i).into_any_element()))
            .children(self.render_selection())
            .when(self.board.items.is_empty(), |d| {
                d.child(
                    div()
                        .absolute()
                        .top(px(16.))
                        .left(px(16.))
                        .text_size(px(13.))
                        .text_color(theme::faint())
                        .child("Double-click anywhere for a note, or pick a tool above."),
                )
            });
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(board)
                    .children(self.render_edit())
                    .on_action(cx.listener(Self::on_stop_editing)),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::{Modifiers, TestAppContext, VisualTestContext};

    use super::*;

    fn open(cx: &mut TestAppContext, board: Whiteboard) -> (Entity<WhiteboardEditor>, Rc<RefCell<usize>>, &mut VisualTestContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            bind_keys(cx);
        });
        let mut editor = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| WhiteboardEditor::new(board, window, cx));
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
        draw(cx);
        (editor, changes, cx)
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
    }

    /// The window point over board point (`x`, `y`).
    fn at(editor: &Entity<WhiteboardEditor>, x: f32, y: f32, cx: &mut VisualTestContext) -> gpui::Point<Pixels> {
        let (vx, vy) = editor.read_with(cx, |e, _| {
            let (ox, oy) = e.origin();
            let (vx, vy) = e.to_view((x, y));
            (vx + ox, vy + oy)
        });
        point(px(vx), px(vy))
    }

    fn press(editor: &Entity<WhiteboardEditor>, x: f32, y: f32, mods: Modifiers, cx: &mut VisualTestContext) {
        let p = at(editor, x, y, cx);
        cx.simulate_mouse_move(p, None, mods);
        cx.simulate_mouse_down(p, MouseButton::Left, mods);
    }

    fn release(editor: &Entity<WhiteboardEditor>, x: f32, y: f32, mods: Modifiers, cx: &mut VisualTestContext) {
        let p = at(editor, x, y, cx);
        cx.simulate_mouse_up(p, MouseButton::Left, mods);
    }

    fn drag_to(editor: &Entity<WhiteboardEditor>, x: f32, y: f32, cx: &mut VisualTestContext) -> gpui::Point<Pixels> {
        let p = at(editor, x, y, cx);
        cx.simulate_mouse_move(p, MouseButton::Left, Modifiers::default());
        p
    }

    fn board(editor: &Entity<WhiteboardEditor>, cx: &mut VisualTestContext) -> Whiteboard {
        editor.read_with(cx, |e, _| e.board.clone())
    }

    fn one_note() -> Whiteboard {
        let mut b = Whiteboard::default();
        b.add(Shape::Note, 0., 0., "hello", Color::Yellow);
        b
    }

    #[gpui::test]
    fn a_tool_puts_an_item_where_clicked_ready_to_write_in(cx: &mut TestAppContext) {
        let (editor, changes, cx) = open(cx, Whiteboard::default());
        cx.simulate_keystrokes("n");
        assert_eq!(editor.read_with(cx, |e, _| e.tool), Tool::Add(Shape::Note));
        press(&editor, 300., 200., Modifiers::default(), cx);
        let p = at(&editor, 300., 200., cx);
        cx.simulate_mouse_up(p, MouseButton::Left, Modifiers::default());
        let b = board(&editor, cx);
        let note = &b.items[0];
        assert_eq!((note.shape, note.x + note.w / 2., note.y + note.h / 2.), (Shape::Note, 300., 200.), "centered on the click");
        assert_eq!(editor.read_with(cx, |e, _| (e.tool, e.editing)), (Tool::Select, Some(note.id)));
        draw(cx);

        // One-key shortcuts don't fire while typing: "v" and "n" are text.
        cx.simulate_input("van");
        assert_eq!(editor.read_with(cx, |e, _| e.tool), Tool::Select);
        assert!(editor.read_with(cx, |e, cx| e.has_pending_edit(cx)));
        cx.simulate_keystrokes("escape");
        assert_eq!(editor.read_with(cx, |e, _| e.editing), None);
        assert_eq!(board(&editor, cx).items[0].text, "van");
        assert_eq!(*changes.borrow(), 2, "added, then written in");

        // Text left empty goes.
        cx.simulate_keystrokes("t");
        press(&editor, 600., 600., Modifiers::default(), cx);
        draw(cx);
        cx.simulate_keystrokes("escape");
        assert_eq!(board(&editor, cx).items.len(), 1);
    }

    #[gpui::test]
    fn dragging_moves_the_item_and_saves_on_release(cx: &mut TestAppContext) {
        let (editor, changes, cx) = open(cx, one_note());
        press(&editor, 20., 20., Modifiers::default(), cx);
        assert_eq!(editor.read_with(cx, |e, _| e.selected.clone()), [1]);
        drag_to(&editor, 70., 50., cx);
        let i = board(&editor, cx).items[0].clone();
        assert_eq!((i.x, i.y), (50., 30.));
        assert_eq!(*changes.borrow(), 0, "not while dragging");
        assert!(editor.read_with(cx, |e, cx| e.has_pending_edit(cx)), "but the move is saved if the app asks");
        let p = at(&editor, 70., 50., cx);
        cx.simulate_mouse_up(p, MouseButton::Left, Modifiers::default());
        assert_eq!(*changes.borrow(), 1);
        assert!(!editor.read_with(cx, |e, cx| e.has_pending_edit(cx)));
    }

    #[gpui::test]
    fn the_handle_resizes_and_empty_board_pans(cx: &mut TestAppContext) {
        let (editor, changes, cx) = open(cx, one_note());
        press(&editor, 20., 20., Modifiers::default(), cx);
        release(&editor, 20., 20., Modifiers::default(), cx);
        draw(cx);
        // The note is 160 × 120: its handle is at (160, 120).
        press(&editor, 160., 120., Modifiers::default(), cx);
        drag_to(&editor, 260., 100., cx);
        release(&editor, 260., 100., Modifiers::default(), cx);
        let i = board(&editor, cx).items[0].clone();
        assert_eq!((i.x, i.y, i.w, i.h), (0., 0., 260., 100.));
        assert_eq!(*changes.borrow(), 1);

        // Empty board: dragging it pans the view, which isn't a change.
        let pan = editor.read_with(cx, |e, _| e.pan);
        press(&editor, 800., 800., Modifiers::default(), cx);
        assert_eq!(editor.read_with(cx, |e, _| e.selected.clone()), Vec::<u64>::new(), "and deselects");
        let to = at(&editor, 700., 750., cx);
        cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::default());
        assert_eq!(editor.read_with(cx, |e, _| e.pan), (pan.0 + 100., pan.1 + 50.));
        assert_eq!(*changes.borrow(), 1);
    }

    #[gpui::test]
    fn shift_selects_more_and_keys_delete_copy_and_restack(cx: &mut TestAppContext) {
        let mut b = one_note();
        b.add(Shape::Box, 300., 0., "b", Color::Blue);
        b.add(Shape::Text, 0., 300., "c", Color::Gray);
        let (editor, changes, cx) = open(cx, b);
        press(&editor, 10., 10., Modifiers::default(), cx);
        release(&editor, 10., 10., Modifiers::default(), cx);
        press(&editor, 310., 10., Modifiers::shift(), cx);
        release(&editor, 310., 10., Modifiers::shift(), cx);
        assert_eq!(editor.read_with(cx, |e, _| e.selected.clone()), [1, 2]);

        cx.simulate_keystrokes("[");
        assert_eq!(board(&editor, cx).items.iter().map(|i| i.id).collect::<Vec<_>>(), [1, 2, 3], "already at the back");
        cx.simulate_keystrokes("]");
        assert_eq!(board(&editor, cx).items.iter().map(|i| i.id).collect::<Vec<_>>(), [3, 1, 2]);
        cx.simulate_keystrokes(secondary!("d"));
        assert_eq!(board(&editor, cx).items.len(), 5);
        assert_eq!(editor.read_with(cx, |e, _| e.selected.clone()), [4, 5], "the copies are selected");
        cx.simulate_keystrokes("backspace");
        assert_eq!(board(&editor, cx).items.len(), 3);
        assert_eq!(*changes.borrow(), 3, "front, copy, delete (back changed nothing)");
    }

    #[gpui::test]
    fn zooming_keeps_the_point_under_the_pointer(cx: &mut TestAppContext) {
        let (editor, _, cx) = open(cx, one_note());
        let p = at(&editor, 100., 100., cx);
        editor.update(cx, |e, _| e.zoom_at(xy(p), 2.));
        assert_eq!(editor.read_with(cx, |e, _| (e.zoom, e.to_board(xy(p)))), (2., (100., 100.)));
        editor.update(cx, |e, _| e.zoom_at(xy(p), 100.));
        assert_eq!(editor.read_with(cx, |e, _| e.zoom), MAX_ZOOM);
        editor.update(cx, |e, cx| e.fit(cx));
        let (zoom, view) = editor.read_with(cx, |e, _| (e.zoom, e.to_view((80., 60.))));
        let size = editor.read_with(cx, |e, _| xy(e.bounds.get().size.into()));
        assert!(zoom <= 1., "{zoom}");
        assert!((view.0 - size.0 / 2.).abs() < 0.5 && (view.1 - size.1 / 2.).abs() < 0.5, "the note is in the middle: {view:?} of {size:?}");
    }

    #[gpui::test]
    fn an_agents_board_gives_way_to_nothing_but_the_edit_it_keeps(cx: &mut TestAppContext) {
        let (editor, _, cx) = open(cx, one_note());
        cx.update(|window, cx| editor.update(cx, |e, cx| e.start_edit(1, window, cx)));
        cx.simulate_input("!");
        let ArtifactContent::Whiteboard(mut saved) = editor.read_with(cx, |e, cx| e.content(cx)) else { panic!() };
        assert_eq!(saved.items[0].text, "hello!", "the text being typed is saved");
        saved.add(Shape::Note, 400., 0., "agent", Color::Pink);
        editor.update(cx, |e, cx| e.replace(ArtifactContent::Whiteboard(saved.clone()), cx));
        assert_eq!(editor.read_with(cx, |e, _| e.editing), Some(1), "typing goes on");

        saved.items[0].text = "rewritten".into();
        editor.update(cx, |e, cx| e.replace(ArtifactContent::Whiteboard(saved), cx));
        assert_eq!(editor.read_with(cx, |e, _| e.editing), None);
        assert_eq!(board(&editor, cx).items[0].text, "rewritten");
        assert_eq!(editor.read_with(cx, |e, _| e.selected.clone()), [1]);
    }
}
