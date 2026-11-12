//! Turn the markdown block tree into GPUI elements.

use gpui::{
    AnyElement, App, FontStyle, FontWeight, InteractiveText, SharedString, StrikethroughStyle,
    StyledText, TextRun, TextStyle, UnderlineStyle, div, prelude::*, px,
};

use pots_agent::markdown::{Align, Block, Inline, ListItem, table_columns};
use crate::selection::{SelectableText, SharedSelection};
use crate::theme;

/// Rendering context for one message. Fragment indices are handed out in the
/// same order as `markdown::fragments` walks the blocks, so selection keys
/// line up with the text that gets copied.
pub struct Ctx {
    pub msg: usize,
    pub selection: SharedSelection,
    next: usize,
}

impl Ctx {
    pub fn new(msg: usize, selection: SharedSelection) -> Self {
        Self {
            msg,
            selection,
            next: 0,
        }
    }

    fn alloc(&mut self, count: usize) -> usize {
        let ix = self.next;
        self.next += count;
        ix
    }

    fn text(&self, frag: usize, inline: &Inline, style: &TextStyle) -> AnyElement {
        let styled = StyledText::new(inline.text.clone()).with_runs(runs_for(inline, style));
        SelectableText::new(
            (self.msg, frag),
            inline.text.clone().into(),
            styled,
            inline.links.clone(),
            self.selection.clone(),
        )
        .into_any_element()
    }
}

fn base_style() -> TextStyle {
    TextStyle {
        color: theme::text(),
        font_family: ".SystemUIFont".into(),
        ..Default::default()
    }
}

fn runs_for(inline: &Inline, base: &TextStyle) -> Vec<TextRun> {
    let mut runs = Vec::new();
    let mut ix = 0;
    for (range, st) in &inline.spans {
        if ix < range.start {
            runs.push(base.to_run(range.start - ix));
        }
        let mut s = base.clone();
        if st.bold {
            s.font_weight = FontWeight::BOLD;
        }
        if st.italic {
            s.font_style = FontStyle::Italic;
        }
        if st.code {
            s.font_family = theme::MONO.into();
            s.color = theme::code_fg();
            s.background_color = Some(theme::code_bg());
        }
        if st.mention {
            s.color = theme::mention_fg();
            s.background_color = Some(theme::mention_bg());
            s.font_weight = FontWeight::MEDIUM;
        }
        if st.link {
            s.color = theme::link();
            s.underline = Some(UnderlineStyle {
                thickness: px(1.),
                color: Some(theme::link()),
                wavy: false,
            });
        }
        if st.strike {
            s.strikethrough = Some(StrikethroughStyle {
                thickness: px(1.),
                color: Some(s.color),
            });
        }
        runs.push(s.to_run(range.len()));
        ix = range.end;
    }
    if ix < inline.text.len() {
        runs.push(base.to_run(inline.text.len() - ix));
    }
    runs
}

/// Non-selectable inline text (used outside message bodies, e.g. the channel
/// topic); links are clickable.
pub fn inline(inline: &Inline, id: SharedString, base: &TextStyle) -> AnyElement {
    let text = StyledText::new(inline.text.clone()).with_runs(runs_for(inline, base));
    if inline.links.is_empty() {
        return text.into_any_element();
    }
    let ranges = inline.links.iter().map(|(r, _)| r.clone()).collect();
    let urls: Vec<String> = inline.links.iter().map(|(_, u)| u.clone()).collect();
    InteractiveText::new(id, text)
        .on_click(ranges, move |ix, _, cx: &mut App| cx.open_url(&urls[ix]))
        .into_any_element()
}

pub fn blocks(blocks: &[Block], ctx: &mut Ctx) -> Vec<AnyElement> {
    blocks.iter().map(|b| block(b, 0, ctx)).collect()
}

fn block(block: &Block, depth: usize, ctx: &mut Ctx) -> AnyElement {
    let base = base_style();
    match block {
        Block::Paragraph(p) => {
            let frag = ctx.alloc(1);
            div().child(ctx.text(frag, p, &base)).into_any_element()
        }
        Block::Heading(level, h) => {
            let (size, top) = match level {
                1 => (px(22.), px(6.)),
                2 => (px(19.), px(4.)),
                3 => (px(17.), px(2.)),
                _ => (px(15.), px(0.)),
            };
            let style = TextStyle {
                font_weight: FontWeight::BOLD,
                ..base
            };
            let frag = ctx.alloc(1);
            div()
                .pt(top)
                .text_size(size)
                .line_height(size * 1.35)
                .child(ctx.text(frag, h, &style))
                .into_any_element()
        }
        Block::Code { lang, code } => {
            let style = TextStyle {
                font_family: theme::MONO.into(),
                ..base
            };
            let frag = ctx.alloc(1);
            let content = Inline {
                text: code.clone(),
                ..Default::default()
            };
            div()
                .relative()
                .w_full()
                .rounded_md()
                .border_1()
                .border_color(theme::code_block_border())
                .bg(theme::code_block_bg())
                .px_3()
                .py_2()
                .text_size(px(13.))
                .line_height(px(19.))
                .child(ctx.text(frag, &content, &style))
                .when_some(lang.clone(), |d, lang| {
                    d.child(
                        div()
                            .absolute()
                            .top(px(4.))
                            .right(px(8.))
                            .text_size(px(11.))
                            .text_color(theme::faint())
                            .child(lang),
                    )
                })
                .into_any_element()
        }
        Block::Quote(children) => div()
            .flex()
            .flex_col()
            .gap_1()
            .border_l_4()
            .border_color(theme::quote_bar())
            .pl_3()
            .children(children.iter().map(|b| self::block(b, depth, ctx)).collect::<Vec<_>>())
            .into_any_element(),
        Block::List { start, items } => list(*start, items, depth, ctx),
        Block::Table {
            aligns,
            header,
            rows,
        } => table(aligns, header, rows, ctx),
        Block::Rule => div()
            .my_1()
            .h(px(1.))
            .w_full()
            .bg(theme::border())
            .into_any_element(),
    }
}

