//! 通用 webhook connector：每個來源一個 URL 與 secret，收到的 JSON 用 JSON pointer
//! 對應成事件（`proposal/events-and-automation.md#connector-介面`）。
//!
//! - 驗簽：`X-Pots-Signature: sha256=<hex>`（HMAC-SHA256，secret 是來源的 secret）。
//!   也接受 `X-Hub-Signature-256`（GitHub 格式），方便既有服務直接打過來。
//! - 事件種類、資源鍵、摘要、去重與內文都用設定裡的 pointer 取；kind 必須是
//!   `[a-z0-9][a-z0-9._-]*`，長度 80 以內。
//! - 去重：有 dedupe pointer 就用它的值，沒有就用整個 body 的雜湊。

use std::collections::BTreeMap;

use hmac::{Hmac, Mac};
use pots_proto::WebhookMapping;
use serde_json::Value;
use sha2::Sha256;

use super::event::{EventKind, ExternalEvent, Untrusted};
use super::source::SourceRow;
/// 事件的種類名稱（listener 與 `WaitCond::Resource` 看到的名稱）。
const MAX_KIND: usize = 80;
const MAX_TEXT: usize = 20_000;

/// 檢查欄位對應；不合法時回說明。
pub fn check_mapping(m: &WebhookMapping) -> Result<(), String> {
    for (label, pointer) in [
        ("種類", &m.kind),
        ("資源", &m.resource),
        ("摘要", &m.summary),
        ("去重", &m.dedupe),
        ("內文", &m.body),
    ] {
        check_pointer(pointer).map_err(|e| format!("{label}的 JSON pointer 不對：{e}"))?;
    }
    if m.kind.trim().is_empty() {
        return Err("要指定事件種類的 JSON pointer（例如 `/type`）".into());
    }
    if m.resource.trim().is_empty() {
        return Err("要指定資源鍵的 JSON pointer（例如 `/id`）".into());
    }
    if m.summary.trim().is_empty() {
        return Err("要指定摘要的 JSON pointer".into());
    }
    Ok(())
}

fn check_pointer(p: &str) -> Result<(), String> {
    if p.is_empty() {
        return Ok(());
    }
    if !p.starts_with('/') {
        return Err("要以 `/` 開頭（或留空表示整個 body）".into());
    }
    Ok(())
}

/// 依 [RFC 6901](https://www.rfc-editor.org/rfc/rfc6901) 取值。
pub fn pointer<'a>(value: &'a Value, pointer: &str) -> Option<&'a Value> {
    if pointer.is_empty() {
        return Some(value);
    }
    let mut cur = value;
    for raw in pointer.trim_start_matches('/').split('/') {
        let key = raw.replace("~1", "/").replace("~0", "~");
        cur = match cur {
            Value::Object(map) => map.get(&key)?,
            Value::Array(list) => list.get(key.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

fn text_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// 驗證 HMAC；`header` 是 `sha256=<hex>` 或純 hex。
pub fn verify(secret: &str, body: &[u8], header: &str) -> Result<(), String> {
    let hex_sig = header
        .trim()
        .strip_prefix("sha256=")
        .unwrap_or(header.trim());
    let Ok(sig) = hex::decode(hex_sig) else {
        return Err("簽章格式不對".into());
    };
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|_| "secret 長度不對".to_string())?;
    mac.update(body);
    mac.verify_slice(&sig).map_err(|_| "簽章不符".to_string())
}

/// 把收到的 JSON 正規化成事件。
pub fn normalize(
    source: &SourceRow,
    body: &[u8],
    now: pots_proto::Timestamp,
) -> Result<ExternalEvent, String> {
    let json: Value = serde_json::from_slice(body).map_err(|e| format!("不是合法的 JSON：{e}"))?;
    let mapping = &source.config.mapping;
    let kind_name = text_of(pointer(&json, &mapping.kind))
        .trim()
        .to_ascii_lowercase();
    if kind_name.is_empty() || kind_name.len() > MAX_KIND {
        return Err(format!("事件種類要是 1 到 {MAX_KIND} 個字元"));
    }
    if !kind_name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        || !kind_name.contains('.')
        || kind_name.starts_with(['.', '_', '-'])
    {
        return Err(
            "事件種類只能用英文、數字與 `.`、`_`、`-`，而且至少兩段（例如 `alert.triggered`）"
                .into(),
        );
    }
    let resource = text_of(pointer(&json, &mapping.resource));
    if resource.trim().is_empty() {
        return Err("資源鍵是空的（檢查設定裡的 JSON pointer）".into());
    }
    let summary = text_of(pointer(&json, &mapping.summary));
    let dedupe = if mapping.dedupe.trim().is_empty() {
        body_hash(body)
    } else {
        let v = text_of(pointer(&json, &mapping.dedupe));
        if v.is_empty() { body_hash(body) } else { v }
    };
    let body_text = text_of(pointer(&json, &mapping.body));
    let mut fields = BTreeMap::new();
    fields.insert("state".to_string(), kind_name.clone());
    Ok(ExternalEvent {
        source: source.id.clone(),
        kind: EventKind::Custom {
            name: kind_name.clone(),
        },
        resource: format!("webhook:{}:{}", source.id, resource.trim()),
        dedupe_key: format!("{}:{}", source.id, dedupe.trim()),
        occurred_at: now,
        trust: source.trust,
        summary: if summary.trim().is_empty() {
            format!("{}：{kind_name}", source.name)
        } else {
            summary.trim().to_string()
        },
        body: Untrusted(body_text.chars().take(MAX_TEXT).collect::<String>()),
        links: Vec::new(),
        fields,
        backfill: false,
    })
}

fn body_hash(body: &[u8]) -> String {
    use sha2::Digest;
    let mut h = Sha256::new();
    h.update(body);
    format!("sha256:{}", hex::encode(&h.finalize()[..16]))
}
