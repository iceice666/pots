//! Markdown -> render tree.
//!
//! Messages are parsed once (with pulldown-cmark) into a small block tree that
//! the UI layer turns into GPUI elements. Inline formatting is flattened into a
//! single string plus non-overlapping styled spans, which maps directly onto
//! GPUI `TextRun`s.

use std::ops::Range;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct InlineStyle {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
    pub link: bool,
    pub mention: bool,
}

/// A run of inline text with formatting spans and clickable link ranges.
#[derive(Clone, Default, Debug)]
pub struct Inline {
    pub text: String,
    /// Sorted, non-overlapping styled ranges (byte offsets into `text`).
    pub spans: Vec<(Range<usize>, InlineStyle)>,
    /// Clickable ranges and their destinations.
    pub links: Vec<(Range<usize>, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Clone, Debug)]
pub enum Block {
    Paragraph(Inline),
    Heading(u8, Inline),
    Code {
        lang: Option<String>,
        code: String,
    },
    Quote(Vec<Block>),
    List {
        start: Option<u64>,
        items: Vec<ListItem>,
    },
    Table {
        aligns: Vec<Align>,
        header: Vec<Inline>,
        rows: Vec<Vec<Inline>>,
    },
    Rule,
}

#[derive(Clone, Debug)]
pub struct ListItem {
    pub checked: Option<bool>,
    pub blocks: Vec<Block>,
}

