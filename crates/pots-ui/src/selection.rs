//! Text selection across rendered messages.
//!
//! Every selectable run of message text (a paragraph, heading, code block or
//! table cell) is a *fragment* identified by `(message index, fragment index)`.
//! Fragments are rendered with [`SelectableText`], which registers its on-screen
//! geometry in the shared [`Selection`] each frame and paints the highlight for
//! whatever part of it is selected. Mouse handling lives on the message list
//! container and uses the registered geometry for hit-testing, so a drag can
//! span several messages. Copying rebuilds the text from the message data
//! (see `markdown::fragments`), so off-screen fragments copy correctly too.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    App, Bounds, CursorStyle, Element, ElementId, GlobalElementId, Hitbox, HitboxBehavior,
    InspectorElementId, IntoElement, LayoutId, Pixels, Point, SharedString, StyledText, TextLayout,
    Window, fill, point, px,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::theme;

pub type Key = (usize, usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub msg: usize,
    pub frag: usize,
    pub offset: usize,
}

impl Pos {
    fn key(&self) -> Key {
        (self.msg, self.frag)
    }
}

struct Geom {
    text: SharedString,
    layout: TextLayout,
    bounds: Bounds<Pixels>,
    /// `bounds` clipped to the visible area of the list.
    visible: Bounds<Pixels>,
    links: Rc<[(Range<usize>, String)]>,
}

#[derive(Default)]
pub struct Selection {
    pub anchor: Option<Pos>,
    pub head: Option<Pos>,
    pub dragging: bool,
    geoms: BTreeMap<Key, Geom>,
}

pub type SharedSelection = Rc<RefCell<Selection>>;

impl Selection {
    /// The selected range in document order, if non-empty.
    pub fn range(&self) -> Option<(Pos, Pos)> {
        let (a, h) = (self.anchor?, self.head?);
        (a != h).then(|| (a.min(h), a.max(h)))
    }

    pub fn clear(&mut self) {
        self.anchor = None;
        self.head = None;
        self.dragging = false;
    }

    /// Forget last frame's geometry; fragments re-register as they prepaint.
    pub fn begin_frame(&mut self) {
        self.geoms.clear();
    }

    /// Byte range of fragment `key` (of length `len`) that is selected.
    fn range_in(&self, key: Key, len: usize) -> Option<Range<usize>> {
        let (s, e) = self.range()?;
        if key < s.key() || key > e.key() {
            return None;
        }
        let start = if key == s.key() { s.offset.min(len) } else { 0 };
        let end = if key == e.key() { e.offset.min(len) } else { len };
        (start < end).then_some(start..end)
    }

    /// Text position nearest to `p`, among fragments on screen.
    pub fn hit(&self, p: Point<Pixels>) -> Option<Pos> {
        let pos = |key: &Key, g: &Geom| Pos {
            msg: key.0,
            frag: key.1,
            offset: closest_index(g, p),
        };
        if let Some((key, g)) = self.geoms.iter().find(|(_, g)| g.visible.contains(&p)) {
            return Some(pos(key, g));
        }
        // Same row (e.g. right of a short line, or between table cells): nearest by x.
        let same_row = self
            .geoms
            .iter()
            .filter(|(_, g)| g.visible.top() <= p.y && p.y < g.visible.bottom())
            .min_by_key(|(_, g)| {
                let dx = if p.x < g.visible.left() {
                    g.visible.left() - p.x
                } else {
                    p.x - g.visible.right()
                };
                (f32::from(dx.max(px(0.))) * 100.) as i64
            });
        if let Some((key, g)) = same_row {
            return Some(pos(key, g));
        }
        // Between rows: end of the nearest fragment above, else start of the first.
        let above = self
            .geoms
            .iter()
            .filter(|(_, g)| g.visible.bottom() <= p.y)
            .max_by_key(|(key, g)| ((f32::from(g.visible.bottom()) * 100.) as i64, **key));
        if let Some((key, g)) = above {
            return Some(Pos {
                msg: key.0,
                frag: key.1,
                offset: g.text.len(),
            });
        }
        self.geoms.iter().next().map(|(key, _)| Pos {
            msg: key.0,
            frag: key.1,
            offset: 0,
        })
    }

    /// Range of the word (or, with `whole`, the entire fragment) at `pos`.
    pub fn expand(&self, pos: Pos, whole: bool) -> Option<(Pos, Pos)> {
        let g = self.geoms.get(&pos.key())?;
        let range = if whole {
            0..g.text.len()
        } else {
            g.text
                .split_word_bound_indices()
                .map(|(i, w)| i..i + w.len())
                .find(|r| r.contains(&pos.offset))
                .or_else(|| {
                    g.text
                        .split_word_bound_indices()
                        .last()
                        .map(|(i, w)| i..i + w.len())
                })?
        };
        Some((
            Pos {
                offset: range.start,
                ..pos
            },
            Pos {
                offset: range.end,
                ..pos
            },
        ))
    }

