//! HTTP 探測 connector：定時打一個 URL，連續失敗／成功才發事件
//! （`proposal/events-and-automation.md#正規化與去重`）。
//!
//! - 連續 3 次失敗才發 `probe.failed`，連續 2 次成功才發 `probe.recovered`，避免抖動。
//! - 狀態存在 `sources.cursor`：重啟後接續，不會重設連續次數。
//! - `probe.failed` 的信任等級是 Owned（自己的探測結果），其他來源種類不會產生這兩種事件。

use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::event::{EventKind, ExternalEvent};
use super::source::SourceRow;
use crate::config::Clock;

/// 連續幾次失敗才算失敗。
const FAILURES: u32 = 3;
/// 連續幾次成功才算恢復。
const RECOVERIES: u32 = 2;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Cursor {
    #[serde(default)]
    pub failures: u32,
    #[serde(default)]
    pub successes: u32,
    /// `None` 表示還沒確定（服務剛加進來）。
    #[serde(default)]
    pub up: Option<bool>,
    #[serde(default)]
    pub last_poll: i64,
    #[serde(default)]
    pub last_ms: u64,
}

pub struct Outcome {
    pub events: Vec<ExternalEvent>,
    pub cursor: Cursor,
    pub error: Option<String>,
}

pub async fn poll(
    http: &reqwest::Client,
    source: &SourceRow,
    mut cursor: Cursor,
    clock: &Clock,
) -> Outcome {
    let url = source.config.url.clone().unwrap_or_default();
    let started = Instant::now();
    let result = http.get(&url).send().await;
    let ms = started.elapsed().as_millis() as u64;
    let now = clock.now();
    cursor.last_poll = now.0;
    cursor.last_ms = ms;
    let mut events = Vec::new();
    let mut error = None;
    match result {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() || status.is_redirection() {
                cursor.failures = 0;
                cursor.successes += 1;
                if cursor.up == Some(false) && cursor.successes >= RECOVERIES {
                    events.push(recovered(source, status.as_u16(), ms, now));
                    cursor.up = Some(true);
                } else if cursor.up.is_none() {
                    cursor.up = Some(true);
                }
            } else {
                cursor.successes = 0;
                cursor.failures += 1;
                if cursor.up != Some(false) && cursor.failures >= FAILURES {
                    events.push(failed(
                        source,
                        &format!("HTTP {}", status.as_u16()),
                        cursor.failures,
                        now,
                    ));
                    cursor.up = Some(false);
                }
            }
        }
        Err(e) => {
            cursor.successes = 0;
            cursor.failures += 1;
            error = Some(e.to_string());
            if cursor.up != Some(false) && cursor.failures >= FAILURES {
                events.push(failed(source, &e.to_string(), cursor.failures, now));
                cursor.up = Some(false);
            }
        }
    }
    Outcome {
        events,
        cursor,
        error,
    }
}

fn failed(
    source: &SourceRow,
    reason: &str,
    failures: u32,
    now: pots_proto::Timestamp,
) -> ExternalEvent {
    ExternalEvent {
        source: source.id.clone(),
        kind: EventKind::ProbeFailed,
        resource: format!("probe:{}", source.id),
        dedupe_key: format!("{}:failed:{}", source.id, now.0),
        occurred_at: now,
        trust: pots_proto::TrustLevel::Owned,
        summary: format!(
            "探測失敗（連續 {failures} 次）：{}（{reason}）",
            source.name
        ),
        body: Default::default(),
        links: source.config.url.clone().into_iter().collect(),
        fields: [("state".to_string(), "down".to_string())]
            .into_iter()
            .collect(),
        backfill: false,
    }
}

fn recovered(
    source: &SourceRow,
    status: u16,
    ms: u64,
    now: pots_proto::Timestamp,
) -> ExternalEvent {
    ExternalEvent {
        source: source.id.clone(),
        kind: EventKind::ProbeRecovered,
        resource: format!("probe:{}", source.id),
        dedupe_key: format!("{}:recovered:{}", source.id, now.0),
        occurred_at: now,
        trust: pots_proto::TrustLevel::Owned,
        summary: format!("探測恢復：{}（HTTP {status}，{ms} ms）", source.name),
        body: Default::default(),
        links: source.config.url.clone().into_iter().collect(),
        fields: [("state".to_string(), "up".to_string())]
            .into_iter()
            .collect(),
        backfill: false,
    }
}

/// 給 `get_source_status` 用的一行摘要。
pub fn describe(row: &SourceRow, cursor: &serde_json::Value) -> serde_json::Value {
    let c: Cursor = serde_json::from_value(cursor.clone()).unwrap_or_default();
    json!({
        "kind": "probe",
        "url": row.config.url,
        "up": c.up,
        "failures": c.failures,
        "successes": c.successes,
        "last_latency_ms": c.last_ms,
    })
}
