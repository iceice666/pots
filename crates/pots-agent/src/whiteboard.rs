//! The whiteboard artifact's content: notes, boxes and text placed freely
//! on an endless board, and where to put a new one so it doesn't land on
//! another.
//!
//! Positions and sizes are in board units: pixels at 100% zoom. The board
//! has no edges; (0, 0) is only where placing starts. How the board is
//! panned and zoomed is each viewer's own, and isn't saved here.

use serde::{Deserialize, Serialize};

/// The space kept between items placed for someone.
pub const GAP: f32 = 24.;
/// The smallest an item may be made.
pub const MIN_W: f32 = 40.;
pub const MIN_H: f32 = 24.;
/// The largest an item may be made.
pub const MAX_SIZE: f32 = 4000.;
/// How far from (0, 0) an item may be put.
pub const MAX_COORD: f32 = 100_000.;
/// How many items in a row placing fills before starting a new row.
const ROW: usize = 5;

/// A board of items, the first drawn bottom-most.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Whiteboard {
    #[serde(default)]
    pub items: Vec<WhiteboardItem>,
    /// The id the next item gets. Never goes back, so an id an agent
    /// remembers never names a different item later.
    #[serde(default)]
    next_id: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WhiteboardItem {
    /// Stable for the item's life; not its place in the list.
    pub id: u64,
    #[serde(default)]
    pub shape: Shape,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    #[serde(default)]
    pub text: String,
    /// A note's paper, a box's tint. Text has none.
    #[serde(default)]
    pub color: Color,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shape {
    /// A sticky note.
    #[default]
    Note,
    /// A box with a border, text in its middle.
    Box,
    /// A box with round corners.
    RoundBox,
    /// Text on the board, without a border.
    Text,
}

impl Shape {
    pub const ALL: [Shape; 4] = [Shape::Note, Shape::Box, Shape::RoundBox, Shape::Text];

    /// The name agents use for it ("round_box").
    pub fn name(self) -> &'static str {
        match self {
            Shape::Note => "note",
            Shape::Box => "box",
            Shape::RoundBox => "round_box",
            Shape::Text => "text",
        }
    }

    pub fn from_name(name: &str) -> Option<Shape> {
        Shape::ALL.into_iter().find(|s| s.name().eq_ignore_ascii_case(name.trim()))
    }

    /// The size a new item of this shape has.
    pub fn default_size(self) -> (f32, f32) {
        match self {
            Shape::Note => (160., 120.),
            Shape::Box | Shape::RoundBox => (180., 90.),
            Shape::Text => (200., 40.),
        }
    }
}

/// The palette items are colored from. Named, so agents can pick one and
/// the app can draw each in its own way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Color {
    #[default]
    Yellow,
    Blue,
    Green,
    Pink,
    Purple,
    Gray,
}

impl Color {
    pub const ALL: [Color; 6] = [Color::Yellow, Color::Blue, Color::Green, Color::Pink, Color::Purple, Color::Gray];

    pub fn name(self) -> &'static str {
        match self {
            Color::Yellow => "yellow",
            Color::Blue => "blue",
            Color::Green => "green",
            Color::Pink => "pink",
            Color::Purple => "purple",
            Color::Gray => "gray",
        }
    }

    pub fn from_name(name: &str) -> Option<Color> {
        let name = name.trim();
        let name = if name.eq_ignore_ascii_case("grey") { "gray" } else { name };
        Color::ALL.into_iter().find(|c| c.name().eq_ignore_ascii_case(name))
    }
}

/// A rectangle on the board: its left, top, width and height.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x <= self.x + self.w && y >= self.y && y <= self.y + self.h
    }

    /// Whether the two overlap, or come closer than `gap`.
    pub fn near(&self, other: &Rect, gap: f32) -> bool {
        self.x < other.x + other.w + gap
            && other.x < self.x + self.w + gap
            && self.y < other.y + other.h + gap
            && other.y < self.y + self.h + gap
    }

    /// The smallest rectangle holding both.
    pub fn union(&self, other: &Rect) -> Rect {
        let (x, y) = (self.x.min(other.x), self.y.min(other.y));
        Rect { x, y, w: (self.x + self.w).max(other.x + other.w) - x, h: (self.y + self.h).max(other.y + other.h) - y }
    }
}

