//! RSS／Atom 解析（RSS connector 與用 feed 的狀態頁共用）。
//!
//! 只取事件需要的欄位：標題、連結、id／guid、時間、內文與作者；其餘（分類、enclosure、
//! 圖片）不保留。命名空間前綴（`atom:link`、`content:encoded`）會在比對時去掉。

use pots_proto::Timestamp;
use quick_xml::Reader;
use quick_xml::XmlVersion;
use quick_xml::events::Event;
use serde::{Deserialize, Serialize};

/// feed 裡的一則項目。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    /// `guid`／`id`；沒有時用連結，再沒有時用標題與時間算一個。
    pub id: String,
    pub title: String,
    pub link: Option<String>,
    /// unix 秒。
    pub published: Option<i64>,
    pub body: String,
    pub author: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Feed {
    pub title: String,
    pub entries: Vec<Entry>,
}

/// 解析 RSS 2.0 或 Atom。不是 feed 時回說明。
pub fn parse(xml: &str) -> Result<Feed, String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut feed = Feed::default();
    let mut entries: Vec<Entry> = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut entry: Option<Entry> = None;
    // 正在讀哪個元素的值（`title`、`guid`…）；`author/name` 這種用路徑記。
    let mut text_target: Option<String> = None;
    let mut in_author = false;
    let mut author_name = String::new();
    loop {
        match reader.read_event() {
            Err(e) => return Err(format!("feed 不是合法的 XML：{e}")),
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref());
                if name == "item" || name == "entry" {
                    entry = Some(Entry::default());
                }
                if name == "author" {
                    in_author = true;
                    author_name.clear();
                }
                if name == "link" {
                    // Atom 的連結放在屬性；RSS 的 `<link>` 是文字。
                    if let Some(href) = attr(&e, "href")
                        && entry.as_ref().is_some_and(|x| x.link.is_none())
                        && let Some(x) = entry.as_mut()
                    {
                        x.link = Some(href);
                    }
                }
                text_target = text_field(&name, entry.is_some()).map(str::to_string);
                stack.push(name);
            }
            Ok(Event::Empty(e)) => {
                if local_name(e.name().as_ref()) == "link"
                    && let Some(href) = attr(&e, "href")
                    && let Some(x) = entry.as_mut()
                    && x.link.is_none()
                {
                    x.link = Some(href);
                }
            }
            Ok(Event::Text(t)) => {
                let text = t.xml_content(XmlVersion::Implicit1_0).unwrap_or_default();
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                if in_author {
                    if !author_name.is_empty() {
                        author_name.push(' ');
                    }
                    author_name.push_str(text);
                    continue;
                }
                if let Some(field) = &text_target {
                    match entry.as_mut() {
                        Some(x) => set_entry_field(x, field, text),
                        None => {
                            if field == "title" {
                                feed.title = text.to_string();
                            }
                        }
                    }
                }
            }
            Ok(Event::CData(t)) => {
                let text = t.xml_content(XmlVersion::Implicit1_0).unwrap_or_default();
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                if let Some(field) = &text_target {
                    match entry.as_mut() {
                        Some(x) => set_entry_field(x, field, text),
                        None => {
                            if field == "title" {
                                feed.title = text.to_string();
                            }
                        }
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = local_name(e.name().as_ref());
                if name == "author"
                    && let Some(x) = entry.as_mut()
                {
                    let name = author_name.trim();
                    if !name.is_empty() && x.author.is_none() {
                        x.author = Some(name.to_string());
                    }
                }
                if (name == "item" || name == "entry")
                    && let Some(x) = entry.take()
                {
                    entries.push(finish(x));
                }
                stack.pop();
                text_target = None;
            }
            _ => {}
        }
    }
    if entries.is_empty() {
        return Err("feed 裡沒有項目".into());
    }
    feed.entries = entries;
    Ok(feed)
}

/// 補上沒有 id 的項目。
fn finish(mut entry: Entry) -> Entry {
    if entry.id.is_empty() {
        entry.id = entry
            .link
            .clone()
            .unwrap_or_else(|| stable_id(&entry.title, entry.published));
    }
    entry
}

/// 沒有 guid 也沒有連結時的識別碼（同一個標題與時間會得到同一個值）。
fn stable_id(title: &str, published: Option<i64>) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(title.as_bytes());
    if let Some(t) = published {
        h.update(t.to_le_bytes());
    }
    format!("sha256:{}", hex::encode(&h.finalize()[..12]))
}

