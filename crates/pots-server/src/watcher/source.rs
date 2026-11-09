//! 通用外部來源的資料列、秘密檔與健康狀態（`proposal/events-and-automation.md#connector-介面`）。
//!
//! GitHub 是特例（`github.rs` 自己管 token 與設定），其他來源（RSS、HTTP 探測、狀態頁、
//! 通用 webhook）共用這裡的 `sources` 資料列：`config` 放非秘密設定，secret 放資料目錄的
//! `auth/sources/<hex(id)>.json`（`0600`），`cursor` 放各 connector 的輪詢進度，
//! `health` 放最近一次的成功與錯誤。

use std::path::Path;
use std::time::Duration;

use pots_proto::{SourceDraft, SourceHealth, SourceKind, SourceView, Timestamp, TrustLevel};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::sqlite::SqliteConnection;

use crate::Error;
use crate::datadir::{create_private_dirs_under, write_private_file};

/// 輪詢間隔的下限與上限（秒）。
pub const MIN_INTERVAL_SECS: u32 = 30;
pub const MAX_INTERVAL_SECS: u32 = 3600;
/// 一個來源最多記住幾個看過的自訂事件種類（listener 表單用）。
const MAX_CUSTOM_KINDS: usize = 50;

/// `sources.health` 的內容。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Health {
    pub error: Option<String>,
    pub since: Option<Timestamp>,
    pub last_ok: Option<Timestamp>,
}

/// `sources.config` 的內容（GitHub 以外的來源）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SourceConfig {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub interval_secs: Option<u32>,
    #[serde(default)]
    pub mapping: pots_proto::WebhookMapping,
    /// webhook 看過的自訂事件種類（給 listener 表單列選項）。
    #[serde(default)]
    pub observed_kinds: Vec<String>,
}

/// 一個 GitHub 以外的來源。
#[derive(Clone, Debug)]
pub struct SourceRow {
    pub id: String,
    pub kind: SourceKind,
    pub name: String,
    pub config: SourceConfig,
    pub trust: TrustLevel,
    pub cursor: serde_json::Value,
    pub enabled: bool,
}

impl SourceRow {
    /// 幾秒輪詢一次（夾在 [`MIN_INTERVAL_SECS`] 到 [`MAX_INTERVAL_SECS`]）。
    pub fn interval(&self) -> Duration {
        let secs = self
            .config
            .interval_secs
            .filter(|s| *s > 0)
            .unwrap_or_else(|| self.kind.default_interval_secs())
            .clamp(MIN_INTERVAL_SECS, MAX_INTERVAL_SECS);
        Duration::from_secs(u64::from(secs))
    }
}

/// 秘密檔的內容。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct SecretFile {
    #[serde(default)]
    secret: Option<String>,
}

/// 秘密檔的路徑：id 用 hex 編碼，避免 `:` 之類的字元進到檔名。
fn secret_path(data_dir: &Path, id: &str) -> std::path::PathBuf {
    data_dir
        .join("auth/sources")
        .join(format!("{}.json", hex::encode(id.as_bytes())))
}

pub fn read_secret(data_dir: &Path, id: &str) -> Option<String> {
    std::fs::read(secret_path(data_dir, id))
        .ok()
        .and_then(|b| serde_json::from_slice::<SecretFile>(&b).ok())
        .and_then(|s| s.secret)
}

fn write_secret(data_dir: &Path, id: &str, secret: Option<&str>) -> std::io::Result<()> {
    // 目錄走 `auth/sources`，和 GitHub 的 token 一樣是私有的。
    create_private_dirs_under(data_dir, Path::new("auth/sources"))?;
    let path = secret_path(data_dir, id);
    match secret {
        Some(s) => write_private_file(
            &path,
            &serde_json::to_vec(&SecretFile {
                secret: Some(s.to_string()),
            })?,
        ),
        None => match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        },
    }
}

/// 通用 webhook 的路徑（`POST /webhooks/source/<id>`）。
pub fn webhook_path(id: &str) -> String {
    format!("/webhooks/source/{id}")
}

fn parse_trust(s: &str) -> TrustLevel {
    match s {
        "owned" => TrustLevel::Owned,
        "public" => TrustLevel::Public,
        _ => TrustLevel::Members,
    }
}

fn kind_str(kind: SourceKind) -> &'static str {
    kind.as_str()
}

fn row_from(row: &sqlx::sqlite::SqliteRow) -> Option<SourceRow> {
    let kind = row.try_get::<String, _>("kind").ok()?;
    let kind = SourceKind::parse(&kind)?;
    let config: String = row.try_get("config").ok()?;
    let trust: String = row.try_get("trust").ok()?;
    let cursor: String = row.try_get("cursor").ok()?;
    Some(SourceRow {
        id: row.try_get("id").ok()?,
        kind,
        name: row.try_get("name").ok()?,
        config: serde_json::from_str(&config).unwrap_or_default(),
        trust: parse_trust(&trust),
        cursor: serde_json::from_str(&cursor).unwrap_or_default(),
        enabled: row.try_get::<i64, _>("enabled").ok()? != 0,
    })
}