    /// The link under `p`, if any.
    pub fn link_at(&self, p: Point<Pixels>) -> Option<String> {
        self.geoms
            .values()
            .filter(|g| g.visible.contains(&p))
            .find_map(|g| link_at(g, p))
    }
}

fn link_at(g: &Geom, p: Point<Pixels>) -> Option<String> {
    let ix = g.layout.index_for_position(p).ok()?;
    g.links
        .iter()
        .find(|(r, _)| r.contains(&ix))
        .map(|(_, url)| url.clone())
}

/// Character boundary closest to `p` within a fragment (clamped to its bounds).
fn closest_index(g: &Geom, p: Point<Pixels>) -> usize {
    let b = g.bounds;
    if p.y < b.top() {
        return 0;
    }
    if p.y >= b.bottom() {
        return g.text.len();
    }
    let clamped = point(p.x.clamp(b.left(), b.right()), p.y);
    let ix = match g.layout.index_for_position(clamped) {
        Ok(ix) | Err(ix) => ix.min(g.text.len()),
    };
    // `index_for_position` returns the character under the pointer; snap to
    // whichever of its edges is nearer.
    let next = g.text[ix..]
        .chars()
        .next()
        .map_or(ix, |c| ix + c.len_utf8());
    match (g.layout.position_for_index(ix), g.layout.position_for_index(next)) {
        (Some(a), Some(b)) if next > ix && a.y == b.y && (p.x - a.x) > (b.x - p.x) => next,
        _ => ix,
    }
}

// ---------------------------------------------------------------------------

/// A `StyledText` that participates in message selection.
pub struct SelectableText {
    key: Key,
    text: SharedString,
    styled: StyledText,
    links: Rc<[(Range<usize>, String)]>,
    selection: SharedSelection,
}

impl SelectableText {
    pub fn new(
        key: Key,
        text: SharedString,
        styled: StyledText,
        links: Vec<(Range<usize>, String)>,
        selection: SharedSelection,
    ) -> Self {
        Self {
            key,
            text,
            styled,
            links: links.into(),
            selection,
        }
    }
}

impl IntoElement for SelectableText {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for SelectableText {
    type RequestLayoutState = ();
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        self.styled.request_layout(id, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Hitbox {
        self.styled
            .prepaint(id, inspector_id, bounds, state, window, cx);
        let visible = bounds.intersect(&window.content_mask().bounds);
        self.selection.borrow_mut().geoms.insert(
            self.key,
            Geom {
                text: self.text.clone(),
                layout: self.styled.layout().clone(),
                bounds,
                visible,
                links: self.links.clone(),
            },
        );
        window.insert_hitbox(bounds, HitboxBehavior::Normal)
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut (),
        hitbox: &mut Hitbox,
        window: &mut Window,
        cx: &mut App,
    ) {
        let layout = self.styled.layout().clone();
        let selected = self.selection.borrow().range_in(self.key, self.text.len());
        if let Some(range) = selected
            && let (Some(a), Some(b)) = (
                layout.position_for_index(range.start),
                layout.position_for_index(range.end),
            )
        {
            let lh = layout.line_height();
            let color = theme::selection();
            let rect = |x0: Pixels, y0: Pixels, x1: Pixels, y1: Pixels| {
                fill(Bounds::from_corners(point(x0, y0), point(x1, y1)), color)
            };
            if a.y == b.y {
                window.paint_quad(rect(a.x, a.y, b.x, a.y + lh));
            } else {
                window.paint_quad(rect(a.x, a.y, bounds.right(), a.y + lh));
                if b.y > a.y + lh {
                    window.paint_quad(rect(bounds.left(), a.y + lh, bounds.right(), b.y));
                }
                window.paint_quad(rect(bounds.left(), b.y, b.x, b.y + lh));
            }
        }

        self.styled
            .paint(id, inspector_id, bounds, state, &mut (), window, cx);

        if hitbox.is_hovered(window) {
            let over_link = !self.links.is_empty()
                && layout
                    .index_for_position(window.mouse_position())
                    .ok()
                    .is_some_and(|ix| self.links.iter().any(|(r, _)| r.contains(&ix)));
            let style = if over_link {
                CursorStyle::PointingHand
            } else {
                CursorStyle::IBeam
            };
            window.set_cursor_style(style, hitbox);
        }
    }
}