pub fn parse(src: &str) -> Vec<Block> {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut b = Builder {
        stack: vec![Frame::Blocks(Vec::new())],
        inline: None,
    };
    for event in Parser::new_ext(src, opts) {
        b.event(event);
    }
    b.flush_implicit();
    match b.stack.into_iter().next() {
        Some(Frame::Blocks(blocks)) => blocks,
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------

#[derive(Default)]
struct InlineBuilder {
    out: Inline,
    bold: u32,
    italic: u32,
    strike: u32,
    link: Option<(usize, String)>,
}

impl InlineBuilder {
    fn style(&self) -> InlineStyle {
        InlineStyle {
            bold: self.bold > 0,
            italic: self.italic > 0,
            strike: self.strike > 0,
            link: self.link.is_some(),
            ..Default::default()
        }
    }

    fn push_code(&mut self, text: &str) {
        let style = InlineStyle {
            code: true,
            ..self.style()
        };
        self.push_styled(text, style);
    }

    /// Push plain text, expanding `:emoji:` shortcodes and detecting
    /// `@mentions` and bare URLs.
    fn push_text(&mut self, text: &str) {
        let text = expand_emoji(text);
        let base = self.style();
        let mut rest = text.as_str();
        while let Some((range, kind)) = next_token(rest) {
            self.push_styled(&rest[..range.start], base);
            let token = &rest[range.clone()];
            match kind {
                Token::Mention => self.push_styled(
                    token,
                    InlineStyle {
                        mention: true,
                        ..base
                    },
                ),
                Token::Url if self.link.is_none() => {
                    let start = self.out.text.len();
                    self.push_styled(token, InlineStyle { link: true, ..base });
                    let end = self.out.text.len();
                    self.out.links.push((start..end, token.to_string()));
                }
                Token::Url => self.push_styled(token, base),
            }
            rest = &rest[range.end..];
        }
        self.push_styled(rest, base);
    }

    fn push_styled(&mut self, text: &str, style: InlineStyle) {
        if text.is_empty() {
            return;
        }
        let start = self.out.text.len();
        self.out.text.push_str(text);
        let end = self.out.text.len();
        if style == InlineStyle::default() {
            return;
        }
        match self.out.spans.last_mut() {
            Some((range, s)) if range.end == start && *s == style => range.end = end,
            _ => self.out.spans.push((start..end, style)),
        }
    }

    fn finish(self) -> Inline {
        self.out
    }
}

enum Token {
    Mention,
    Url,
}

/// Find the next `@mention` or `http(s)://` URL in `s`.
fn next_token(s: &str) -> Option<(Range<usize>, Token)> {
    // A token can't start in the middle of a Latin word (`me@x.com`), but
    // may follow CJK text or punctuation directly (`完成了。@Reviewer`).
    let is_boundary = |i: usize| {
        !s[..i]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || "_.-+/:@".contains(c))
    };
    let mut i = 0;
    while i < s.len() {
        let rest = &s[i..];
        if rest.starts_with('@') && is_boundary(i) {
            // An ASCII name ends at the first non-ASCII character, so CJK text
            // right after it (`@Reviewer請看`) isn't part of the name.
            let ascii = rest[1..].starts_with(|c: char| c.is_ascii());
            let name_len = rest[1..]
                .char_indices()
                .find(|&(_, c)| {
                    let word = if ascii { c.is_ascii_alphanumeric() } else { c.is_alphanumeric() };
                    !(word || c == '_' || c == '-' || c == '.')
                })
                .map(|(j, _)| j)
                .unwrap_or(rest.len() - 1);
            let name = rest[1..1 + name_len].trim_end_matches('.');
            if !name.is_empty() {
                return Some((i..i + 1 + name.len(), Token::Mention));
            }
        }
        if (rest.starts_with("https://") || rest.starts_with("http://")) && is_boundary(i) {
            let len = rest
                .find(|c: char| c.is_whitespace() || c == '<' || c == '>')
                .unwrap_or(rest.len());
            let url = rest[..len].trim_end_matches(['.', ',', ')', ';', ':', '!', '?']);
            return Some((i..i + url.len(), Token::Url));
        }
        i += rest.chars().next().map_or(1, char::len_utf8);
    }
    None
}

const EMOJI: &[(&str, &str)] = &[
    (":tada:", "🎉"),
    (":rocket:", "🚀"),
    (":+1:", "👍"),
    (":thumbsup:", "👍"),
    (":fire:", "🔥"),
    (":eyes:", "👀"),
    (":white_check_mark:", "✅"),
    (":warning:", "⚠️"),
    (":smile:", "😄"),
    (":joy:", "😂"),
    (":heart:", "❤️"),
    (":wave:", "👋"),
    (":coffee:", "☕"),
    (":bug:", "🐛"),
    (":sparkles:", "✨"),
    (":100:", "💯"),
    (":pray:", "🙏"),
    (":thinking_face:", "🤔"),
    (":crab:", "🦀"),
];

fn expand_emoji(text: &str) -> String {
    if !text.contains(':') {
        return text.to_string();
    }
    let mut out = text.to_string();
    for (code, emoji) in EMOJI {
        if out.contains(code) {
            out = out.replace(code, emoji);
        }
    }
    out
}

/// Look up the emoji for a shortcode (used by reactions).
pub fn emoji(code: &str) -> &str {
    EMOJI
        .iter()
        .find(|(c, _)| c.trim_matches(':') == code.trim_matches(':'))
        .map_or(code, |(_, e)| e)
}

// ---------------------------------------------------------------------------

enum Frame {
    Blocks(Vec<Block>),
    Quote(Vec<Block>),
    List {
        start: Option<u64>,
        items: Vec<ListItem>,
    },
    Item {
        checked: Option<bool>,
        blocks: Vec<Block>,
    },
    Table {
        aligns: Vec<Align>,
        header: Vec<Inline>,
        rows: Vec<Vec<Inline>>,
        row: Vec<Inline>,
    },
    Code {
        lang: Option<String>,
        code: String,
    },
}

#[derive(PartialEq)]
enum InlineKind {
    /// Text that appeared without an enclosing paragraph (tight list items).
    Implicit,
    Explicit,
}

struct Builder {
    stack: Vec<Frame>,
    inline: Option<(InlineKind, InlineBuilder)>,
}

impl Builder {
    fn push_block(&mut self, block: Block) {
        match self.stack.last_mut() {
            Some(Frame::Blocks(b) | Frame::Quote(b) | Frame::Item { blocks: b, .. }) => b.push(block),
            _ => {}
        }
    }

    fn flush_implicit(&mut self) {
        if matches!(self.inline, Some((InlineKind::Implicit, _))) {
            let (_, b) = self.inline.take().unwrap();
            self.push_block(Block::Paragraph(b.finish()));
        }
    }

    fn inline_mut(&mut self) -> &mut InlineBuilder {
        &mut self
            .inline
            .get_or_insert_with(|| (InlineKind::Implicit, InlineBuilder::default()))
            .1
    }

    fn take_inline(&mut self) -> Inline {
        self.inline.take().map(|(_, b)| b.finish()).unwrap_or_default()
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => {
                if let Some(Frame::Code { code, .. }) = self.stack.last_mut() {
                    code.push_str(&t);
                } else {
                    self.inline_mut().push_text(&t);
                }
            }
            Event::Code(t) => self.inline_mut().push_code(&t),
            Event::SoftBreak | Event::HardBreak => {
                // Slack keeps single newlines as line breaks.
                self.inline_mut().push_styled("\n", InlineStyle::default())
            }
            Event::Html(t) | Event::InlineHtml(t) => self.inline_mut().push_text(&t),
            Event::Rule => {
                self.flush_implicit();
                self.push_block(Block::Rule);
            }
            Event::TaskListMarker(done) => {
                if let Some(Frame::Item { checked, .. }) = self.stack.last_mut() {
                    *checked = Some(done);
                }
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph | Tag::Heading { .. } => {
                self.flush_implicit();
                self.inline = Some((InlineKind::Explicit, InlineBuilder::default()));
            }
            Tag::BlockQuote(_) => {
                self.flush_implicit();
                self.stack.push(Frame::Quote(Vec::new()));
            }
            Tag::CodeBlock(kind) => {
                self.flush_implicit();
                let lang = match kind {
                    CodeBlockKind::Fenced(l) if !l.trim().is_empty() => Some(l.trim().to_string()),
                    _ => None,
                };
                self.stack.push(Frame::Code {
                    lang,
                    code: String::new(),
                });
            }
            Tag::List(start) => {
                self.flush_implicit();
                self.stack.push(Frame::List {
                    start,
                    items: Vec::new(),
                });
            }
            Tag::Item => self.stack.push(Frame::Item {
                checked: None,
                blocks: Vec::new(),
            }),
            Tag::Table(aligns) => {
                self.flush_implicit();
                self.stack.push(Frame::Table {
                    aligns: aligns
                        .into_iter()
                        .map(|a| match a {
                            Alignment::Center => Align::Center,
                            Alignment::Right => Align::Right,
                            _ => Align::Left,
                        })
                        .collect(),
                    header: Vec::new(),
                    rows: Vec::new(),
                    row: Vec::new(),
                });
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(Frame::Table { row, .. }) = self.stack.last_mut() {
                    row.clear();
                }
            }
            Tag::TableCell => {
                self.inline = Some((InlineKind::Explicit, InlineBuilder::default()));
            }
            Tag::Emphasis => self.inline_mut().italic += 1,
            Tag::Strong => self.inline_mut().bold += 1,
            Tag::Strikethrough => self.inline_mut().strike += 1,
            Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                let b = self.inline_mut();
                b.link = Some((b.out.text.len(), dest_url.to_string()));
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                let inline = self.take_inline();
                self.push_block(Block::Paragraph(inline));
            }
            TagEnd::Heading(level) => {
                let inline = self.take_inline();
                self.push_block(Block::Heading(level as u8, inline));
            }
            TagEnd::BlockQuote(_) => {
                self.flush_implicit();
                if let Some(Frame::Quote(blocks)) = self.stack.pop() {
                    self.push_block(Block::Quote(blocks));
                }
            }
            TagEnd::CodeBlock => {
                if let Some(Frame::Code { lang, mut code }) = self.stack.pop() {
                    while code.ends_with('\n') {
                        code.pop();
                    }
                    self.push_block(Block::Code { lang, code });
                }
            }
            TagEnd::List(_) => {
                self.flush_implicit();
                if let Some(Frame::List { start, items }) = self.stack.pop() {
                    self.push_block(Block::List { start, items });
                }
            }
            TagEnd::Item => {
                self.flush_implicit();
                if let Some(Frame::Item { checked, blocks }) = self.stack.pop()
                    && let Some(Frame::List { items, .. }) = self.stack.last_mut()
                {
                    items.push(ListItem { checked, blocks });
                }
            }
            TagEnd::TableCell => {
                let inline = self.take_inline();
                if let Some(Frame::Table { row, .. }) = self.stack.last_mut() {
                    row.push(inline);
                }
            }
            TagEnd::TableHead => {
                if let Some(Frame::Table { row, header, .. }) = self.stack.last_mut() {
                    *header = std::mem::take(row);
                }
            }
            TagEnd::TableRow => {
                if let Some(Frame::Table { row, rows, .. }) = self.stack.last_mut() {
                    rows.push(std::mem::take(row));
                }
            }
            TagEnd::Table => {
                if let Some(Frame::Table {
                    aligns,
                    header,
                    rows,
                    ..
                }) = self.stack.pop()
                {
                    self.push_block(Block::Table {
                        aligns,
                        header,
                        rows,
                    });
                }
            }
            TagEnd::Emphasis => {
                let b = self.inline_mut();
                b.italic = b.italic.saturating_sub(1);
            }
            TagEnd::Strong => {
                let b = self.inline_mut();
                b.bold = b.bold.saturating_sub(1);
            }
            TagEnd::Strikethrough => {
                let b = self.inline_mut();
                b.strike = b.strike.saturating_sub(1);
            }
            TagEnd::Link | TagEnd::Image => {
                let b = self.inline_mut();
                if let Some((start, url)) = b.link.take() {
                    let end = b.out.text.len();
                    if end > start {
                        b.out.links.push((start..end, url));
                    }
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------

/// How a fragment is joined to the previous one when copied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sep {
    Line,
    Tab,
}

/// A selectable run of text, in document order. The renderer assigns
/// fragment indices in exactly this order (see `render_md`).
#[derive(Clone, Debug)]
pub struct Fragment {
    pub text: String,
    /// Prepended when the fragment is copied from its start (list markers,
    /// quote markers, indentation).
    pub prefix: String,
    pub sep: Sep,
}

pub fn fragments(blocks: &[Block]) -> Vec<Fragment> {
    let mut out = Vec::new();
    collect_fragments(blocks, "", &mut out);
    out
}

fn collect_fragments(blocks: &[Block], indent: &str, out: &mut Vec<Fragment>) {
    let push = |out: &mut Vec<Fragment>, text: &str, prefix: &str, sep: Sep| {
        out.push(Fragment {
            text: text.to_string(),
            prefix: prefix.to_string(),
            sep,
        })
    };
    for block in blocks {
        match block {
            Block::Paragraph(i) | Block::Heading(_, i) => push(out, &i.text, indent, Sep::Line),
            Block::Code { code, .. } => push(out, code, indent, Sep::Line),
            Block::Quote(children) => collect_fragments(children, &format!("{indent}> "), out),
            Block::List { start, items } => {
                for (i, item) in items.iter().enumerate() {
                    let marker = match (item.checked, start) {
                        (Some(true), _) => "[x] ".to_string(),
                        (Some(false), _) => "[ ] ".to_string(),
                        (None, Some(n)) => format!("{}. ", n + i as u64),
                        (None, None) => "• ".to_string(),
                    };
                    let first = out.len();
                    collect_fragments(&item.blocks, &format!("{indent}   "), out);
                    if let Some(f) = out.get_mut(first) {
                        f.prefix = format!("{indent}{marker}");
                    }
                }
            }
            Block::Table { header, rows, .. } => {
                let ncols = table_columns(header, rows);
                let empty = Inline::default();
                for row in std::iter::once(header).chain(rows) {
                    for c in 0..ncols {
                        let cell = row.get(c).unwrap_or(&empty);
                        if c == 0 {
                            push(out, &cell.text, indent, Sep::Line);
                        } else {
                            push(out, &cell.text, "", Sep::Tab);
                        }
                    }
                }
            }
            Block::Rule => {}
        }
    }
}

/// Names `@mentioned` in `blocks` (without the `@`), in order. Text in code
/// spans and code blocks is not parsed for mentions, so it never counts.
pub fn mentions(blocks: &[Block]) -> Vec<String> {
    fn inline(i: &Inline, out: &mut Vec<String>) {
        for (range, style) in &i.spans {
            if style.mention {
                out.push(i.text[range.clone()].trim_start_matches('@').to_string());
            }
        }
    }
    fn walk(blocks: &[Block], out: &mut Vec<String>) {
        for block in blocks {
            match block {
                Block::Paragraph(i) | Block::Heading(_, i) => inline(i, out),
                Block::Quote(children) => walk(children, out),
                Block::List { items, .. } => items.iter().for_each(|item| walk(&item.blocks, out)),
                Block::Table { header, rows, .. } => {
                    header.iter().chain(rows.iter().flatten()).for_each(|cell| inline(cell, out))
                }
                Block::Code { .. } | Block::Rule => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(blocks, &mut out);
    out
}

pub fn table_columns(header: &[Inline], rows: &[Vec<Inline>]) -> usize {
    header.len().max(rows.iter().map(Vec::len).max().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragment_order_and_prefixes() {
        let blocks = parse("# Title\n\n> quoted\n\n1. one\n2. two\n\n| a | b |\n|---|---|\n| 1 | 2 |");
        let f = fragments(&blocks);
        let texts: Vec<_> = f.iter().map(|f| f.text.as_str()).collect();
        assert_eq!(texts, ["Title", "quoted", "one", "two", "a", "b", "1", "2"]);
        assert_eq!(f[1].prefix, "> ");
        assert_eq!(f[3].prefix, "2. ");
        assert_eq!(f[5].sep, Sep::Tab);
        assert_eq!(f[6].sep, Sep::Line);
    }

    #[test]
    fn parses_table() {
        let blocks = parse("| a | b |\n|:--|--:|\n| 1 | **2** |\n| 3 | 4 |");
        let Block::Table {
            aligns,
            header,
            rows,
        } = &blocks[0]
        else {
            panic!("expected table, got {blocks:?}");
        };
        assert_eq!(aligns, &[Align::Left, Align::Right]);
        assert_eq!(header[0].text, "a");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][1].text, "2");
        assert!(rows[0][1].spans[0].1.bold);
    }

    #[test]
    fn inline_styles_and_links() {
        let blocks = parse("hi **bold _both_** `code` [docs](https://x.dev) @alice");
        let Block::Paragraph(p) = &blocks[0] else {
            panic!()
        };
        assert_eq!(p.text, "hi bold both code docs @alice");
        assert_eq!(p.links, vec![(18..22, "https://x.dev".to_string())]);
        assert!(p.spans.iter().any(|(r, s)| s.mention && &p.text[r.clone()] == "@alice"));
        assert!(p.spans.iter().any(|(r, s)| s.bold && s.italic && &p.text[r.clone()] == "both"));
    }

    #[test]
    fn mentions_next_to_cjk_text_but_not_in_emails() {
        let names = |src: &str| mentions(&parse(src));
        assert_eq!(names("完成了。@Reviewer 請看一下"), ["Reviewer"]);
        assert_eq!(names("請@Reviewer看一下，然後（@PM）"), ["Reviewer", "PM"]);
        assert_eq!(names("@小明 你好"), ["小明"]);
        assert_eq!(names("mail me@example.com or see a.b@c"), Vec::<String>::new());
        assert_eq!(names("ping @leo."), ["leo"]);
    }

    #[test]
    fn tight_and_task_lists() {
        let blocks = parse("- [x] done\n- [ ] todo\n  - nested");
        let Block::List { items, .. } = &blocks[0] else {
            panic!()
        };
        assert_eq!(items[0].checked, Some(true));
        assert_eq!(items[1].checked, Some(false));
        assert!(matches!(items[1].blocks[1], Block::List { .. }));
    }

    #[test]
    fn soft_breaks_and_emoji() {
        let blocks = parse("one :tada:\ntwo https://a.b/c.");
        let Block::Paragraph(p) = &blocks[0] else {
            panic!()
        };
        assert_eq!(p.text, "one 🎉\ntwo https://a.b/c.");
        assert_eq!(p.links[0].1, "https://a.b/c");
    }
}