fn set_entry_field(entry: &mut Entry, field: &str, text: &str) {
    match field {
        "title" if entry.title.is_empty() => entry.title = text.to_string(),
        "guid" | "id" if entry.id.is_empty() => entry.id = text.to_string(),
        "link" if entry.link.is_none() => entry.link = Some(text.to_string()),
        "published" if entry.published.is_none() => entry.published = parse_time(text),
        "body" if entry.body.is_empty() => entry.body = text.to_string(),
        _ => {}
    }
}

/// 元素在 feed／entry 裡的意義。
fn text_field(name: &str, in_entry: bool) -> Option<&'static str> {
    match name {
        "title" => Some("title"),
        "guid" | "id" if in_entry => Some("guid"),
        // RSS 的連結是文字（Atom 的放在 href 屬性，上面已處理）。
        "link" if in_entry => Some("link"),
        // RSS 的 pubDate、Atom 的 published／updated（DC 的 date）。
        "pubdate" | "published" | "updated" | "date" if in_entry => Some("published"),
        "description" | "summary" | "content" | "encoded" if in_entry => Some("body"),
        _ => None,
    }
}

fn local_name(name: &[u8]) -> String {
    let name = String::from_utf8_lossy(name);
    name.rsplit(':')
        .next()
        .unwrap_or(&name)
        .to_ascii_lowercase()
}

fn attr(e: &quick_xml::events::BytesStart<'_>, key: &str) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        (local_name(a.key.as_ref()) == key)
            .then(|| {
                a.normalized_value(XmlVersion::Implicit1_0)
                    .ok()
                    .map(|v| v.into_owned())
            })
            .flatten()
    })
}

/// 解析 RSS 的 RFC 822／Atom 的 RFC 3339 時間；認不得就回 `None`。
fn parse_time(text: &str) -> Option<i64> {
    use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
    if let Ok(t) = DateTime::parse_from_rfc3339(text.trim()) {
        return Some(t.timestamp());
    }
    if let Ok(t) = DateTime::parse_from_rfc2822(text.trim()) {
        return Some(t.timestamp());
    }
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(t) = NaiveDateTime::parse_from_str(text.trim(), fmt) {
            return Some(Utc.from_utc_datetime(&t).timestamp());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// RSS／Atom connector

use super::event::{EventKind, ExternalEvent, Untrusted};
use super::source::SourceRow;
use crate::config::Clock;
use pots_proto::TrustLevel;

/// 最多記住幾則看過的項目。
const MAX_SEEN: usize = 500;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Cursor {
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub last_modified: Option<String>,
    #[serde(default)]
    pub seen: Vec<String>,
    #[serde(default)]
    pub last_poll: i64,
}

pub struct Outcome {
    pub events: Vec<ExternalEvent>,
    pub cursor: Cursor,
    pub error: Option<String>,
}

/// 輪詢一個 feed；第一次只記下現況，不把舊項目當新事件。
pub async fn poll(
    http: &reqwest::Client,
    source: &SourceRow,
    mut cursor: Cursor,
    clock: &Clock,
) -> Outcome {
    let url = source.config.url.clone().unwrap_or_default();
    let now = clock.now();
    let first = cursor.last_poll == 0 && cursor.seen.is_empty();
    let mut req = http.get(&url);
    if let Some(etag) = &cursor.etag {
        req = req.header(reqwest::header::IF_NONE_MATCH, etag);
    }
    if let Some(lm) = &cursor.last_modified {
        req = req.header(reqwest::header::IF_MODIFIED_SINCE, lm);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            return Outcome {
                events: vec![],
                cursor,
                error: Some(e.to_string()),
            };
        }
    };
    if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
        cursor.last_poll = now.0;
        return Outcome {
            events: vec![],
            cursor,
            error: None,
        };
    }
    if !resp.status().is_success() {
        return Outcome {
            events: vec![],
            cursor,
            error: Some(format!("HTTP {}", resp.status().as_u16())),
        };
    }
    let etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let last_modified = resp
        .headers()
        .get(reqwest::header::LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => {
            return Outcome {
                events: vec![],
                cursor,
                error: Some(e.to_string()),
            };
        }
    };
    let parsed = match parse(&body) {
        Ok(f) => f,
        Err(e) => {
            return Outcome {
                events: vec![],
                cursor,
                error: Some(e),
            };
        }
    };
    cursor.etag = etag;
    cursor.last_modified = last_modified;
    cursor.last_poll = now.0;
    let mut events = Vec::new();
    for entry in &parsed.entries {
        let fresh = !cursor.seen.iter().any(|s| s == &entry.id);
        if fresh {
            cursor.seen.push(entry.id.clone());
        }
        if first || !fresh {
            continue;
        }
        let mut fields = std::collections::BTreeMap::new();
        if let Some(author) = &entry.author {
            fields.insert("author".to_string(), author.clone());
        }
        events.push(ExternalEvent {
            source: source.id.clone(),
            kind: EventKind::FeedItem,
            resource: format!("feed:{}:{}", source.id, entry.id),
            dedupe_key: format!("feed:{}:{}", source.id, entry.id),
            occurred_at: entry.published.map(Timestamp).unwrap_or(now),
            trust: TrustLevel::Public,
            summary: if parsed.title.is_empty() {
                format!("新項目：{}", entry.title)
            } else {
                format!("{}：{}", parsed.title, entry.title)
            },
            body: Untrusted(entry.body.clone()),
            links: entry.link.clone().into_iter().collect(),
            fields,
            backfill: false,
        });
    }
    if cursor.seen.len() > MAX_SEEN {
        let drop = cursor.seen.len() - MAX_SEEN;
        cursor.seen.drain(..drop);
    }
    Outcome {
        events,
        cursor,
        error: None,
    }
}