fn list(start: Option<u64>, items: &[ListItem], depth: usize, ctx: &mut Ctx) -> AnyElement {
    let mut rows = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let marker: AnyElement = match (item.checked, start) {
            (Some(done), _) => checkbox(done).into_any_element(),
            (None, Some(n)) => div()
                .text_color(theme::muted())
                .child(format!("{}.", n + i as u64))
                .into_any_element(),
            (None, None) => div()
                .text_color(theme::muted())
                .child(["•", "◦", "▪"][depth % 3])
                .into_any_element(),
        };
        let content: Vec<_> = item.blocks.iter().map(|b| block(b, depth + 1, ctx)).collect();
        rows.push(
            div()
                .flex()
                .items_start()
                .child(div().w(px(22.)).flex_none().flex().justify_end().pr(px(6.)).child(marker))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .when(item.checked == Some(true), |d| d.text_color(theme::muted()))
                        .children(content),
                ),
        );
    }
    div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .children(rows)
        .into_any_element()
}

fn checkbox(done: bool) -> impl IntoElement {
    div()
        .mt(px(4.))
        .size(px(14.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(3.))
        .border_1()
        .text_size(px(10.))
        .when(done, |d| {
            d.bg(theme::checkbox_on())
                .border_color(theme::checkbox_on())
                .text_color(gpui::white())
                .child("✓")
        })
        .when(!done, |d| d.border_color(theme::faint()))
}

/// Tables are laid out column-major: each column is a flex column that sizes
/// to its widest cell, and cells don't wrap, so rows line up across columns.
/// Wide tables scroll horizontally. Fragment indices are row-major (header
/// first) so a selection reads in table order.
fn table(aligns: &[Align], header: &[Inline], rows: &[Vec<Inline>], ctx: &mut Ctx) -> AnyElement {
    let ncols = table_columns(header, rows);
    let nrows = rows.len();
    let base_frag = ctx.alloc((nrows + 1) * ncols);
    let id = format!("table-{}-{}", ctx.msg, base_frag);
    let empty = Inline::default();
    let header_style = TextStyle {
        font_weight: FontWeight::BOLD,
        ..base_style()
    };
    let base = base_style();

    let cell = |content: &Inline, style: &TextStyle, align: Align, frag: usize| {
        div()
            .h(px(34.))
            .px_3()
            .flex()
            .items_center()
            .whitespace_nowrap()
            .map(|d| match align {
                Align::Left => d.justify_start(),
                Align::Center => d.justify_center(),
                Align::Right => d.justify_end(),
            })
            .child(ctx.text(frag, content, style))
    };

    let columns: Vec<_> = (0..ncols)
        .map(|c| {
            let align = aligns.get(c).copied().unwrap_or(Align::Left);
            let first = c == 0;
            let last = c + 1 == ncols;
            div()
                .flex()
                .flex_col()
                .when(!last, |d| d.border_r_1().border_color(theme::border()))
                .child(
                    cell(header.get(c).unwrap_or(&empty), &header_style, align, base_frag + c)
                        .bg(theme::table_header())
                        .when(first, |d| d.rounded_tl_md())
                        .when(last, |d| d.rounded_tr_md())
                        .when(nrows > 0, |d| d.border_b_1().border_color(theme::border())),
                )
                .children(rows.iter().enumerate().map(|(r, row)| {
                    let last_row = r + 1 == nrows;
                    let frag = base_frag + (r + 1) * ncols + c;
                    cell(row.get(c).unwrap_or(&empty), &base, align, frag)
                        .when(r % 2 == 1, |d| d.bg(theme::table_stripe()))
                        .when(!last_row, |d| d.border_b_1().border_color(theme::border()))
                        .when(last_row && first, |d| d.rounded_bl_md())
                        .when(last_row && last, |d| d.rounded_br_md())
                }))
        })
        .collect();

    div()
        .id(SharedString::from(id))
        .flex()
        .max_w_full()
        .overflow_x_scroll()
        .py_1()
        .child(
            div()
                .flex()
                .flex_none()
                .rounded_md()
                .border_1()
                .border_color(theme::border())
                .children(columns),
        )
        .into_any_element()
}