/// 所有 GitHub 以外的來源。
pub async fn rows(conn: &mut SqliteConnection) -> Result<Vec<SourceRow>, Error> {
    let rows = sqlx::query(
        "SELECT id, kind, name, config, trust, cursor, enabled FROM sources \
         WHERE kind IN ('rss', 'probe', 'statuspage', 'webhook') ORDER BY id",
    )
    .fetch_all(conn)
    .await?;
    Ok(rows.iter().filter_map(row_from).collect())
}

pub async fn row(conn: &mut SqliteConnection, id: &str) -> Result<Option<SourceRow>, Error> {
    let row = sqlx::query(
        "SELECT id, kind, name, config, trust, cursor, enabled FROM sources WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(conn)
    .await?;
    Ok(row.as_ref().and_then(row_from))
}

pub async fn health(conn: &mut SqliteConnection, id: &str) -> Result<Health, Error> {
    let text: Option<String> = sqlx::query_scalar("SELECT health FROM sources WHERE id = ?")
        .bind(id)
        .fetch_optional(conn)
        .await?;
    Ok(text
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default())
}

pub async fn set_cursor(
    conn: &mut SqliteConnection,
    id: &str,
    cursor: &serde_json::Value,
) -> Result<(), Error> {
    sqlx::query("UPDATE sources SET cursor = ? WHERE id = ?")
        .bind(serde_json::to_string(cursor)?)
        .bind(id)
        .execute(conn)
        .await?;
    Ok(())
}

/// 記下這一輪的結果；狀態改變時回傳 `true`（呼叫端再推 `Automation`）。
pub async fn record_health(
    conn: &mut SqliteConnection,
    id: &str,
    now: Timestamp,
    error: Option<String>,
) -> Result<bool, Error> {
    let mut h = health(conn, id).await?;
    let before = (h.error.is_some(), h.last_ok.is_some());
    match error {
        Some(message) => {
            if h.error.is_none() {
                h.since = Some(now);
            }
            h.error = Some(message);
        }
        None => {
            h.error = None;
            h.since = None;
            h.last_ok = Some(now);
        }
    }
    sqlx::query("UPDATE sources SET health = ?, updated_at = ? WHERE id = ?")
        .bind(serde_json::to_string(&h)?)
        .bind(now.0)
        .bind(id)
        .execute(conn)
        .await?;
    Ok(before != (h.error.is_some(), h.last_ok.is_some()))
}

/// 看過的自訂事件種類（通用 webhook 對應出來的），給 listener 表單列選項。
pub async fn custom_kinds(conn: &mut SqliteConnection) -> Result<Vec<String>, Error> {
    let mut out: Vec<String> = Vec::new();
    for s in rows(conn).await? {
        if s.kind != SourceKind::Webhook {
            continue;
        }
        for k in s.config.observed_kinds {
            if !out.contains(&k) {
                out.push(k);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// 新增或修改一個來源；不合法時回說明（給使用者看）。
pub async fn save(
    conn: &mut SqliteConnection,
    data_dir: &Path,
    draft: &SourceDraft,
    now: Timestamp,
) -> Result<(), String> {
    let id = draft.id.trim();
    if id.is_empty() || id.len() > 64 {
        return Err("來源 id 要是 1 到 64 個字元".into());
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.' | ':'))
        || id.starts_with(['-', '_', '.', ':'])
    {
        return Err("來源 id 只能用英文小寫、數字與 `-`、`_`、`.`、`:`，且不能以符號開頭".into());
    }
    if id.starts_with("github:") {
        return Err("`github:` 開頭是 GitHub connector 專用".into());
    }
    let name = draft.name.trim();
    if name.is_empty() || name.chars().count() > 60 {
        return Err("名稱要是 1 到 60 個字".into());
    }
    let mut config = match row(conn, id).await {
        Ok(Some(existing)) => existing.config,
        _ => SourceConfig::default(),
    };
    match draft.kind {
        SourceKind::Rss | SourceKind::Probe | SourceKind::StatusPage => {
            let url = draft.url.as_deref().map(str::trim).unwrap_or_default();
            crate::watcher::source::check_url(url)?;
            config.url = Some(url.to_string());
            config.mapping = pots_proto::WebhookMapping::default();
            config.observed_kinds.clear();
        }
        SourceKind::Webhook => {
            if draft.url.as_deref().is_some_and(|u| !u.trim().is_empty()) {
                return Err("通用 webhook 不收 URL".into());
            }
            config.url = None;
            crate::watcher::webhook::check_mapping(&draft.mapping)?;
            config.mapping = draft.mapping.clone();
        }
    }
    if let Some(secs) = draft.interval_secs {
        if draft.kind == SourceKind::Webhook {
            if secs != 0 {
                return Err("通用 webhook 不輪詢，沒有間隔".into());
            }
        } else if !(MIN_INTERVAL_SECS..=MAX_INTERVAL_SECS).contains(&secs) {
            return Err(format!(
                "輪詢間隔要是 {MIN_INTERVAL_SECS} 到 {MAX_INTERVAL_SECS} 秒"
            ));
        }
        config.interval_secs = (draft.kind != SourceKind::Webhook).then_some(secs);
    }
    if draft.kind == SourceKind::Webhook {
        match draft.secret.as_deref().map(str::trim) {
            Some("") => write_secret(data_dir, id, None).map_err(|e| e.to_string())?,
            Some(s) => {
                if s.len() < 16 || s.chars().any(char::is_whitespace) {
                    return Err("webhook secret 至少 16 個字元，不能有空白".into());
                }
                write_secret(data_dir, id, Some(s)).map_err(|e| e.to_string())?;
            }
            None => {
                if read_secret(data_dir, id).is_none() {
                    return Err("通用 webhook 需要一個 secret".into());
                }
            }
        }
    } else {
        // 換掉種類時把不再用的 secret 清掉。
        write_secret(data_dir, id, None).map_err(|e| e.to_string())?;
    }
    let ingest = match draft.kind {
        SourceKind::Webhook => vec!["push"],
        _ => vec!["poll"],
    };
    sqlx::query(
        "INSERT INTO sources (id, kind, name, config, trust, ingest, enabled, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (id) DO UPDATE SET kind = excluded.kind, name = excluded.name, \
         config = excluded.config, trust = excluded.trust, ingest = excluded.ingest, \
         enabled = excluded.enabled, updated_at = excluded.updated_at",
    )
    .bind(id)
    .bind(kind_str(draft.kind))
    .bind(name)
    .bind(serde_json::to_string(&config).map_err(|e| e.to_string())?)
    .bind(match draft.trust {
        TrustLevel::Owned => "owned",
        TrustLevel::Members => "members",
        TrustLevel::Public => "public",
    })
    .bind(serde_json::to_string(&ingest).map_err(|e| e.to_string())?)
    .bind(i64::from(draft.enabled))
    .bind(now.0)
    .bind(now.0)
    .execute(conn)
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub async fn delete(conn: &mut SqliteConnection, data_dir: &Path, id: &str) -> Result<(), Error> {
    sqlx::query("DELETE FROM sources WHERE id = ? AND kind != 'github'")
        .bind(id)
        .execute(conn)
        .await?;
    if let Err(e) = write_secret(data_dir, id, None) {
        tracing::warn!(error = %e, %id, "無法刪除來源的 secret");
    }
    Ok(())
}

/// 檢查輪詢的 URL：只收 http/https，不接受內嵌帳密。
pub fn check_url(url: &str) -> Result<(), String> {
    if url.is_empty() || url.len() > 2000 {
        return Err("要填一個 URL".into());
    }
    let parsed = url::Url::parse(url).map_err(|_| "URL 格式不對".to_string())?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("URL 只收 http 或 https".into());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("URL 不能內嵌帳號密碼".into());
    }
    if parsed.host_str().is_none() {
        return Err("URL 要有 host".into());
    }
    Ok(())
}

/// 記下一個自訂事件種類（上限 [`MAX_CUSTOM_KINDS`]）；新的種類回傳 `true`。
pub fn observe_kind(config: &mut SourceConfig, kind: &str) -> bool {
    if config.observed_kinds.iter().any(|k| k == kind) {
        return false;
    }
    config.observed_kinds.push(kind.to_string());
    if config.observed_kinds.len() > MAX_CUSTOM_KINDS {
        config.observed_kinds.remove(0);
    }
    true
}

/// 自動化頁的來源清單（GitHub 也在裡面，由 `mod.rs` 一起組）。
pub fn view_of(row: &SourceRow, health: &Health) -> SourceView {
    SourceView {
        id: row.id.clone(),
        kind: row.kind.label().to_string(),
        name: row.name.clone(),
        trust: row.trust,
        ingest: match row.kind {
            SourceKind::Webhook => "webhook".into(),
            _ => format!("輪詢（每 {} 秒）", row.interval().as_secs()),
        },
        health: match (&health.error, health.since, health.last_ok) {
            (Some(message), since, _) => SourceHealth::Error {
                message: message.clone(),
                since: since.unwrap_or(Timestamp(0)),
            },
            (None, _, Some(_)) => SourceHealth::Ok,
            (None, _, None) => SourceHealth::Unknown,
        },
        last_ok: health.last_ok,
        enabled: row.enabled,
        url: row.config.url.clone(),
        kind_id: row.kind.as_str().to_string(),
        mapping: row.config.mapping.clone(),
    }
}