pub fn describe(row: &SourceRow, cursor: &serde_json::Value) -> serde_json::Value {
    let c: Cursor = serde_json::from_value(cursor.clone()).unwrap_or_default();
    serde_json::json!({
        "kind": "rss",
        "url": row.config.url,
        "seen": c.seen.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RSS: &str = r#"<?xml version="1.0"?>
<rss version="2.0" xmlns:content="http://purl.org/rss/1.0/modules/content/">
  <channel>
    <title>Release notes</title>
    <item>
      <title><![CDATA[v1.2.0 發佈]]></title>
      <link>https://example.com/v1.2.0</link>
      <guid isPermaLink="false">rel-1.2.0</guid>
      <pubDate>Mon, 09 Nov 2026 10:00:00 +0800</pubDate>
      <description>修正了一些東西</description>
      <author>team@example.com</author>
    </item>
  </channel>
</rss>"#;

    const ATOM: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Status</title>
  <entry>
    <title>Degraded API</title>
    <link rel="alternate" href="https://status.example.com/incidents/1"/>
    <id>tag:status.example.com,2026:1</id>
    <updated>2026-11-09T02:00:00Z</updated>
    <summary>Investigating elevated errors</summary>
    <author><name>Example Ops</name></author>
  </entry>
</feed>"#;

    #[test]
    fn parses_rss_items() {
        let feed = parse(RSS).unwrap();
        assert_eq!(feed.title, "Release notes");
        let e = &feed.entries[0];
        assert_eq!(e.id, "rel-1.2.0");
        assert_eq!(e.title, "v1.2.0 發佈");
        assert_eq!(e.link.as_deref(), Some("https://example.com/v1.2.0"));
        assert_eq!(e.published, Some(1_794_189_600));
        assert_eq!(e.body, "修正了一些東西");
        assert_eq!(e.author.as_deref(), Some("team@example.com"));
    }

    #[test]
    fn parses_atom_entries_with_href_links() {
        let feed = parse(ATOM).unwrap();
        let e = &feed.entries[0];
        assert_eq!(e.id, "tag:status.example.com,2026:1");
        assert_eq!(
            e.link.as_deref(),
            Some("https://status.example.com/incidents/1")
        );
        assert_eq!(e.published, Some(1_794_189_600));
        assert_eq!(e.author.as_deref(), Some("Example Ops"));
    }

    #[test]
    fn entry_without_id_gets_a_stable_one() {
        let xml = r#"<rss><channel><item><title>x</title></item></channel></rss>"#;
        let a = parse(xml).unwrap();
        let b = parse(xml).unwrap();
        assert_eq!(a.entries[0].id, b.entries[0].id);
        assert!(a.entries[0].id.starts_with("sha256:"));
    }

    #[test]
    fn rejects_non_feed_xml() {
        assert!(parse("<html><body>hi</body></html>").is_err());
    }
}