impl WhiteboardItem {
    pub fn rect(&self) -> Rect {
        Rect { x: self.x, y: self.y, w: self.w, h: self.h }
    }
}

impl Whiteboard {
    pub fn item(&self, id: u64) -> Option<&WhiteboardItem> {
        self.items.iter().find(|i| i.id == id)
    }

    pub fn item_mut(&mut self, id: u64) -> Option<&mut WhiteboardItem> {
        self.items.iter_mut().find(|i| i.id == id)
    }

    /// An id no item has had.
    fn take_id(&mut self) -> u64 {
        let id = self.next_id.max(self.items.iter().map(|i| i.id + 1).max().unwrap_or(1)).max(1);
        self.next_id = id + 1;
        id
    }

    /// Add a new item of `shape` at (`x`, `y`), its shape's usual size, on
    /// top of the rest. Returns its id.
    pub fn add(&mut self, shape: Shape, x: f32, y: f32, text: &str, color: Color) -> u64 {
        let (w, h) = shape.default_size();
        self.add_sized(shape, Rect { x, y, w, h }, text, color)
    }

    pub fn add_sized(&mut self, shape: Shape, rect: Rect, text: &str, color: Color) -> u64 {
        let id = self.take_id();
        let (w, h) = clamp_size(rect.w, rect.h);
        let (x, y) = (clamp_coord(rect.x), clamp_coord(rect.y));
        self.items.push(WhiteboardItem { id, shape, x, y, w, h, text: text.to_string(), color });
        id
    }

    pub fn remove(&mut self, id: u64) -> bool {
        let before = self.items.len();
        self.items.retain(|i| i.id != id);
        self.items.len() != before
    }

    /// A copy of item `id`, a little below and to the right of it, on top.
    /// Returns the copy's id.
    pub fn duplicate(&mut self, id: u64) -> Option<u64> {
        let item = self.item(id)?.clone();
        let rect = Rect { x: item.x + GAP, y: item.y + GAP, ..item.rect() };
        Some(self.add_sized(item.shape, rect, &item.text, item.color))
    }

    /// Draw `ids` above (`front`) or below every other item, keeping their
    /// own order. Returns whether that changed the order.
    pub fn restack(&mut self, ids: &[u64], front: bool) -> bool {
        let before: Vec<u64> = self.items.iter().map(|i| i.id).collect();
        let (mut moved, mut rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.items).into_iter().partition(|i| ids.contains(&i.id));
        self.items = match front {
            true => {
                rest.append(&mut moved);
                rest
            }
            false => {
                moved.append(&mut rest);
                moved
            }
        };
        self.items.iter().map(|i| i.id).collect::<Vec<_>>() != before
    }

    /// The topmost item under (`x`, `y`).
    pub fn hit(&self, x: f32, y: f32) -> Option<u64> {
        self.items.iter().rev().find(|i| i.rect().contains(x, y)).map(|i| i.id)
    }

    /// The rectangle holding every item, if there are any.
    pub fn bounds(&self) -> Option<Rect> {
        self.items.iter().map(WhiteboardItem::rect).reduce(|a, b| a.union(&b))
    }

    fn free(&self, rect: &Rect) -> bool {
        !self.items.iter().any(|i| i.rect().near(rect, GAP / 2.))
    }

    /// Where to put a new `w` × `h` item so it touches no other: beside
    /// item `near` if given (right of it, else below, left, above, then
    /// further right), or else in the first free place of rows of items
    /// from (0, 0).
    pub fn place(&self, w: f32, h: f32, near: Option<u64>) -> (f32, f32) {
        if let Some(n) = near.and_then(|id| self.item(id)) {
            let r = n.rect();
            let tries = [
                (r.x + r.w + GAP, r.y),
                (r.x, r.y + r.h + GAP),
                (r.x - w - GAP, r.y),
                (r.x, r.y - h - GAP),
            ];
            if let Some(&(x, y)) = tries.iter().find(|&&(x, y)| self.free(&Rect { x, y, w, h })) {
                return (x, y);
            }
            for step in 2..200 {
                let x = r.x + (r.w + GAP) * step as f32;
                if self.free(&Rect { x, y: r.y, w, h }) {
                    return (x, r.y);
                }
            }
        }
        let (cw, ch) = (Shape::Note.default_size().0.max(w) + GAP, Shape::Note.default_size().1.max(h) + GAP);
        for slot in 0..10_000 {
            let (x, y) = ((slot % ROW) as f32 * cw, (slot / ROW) as f32 * ch);
            if self.free(&Rect { x, y, w, h }) {
                return (x, y);
            }
        }
        // Somewhere below everything, then.
        let below = self.bounds().map_or(0., |b| b.y + b.h + GAP);
        (0., below)
    }
}

