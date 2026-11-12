//! Searching the workspace from the top bar: channel and thread names, and
//! message text, matched as case-insensitive substrings.
//!
//! The whole workspace is in memory, so a search just scans it. Messages are
//! matched on the text as shown (markdown markers left out), so `all-hands`
//! finds `**all-hands**`.

use std::ops::Range;

use crate::data::{Conv, Message, Workspace};

/// Characters of context kept before a match in a message snippet.
const SNIPPET_BEFORE: usize = 30;
/// Most characters in a message snippet (more only if the match is longer).
const SNIPPET_LEN: usize = 120;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hit {
    /// A channel (or direct message conversation) whose name matches.
    Channel(usize),
    /// A thread whose name matches.
    Thread(usize),
    Message(MessageHit),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageHit {
    /// The message's id, which stays valid as messages come and go.
    pub id: usize,
    /// Where it was posted: its channel, or its thread for a thread reply.
    pub conv: Conv,
    pub author: usize,
    pub ts: i64,
    /// The message's text around the match, on one line.
    pub snippet: String,
    /// Where the match is in `snippet` (a byte range).
    pub matched: Range<usize>,
}

impl Workspace {
    /// Channels, threads and messages matching `query`, ignoring case: the
    /// names first (channels, then threads), then messages newest first.
    /// Names and messages are each cut to `limit`. A blank query finds
    /// nothing; a leading `#` is ignored when matching names.
    pub fn search(&self, query: &str, limit: usize) -> Vec<Hit> {
        let query = query.split_whitespace().collect::<Vec<_>>().join(" ");
        if query.is_empty() || limit == 0 {
            return Vec::new();
        }

        let mut hits = Vec::new();
        let name = fold(query.trim_start_matches('#').trim_start()).text;
        if !name.is_empty() {
            let channels = self.channels.iter().enumerate().map(|(c, ch)| (Hit::Channel(c), &ch.name));
            let threads = self.threads.iter().enumerate().map(|(t, th)| (Hit::Thread(t), &th.name));
            hits.extend(
                channels
                    .chain(threads)
                    .filter(|(_, n)| fold(n).text.contains(&name))
                    .map(|(hit, _)| hit)
                    .take(limit),
            );
        }

        let needle = fold(&query).text;
        let channel_msgs = self
            .channels
            .iter()
            .enumerate()
            .flat_map(|(c, ch)| ch.messages.iter().map(move |m| (Conv::Channel(c), m)));
        let thread_msgs = self
            .threads
            .iter()
            .enumerate()
            .flat_map(|(t, th)| th.messages.iter().map(move |m| (Conv::Thread(t), m)));
        let mut messages: Vec<MessageHit> = channel_msgs
            .chain(thread_msgs)
            .filter_map(|(conv, msg)| match_message(conv, msg, &needle))
            .collect();
        messages.sort_by_key(|m| std::cmp::Reverse((m.ts, m.id)));
        messages.truncate(limit);
        hits.extend(messages.into_iter().map(Hit::Message));
        hits
    }
}

fn match_message(conv: Conv, msg: &Message, needle: &str) -> Option<MessageHit> {
    let text = msg.plain_text();
    let folded = fold(&text);
    let start = folded.text.find(needle)?;
    let matched = folded.original(start..start + needle.len());
    let (snippet, matched) = snippet(&text, matched);
    Some(MessageHit {
        id: msg.id,
        conv,
        author: msg.author,
        ts: msg.ts,
        snippet,
        matched,
    })
}

/// Lowercased text, and for each of its bytes the character of the
/// original it came from. Lowercasing can change a character's length
/// (`İ` becomes two characters), so offsets don't carry over directly.
struct Folded {
    text: String,
    /// Byte range in the original of the character that produced each byte.
    origin: Vec<Range<usize>>,
}

impl Folded {
    /// The original range covering folded bytes `range` (non-empty).
    fn original(&self, range: Range<usize>) -> Range<usize> {
        self.origin[range.start].start..self.origin[range.end - 1].end
    }
}

/// Lowercase character by character (so a query folds the same way as the
/// text it's looked for in).
fn fold(s: &str) -> Folded {
    let mut text = String::with_capacity(s.len());
    let mut origin = Vec::with_capacity(s.len());
    for (i, c) in s.char_indices() {
        let before = text.len();
        text.extend(c.to_lowercase());
        origin.extend(std::iter::repeat_n(i..i + c.len_utf8(), text.len() - before));
    }
    Folded { text, origin }
}

/// Part of `text` around `matched`, with `…` where it was cut, and where the
/// match is in it.
fn snippet(text: &str, matched: Range<usize>) -> (String, Range<usize>) {
    // Up to SNIPPET_BEFORE characters before the match, from a word start.
    let mut start = text[..matched.start]
        .char_indices()
        .rev()
        .nth(SNIPPET_BEFORE - 1)
        .map_or(0, |(i, _)| i);
    if start > 0 {
        if let Some(space) = text[start..matched.start].find(' ') {
            start += space + 1;
        }
    }
    // Then up to SNIPPET_LEN characters in all, ending at a word end.
    let mut end = text[start..]
        .char_indices()
        .nth(SNIPPET_LEN)
        .map_or(text.len(), |(i, _)| start + i)
        .max(matched.end);
    if end < text.len() {
        if let Some(space) = text[matched.end..end].rfind(' ') {
            end = matched.end + space;
        }
    }

    let prefix = if start > 0 { "…" } else { "" };
    let suffix = if end < text.len() { "…" } else { "" };
    let shift = prefix.len() + matched.start - start;
    let snippet = format!("{prefix}{}{suffix}", &text[start..end]);
    (snippet, shift..shift + matched.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::seed;

    fn channel(ws: &Workspace, name: &str) -> usize {
        ws.channels.iter().position(|c| c.name == name).unwrap()
    }

    fn messages(hits: &[Hit]) -> Vec<&MessageHit> {
        hits.iter()
            .filter_map(|h| match h {
                Hit::Message(m) => Some(m),
                _ => None,
            })
            .collect()
    }

    fn matched(m: &MessageHit) -> &str {
        &m.snippet[m.matched.clone()]
    }

    #[test]
    fn blank_query_finds_nothing() {
        let ws = seed();
        assert!(ws.search("", 10).is_empty());
        assert!(ws.search("   \n", 10).is_empty());
        assert!(ws.search("search", 0).is_empty());
    }

    #[test]
    fn names_come_before_messages() {
        let ws = seed();
        let hits = ws.search("ENGINEERING", 10);
        assert_eq!(hits[0], Hit::Channel(channel(&ws, "engineering")));
        assert!(hits[1..].iter().all(|h| matches!(h, Hit::Message(_))));

        // "search" names the seeded thread and appears in many messages.
        let hits = ws.search("Search", 50);
        assert_eq!(hits[0], Hit::Thread(0));
        assert!(messages(&hits).len() > 3);
        let first_message = hits.iter().position(|h| matches!(h, Hit::Message(_))).unwrap();
        assert!(hits[first_message..].iter().all(|h| matches!(h, Hit::Message(_))));
    }

    #[test]
    fn hash_prefix_finds_a_channel() {
        let ws = seed();
        let hits = ws.search("#gen", 10);
        assert_eq!(hits.first(), Some(&Hit::Channel(channel(&ws, "general"))));
        assert_eq!(ws.search("# gen", 10).first(), Some(&Hit::Channel(channel(&ws, "general"))));
        // A lone `#` names nothing, but still finds messages mentioning channels.
        let hits = ws.search("#", 10);
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|h| matches!(h, Hit::Message(_))));
    }

    #[test]
    fn direct_messages_are_searched() {
        let ws = seed();
        let leo = channel(&ws, "Leo Okafor");
        assert!(ws.channels[leo].dm.is_some());
        let hits = ws.search("okafor", 10);
        assert_eq!(hits, vec![Hit::Channel(leo)]);

        let hits = ws.search("pushed the fix", 10);
        let found = messages(&hits);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].conv, Conv::Channel(leo));
        assert_eq!(found[0].id, ws.channels[leo].messages[0].id);
        assert_eq!(found[0].author, ws.channels[leo].messages[0].author);
        assert_eq!(matched(found[0]), "Pushed the fix");
    }

    #[test]
    fn markdown_markers_are_ignored() {
        let ws = seed();
        let general = channel(&ws, "general");
        // The source has `**all-hands**` and `*2:00 PM*`.
        let hits = ws.search("all-hands is at 2:00", 10);
        let found = messages(&hits);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, ws.channels[general].messages[0].id);
        assert_eq!(matched(found[0]), "all-hands is at 2:00");
        assert!(!found[0].snippet.contains('*'));
        // Emoji shortcodes are matched as the emoji shown.
        assert!(ws.search(":wave:", 10).is_empty());
    }

    #[test]
    fn thread_messages_point_at_their_thread() {
        let ws = seed();
        let hits = ws.search("old storage nodes", 10);
        let found = messages(&hits);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].conv, Conv::Thread(0));
        assert_eq!(ws.locate(found[0].id).map(|(conv, _)| conv), Some(Conv::Thread(0)));
        // The thread's starter is a channel message, found once.
        let starter = ws.search("index migration is stuck", 10);
        assert_eq!(messages(&starter).len(), 1);
        assert_eq!(messages(&starter)[0].conv, Conv::Channel(channel(&ws, "engineering")));
    }

    #[test]
    fn messages_are_newest_first_and_limited() {
        let ws = seed();
        let all = ws.search("search", 100);
        let all = messages(&all);
        assert!(all.len() > 3);
        assert!(all.windows(2).all(|w| (w[0].ts, w[0].id) > (w[1].ts, w[1].id)));

        let few = ws.search("search", 2);
        // At most two names and two messages: the two newest.
        assert_eq!(messages(&few).len(), 2);
        assert_eq!(messages(&few), all[..2].to_vec());
        assert!(few.len() <= 4);

        let mut ws = seed();
        ws.post(Conv::Channel(0), ws.me, "the search page is fast now");
        let hits = ws.search("search", 1);
        assert_eq!(matched(messages(&hits)[0]), "search");
        assert!(messages(&hits)[0].snippet.starts_with("the search page"));
    }

    #[test]
    fn chinese_text_is_found() {
        let mut ws = seed();
        let general = channel(&ws, "general");
        ws.post(Conv::Channel(general), ws.me, "明天下午三點討論**搜尋功能**的設計，請大家準備。");
        let t = ws.create_thread(general, 0, "搜尋功能討論");
        let hits = ws.search("搜尋功能", 10);
        assert_eq!(hits[0], Hit::Thread(t));
        let found = messages(&hits);
        assert_eq!(found.len(), 1);
        assert_eq!(matched(found[0]), "搜尋功能");
        assert_eq!(found[0].snippet, "明天下午三點討論搜尋功能的設計，請大家準備。");
    }

    #[test]
    fn characters_that_change_length_when_lowercased() {
        let mut ws = seed();
        // `İ` (2 bytes) lowercases to `i` + U+0307 (3 bytes); `ẞ` (3 bytes) to `ß` (2).
        ws.post(Conv::Channel(0), ws.me, "Offsite in İSTANBUL near the GROẞE Straße, ok?");
        let find = |q: &str| {
            let hits = ws.search(q, 10);
            messages(&hits).first().map(|m| matched(m).to_string())
        };
        assert_eq!(find("istanbul"), None, "`İ` folds to more than `i`");
        assert_eq!(find("i\u{307}stanbul").as_deref(), Some("İSTANBUL"));
        assert_eq!(find("stanbul").as_deref(), Some("STANBUL"));
        assert_eq!(find("große").as_deref(), Some("GROẞE"));
        assert_eq!(find("e straße, ok").as_deref(), Some("E Straße, ok"));
        // A match starting inside a character's lowercase form covers that character.
        assert_eq!(find("\u{307}stan").as_deref(), Some("İSTAN"));
    }

    #[test]
    fn long_messages_are_cut_around_the_match() {
        let mut ws = seed();
        let filler = "lorem ipsum dolor sit amet ".repeat(20);
        ws.post(Conv::Channel(0), ws.me, &format!("{filler}NEEDLE in the haystack {filler}"));
        let hits = ws.search("needle", 10);
        let m = messages(&hits)[0];
        assert_eq!(matched(m), "NEEDLE");
        assert!(m.snippet.starts_with('…') && m.snippet.ends_with('…'), "{}", m.snippet);
        assert!(m.snippet.chars().count() <= SNIPPET_LEN + 2);
        // Cut at word boundaries, with some context before the match.
        assert!(m.snippet.starts_with("…lorem") || m.snippet.starts_with("…ipsum") || m.snippet.starts_with("…dolor")
            || m.snippet.starts_with("…sit") || m.snippet.starts_with("…amet"), "{}", m.snippet);
        assert!(m.matched.start > "…".len() + 20);

        // A match at the very start or end isn't padded with `…` on that side.
        let hits = ws.search("lorem ipsum dolor", 10);
        assert!(!messages(&hits)[0].snippet.starts_with('…'));
        let mut ws = seed();
        let text = format!("{filler}the end");
        ws.post(Conv::Channel(0), ws.me, &text);
        let hits = ws.search("THE END", 10);
        let m = messages(&hits)[0];
        assert!(m.snippet.starts_with('…') && m.snippet.ends_with("the end"));

        // A match longer than a snippet is shown whole.
        let hits = ws.search(filler.trim(), 10);
        let m = messages(&hits)[0];
        assert_eq!(matched(m), filler.trim());
    }
}
