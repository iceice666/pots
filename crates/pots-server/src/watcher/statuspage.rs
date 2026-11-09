//! 狀態頁 connector：支援 statuspage.io 的 `summary.json` 與 RSS／Atom 形式的狀態頁。
//!
//! - JSON：incident 開啟／更新／解決與元件狀態變化，同一個 incident 五分鐘內的多次更新
//!   合併成一則（第一次輪詢只記下現況，不把舊的 incident 當新事件）。
//! - RSS／Atom：每個新項目是一個 incident；標題或內容寫到 resolved／completed 時當成解決。
//!   這是盡力而為，正式的 incident 狀態請用 JSON。

use std::collections::BTreeMap;

use chrono::DateTime;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::event::{EventKind, ExternalEvent, Untrusted};
use super::feed;
use super::source::SourceRow;
use crate::config::Clock;
use pots_proto::{Timestamp, TrustLevel};

/// 同一個 incident 的更新合併多久（秒）。
const MERGE_SECS: i64 = 300;
/// RSS 形式最多記住幾則看過的項目。
const MAX_SEEN: usize = 200;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Cursor {
    #[serde(default)]
    pub incidents: BTreeMap<String, Seen>,
    #[serde(default)]
    pub components: BTreeMap<String, String>,
    /// RSS 形式看過的項目 id。
    #[serde(default)]
    pub seen: Vec<String>,
    #[serde(default)]
    pub last_poll: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Seen {
    pub status: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub last_emit: i64,
}

#[derive(Deserialize, Default)]
struct Summary {
    #[serde(default)]
    page: Page,
    #[serde(default)]
    incidents: Vec<Incident>,
    #[serde(default)]
    components: Vec<Component>,
}

#[derive(Deserialize, Default)]
struct Page {
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct Incident {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    impact: Option<String>,
    #[serde(default)]
    shortlink: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
}

#[derive(Deserialize)]
struct Component {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
}

pub struct Outcome {
    pub events: Vec<ExternalEvent>,
    pub cursor: Cursor,
    pub error: Option<String>,
}

pub async fn poll(
    http: &reqwest::Client,
    source: &SourceRow,
    cursor: Cursor,
    clock: &Clock,
) -> Outcome {
    let url = source.config.url.clone().unwrap_or_default();
    let now = clock.now();
    let body = match http.get(&url).send().await {
        Ok(resp) => {
            if !resp.status().is_success() {
                return Outcome {
                    events: vec![],
                    cursor,
                    error: Some(format!("HTTP {}", resp.status().as_u16())),
                };
            }
            match resp.text().await {
                Ok(t) => t,
                Err(e) => {
                    return Outcome {
                        events: vec![],
                        cursor,
                        error: Some(e.to_string()),
                    };
                }
            }
        }
        Err(e) => {
            return Outcome {
                events: vec![],
                cursor,
                error: Some(e.to_string()),
            };
        }
    };
    let first = cursor.last_poll == 0 && cursor.incidents.is_empty() && cursor.seen.is_empty();
    let mut cursor = cursor;
    cursor.last_poll = now.0;
    if body.trim_start().starts_with('{') {
        match serde_json::from_str::<Summary>(&body) {
            Ok(summary) => {
                let events = from_summary(source, summary, &mut cursor, none_first(first), now);
                Outcome {
                    events,
                    cursor,
                    error: None,
                }
            }
            Err(e) => Outcome {
                events: vec![],
                error: Some(format!("狀態頁 JSON 解析失敗：{e}")),
                cursor,
            },
        }
    } else {
        match feed::parse(&body) {
            Ok(feed) => {
                let events = from_feed(source, feed, &mut cursor, first, now);
                Outcome {
                    events,
                    cursor,
                    error: None,
                }
            }
            Err(e) => Outcome {
                events: vec![],
                error: Some(format!("狀態頁格式不認得（不是 JSON 也不是 feed）：{e}")),
                cursor,
            },
        }
    }
}

/// 第一次輪詢只記現況，不發事件。
fn none_first(first: bool) -> bool {
    first
}

fn from_summary(
    source: &SourceRow,
    summary: Summary,
    cursor: &mut Cursor,
    first: bool,
    now: Timestamp,
) -> Vec<ExternalEvent> {
    let mut events = Vec::new();
    let page = if summary.page.name.is_empty() {
        source.name.clone()
    } else {
        summary.page.name.clone()
    };
    for inc in summary.incidents {
        let id = if inc.id.is_empty() {
            inc.name.clone()
        } else {
            inc.id.clone()
        };
        let status = inc.status.to_ascii_lowercase();
        let resolved = is_resolved(&status);
        let updated_at = inc.updated_at.clone().unwrap_or_default();
        let prev = cursor.incidents.get(&id).cloned();
        if first {
            cursor.incidents.insert(
                id,
                Seen {
                    status,
                    updated_at,
                    last_emit: 0,
                },
            );
            continue;
        }
        let changed = prev.as_ref().is_none_or(|p| {
            p.status != status || (!updated_at.is_empty() && p.updated_at != updated_at)
        });
        if !changed {
            continue;
        }
        // 五分鐘內的多次更新合併成一則；解決一定發。
        if !resolved
            && prev
                .as_ref()
                .is_some_and(|p| now.0 - p.last_emit < MERGE_SECS)
        {
            if let Some(p) = cursor.incidents.get_mut(&id) {
                p.status = status;
                p.updated_at = updated_at;
            }
            continue;
        }
        let reopened = prev.as_ref().is_some_and(|p| is_resolved(&p.status)) && !resolved;
        let kind = match (resolved, reopened) {
            (true, _) => EventKind::StatusIncidentResolved,
            (false, true) => EventKind::StatusIncidentOpened,
            (false, false) => {
                if prev.is_none() {
                    EventKind::StatusIncidentOpened
                } else {
                    EventKind::StatusIncidentUpdated
                }
            }
        };
        let summary_text = match kind {
            EventKind::StatusIncidentResolved => format!("{page}：{} 已解決", inc.name),
            EventKind::StatusIncidentOpened => format!("{page}：開啟 incident「{}」", inc.name),
            _ => format!("{page}：incident「{}」更新（{status}）", inc.name),
        };
        let mut fields = BTreeMap::new();
        fields.insert("state".to_string(), status.clone());
        if let Some(impact) = &inc.impact {
            fields.insert("impact".to_string(), impact.clone());
        }
        events.push(ExternalEvent {
            source: source.id.clone(),
            kind,
            resource: format!("status:{}:{}", source.id, id),
            dedupe_key: format!(
                "{}:incident:{}:{}:{}",
                source.id,
                id,
                status,
                if updated_at.is_empty() {
                    now.0.to_string()
                } else {
                    updated_at.clone()
                }
            ),
            occurred_at: parse_time(&updated_at).unwrap_or(now),
            trust: TrustLevel::Public,
            summary: summary_text,
            body: Default::default(),
            links: inc.shortlink.clone().into_iter().collect(),
            fields,
            backfill: false,
        });
        cursor.incidents.insert(
            id,
            Seen {
                status,
                updated_at,
                last_emit: now.0,
            },
        );
    }
    for comp in summary.components {
        let id = if comp.id.is_empty() {
            comp.name.clone()
        } else {
            comp.id.clone()
        };
        let status = comp.status.to_ascii_lowercase();
        let prev = cursor.components.insert(id.clone(), status.clone());
        if first || prev.as_deref() == Some(status.as_str()) {
            continue;
        }
        events.push(ExternalEvent {
            source: source.id.clone(),
            kind: EventKind::StatusComponentChanged,
            resource: format!("status:{}:component:{}", source.id, id),
            dedupe_key: format!("{}:component:{}:{}:{}", source.id, id, status, now.0),
            occurred_at: now,
            trust: TrustLevel::Public,
            summary: format!(
                "{page}：元件 {} 變成 {status}{}",
                comp.name,
                prev.map(|p| format!("（本來是 {p}）")).unwrap_or_default()
            ),
            body: Default::default(),
            links: vec![],
            fields: [("state".to_string(), status)].into_iter().collect(),
            backfill: false,
        });
    }
    events
}

fn from_feed(
    source: &SourceRow,
    feed: feed::Feed,
    cursor: &mut Cursor,
    first: bool,
    now: Timestamp,
) -> Vec<ExternalEvent> {
    let page = if feed.title.is_empty() {
        source.name.clone()
    } else {
        feed.title.clone()
    };
    let mut events = Vec::new();
    for entry in feed.entries {
        let id = entry.id.clone();
        if !first && cursor.seen.iter().any(|s| s == &id) {
            continue;
        }
        if !first {
            let text = format!("{} {}", entry.title, entry.body).to_ascii_lowercase();
            let resolved = text.contains("resolved")
                || text.contains("completed")
                || text.contains("已解決")
                || text.contains("恢復正常");
            let kind = if resolved {
                EventKind::StatusIncidentResolved
            } else {
                EventKind::StatusIncidentOpened
            };
            events.push(ExternalEvent {
                source: source.id.clone(),
                kind,
                resource: format!("status:{}:{}", source.id, id),
                dedupe_key: format!("{}:feed:{}", source.id, id),
                occurred_at: entry.published.map(Timestamp).unwrap_or(now),
                trust: TrustLevel::Public,
                summary: format!("{page}：{}", entry.title),
                body: Untrusted(entry.body.clone()),
                links: entry.link.clone().into_iter().collect(),
                fields: BTreeMap::new(),
                backfill: false,
            });
        }
        cursor.seen.push(id);
    }
    if cursor.seen.len() > MAX_SEEN {
        let drop = cursor.seen.len() - MAX_SEEN;
        cursor.seen.drain(..drop);
    }
    events
}

fn is_resolved(status: &str) -> bool {
    matches!(status, "resolved" | "completed" | "postmortem")
}

fn parse_time(text: &str) -> Option<Timestamp> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|t| Timestamp(t.timestamp()))
}

pub fn describe(row: &SourceRow, cursor: &serde_json::Value) -> serde_json::Value {
    let c: Cursor = serde_json::from_value(cursor.clone()).unwrap_or_default();
    json!({
        "kind": "statuspage",
        "url": row.config.url,
        "open_incidents": c.incidents.values().filter(|s| !is_resolved(&s.status)).count(),
        "components": c.components,
    })
}