/// A size within what an item may be.
pub fn clamp_size(w: f32, h: f32) -> (f32, f32) {
    let fix = |v: f32, min: f32| if v.is_finite() { v.clamp(min, MAX_SIZE) } else { min };
    (fix(w, MIN_W), fix(h, MIN_H))
}

/// A coordinate within where items may be.
pub fn clamp_coord(v: f32) -> f32 {
    if v.is_finite() { v.clamp(-MAX_COORD, MAX_COORD) } else { 0. }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_get_ids_that_are_never_reused() {
        let mut b = Whiteboard::default();
        let a = b.add(Shape::Note, 0., 0., "a", Color::Yellow);
        let c = b.add(Shape::Box, 10., 10., "c", Color::Blue);
        assert_eq!((a, c), (1, 2));
        assert!(b.remove(c));
        assert!(!b.remove(c));
        assert_eq!(b.add(Shape::Text, 0., 0., "", Color::Gray), 3, "a removed id isn't given out again");
        // Even read back from a file.
        let back: Whiteboard = serde_json::from_value(serde_json::to_value(&b).unwrap()).unwrap();
        let mut back = back;
        assert_eq!(back.add(Shape::Note, 0., 0., "", Color::Pink), 4);
    }

    #[test]
    fn the_topmost_item_is_hit_and_restacking_changes_which() {
        let mut b = Whiteboard::default();
        let under = b.add(Shape::Note, 0., 0., "", Color::Yellow);
        let over = b.add(Shape::Note, 50., 50., "", Color::Yellow);
        assert_eq!(b.hit(60., 60.), Some(over));
        assert_eq!(b.hit(10., 10.), Some(under));
        assert_eq!(b.hit(-5., 0.), None);
        assert!(b.restack(&[under], true));
        assert_eq!(b.hit(60., 60.), Some(under));
        assert!(!b.restack(&[under], true), "already on top");
        assert!(b.restack(&[under], false));
        assert_eq!(b.items[0].id, under);
    }

    #[test]
    fn placing_never_lands_on_another_item() {
        let mut b = Whiteboard::default();
        for _ in 0..12 {
            let (w, h) = Shape::Note.default_size();
            let (x, y) = b.place(w, h, None);
            let new = Rect { x, y, w, h };
            assert!(b.items.iter().all(|i| !i.rect().near(&new, 1.)), "{new:?}");
            b.add(Shape::Note, x, y, "", Color::Yellow);
        }
        assert_eq!(b.items[1].rect().x, 160. + GAP, "rows, left to right");
        assert_eq!(b.items[5].rect().y, 120. + GAP, "then the next row");

        // Beside a given item: right of it, and when that's taken, below.
        let mut b = Whiteboard::default();
        let a = b.add(Shape::Note, 0., 0., "", Color::Yellow);
        assert_eq!(b.place(160., 120., Some(a)), (160. + GAP, 0.));
        b.add(Shape::Note, 160. + GAP, 0., "", Color::Yellow);
        assert_eq!(b.place(160., 120., Some(a)), (0., 120. + GAP));
    }

    #[test]
    fn sizes_and_places_stay_within_bounds() {
        let mut b = Whiteboard::default();
        let id = b.add_sized(Shape::Box, Rect { x: f32::NAN, y: 1e9, w: 1., h: 1e9 }, "", Color::Blue);
        let i = b.item(id).unwrap();
        assert_eq!((i.x, i.y, i.w, i.h), (0., MAX_COORD, MIN_W, MAX_SIZE));
        assert_eq!(Shape::from_name("Round_Box"), Some(Shape::RoundBox));
        assert_eq!(Color::from_name("grey"), Some(Color::Gray));
        assert_eq!(Color::from_name("teal"), None);
        let dup = b.duplicate(id).unwrap();
        assert_eq!(b.item(dup).unwrap().x, GAP);
        assert!(b.duplicate(99).is_none());
    }
}
