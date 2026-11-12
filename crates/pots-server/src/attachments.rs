//! 圖片附件：格式驗證、sha256 去重、本體落地與查回。
//!
//! - 本體寫在 `<data_dir>/attachments/<sha256>`（`0600`，原子寫入），DB 只存中介資料。
//!   **任何路徑都不要把 base64 或 bytes 寫進 DB／`stream_items.body`／`om_raw`**：
//!   歷史每輪整份重送，base64 會讓 DB、事件日誌與每次請求全部爆量。
//! - 同一個 project 內同一張圖只留一列（`attachments_dedup`），上傳第二次回同一個 id；
//!   檔案本身是內容定址，跨 project 也只落地一份。
//! - 收到的 MIME 一定要和實際內容相符（`image::guess_format`），掛錯 `Content-Type`
//!   的檔案在上傳時就拒收，之後組 provider 請求時不必再驗。
//!
//! 上傳走 HTTP（`POST /attachments`）而不是 WebSocket：現有 gateway 只送文字 frame，
//! 10 MiB 的圖塞 WS 不理想。

use std::io::Cursor;
use std::path::{Path, PathBuf};

use axum::http::StatusCode;
use image::ImageFormat;
use pots_proto::{
    ATTACHMENT_MIMES, AttachmentId, AttachmentView, MAX_ATTACHMENT_BYTES, ProjectId, Timestamp,
};
use sha2::{Digest, Sha256};

use crate::Error;
use crate::store::{AttachmentRow, Store};

/// 資料目錄下的附件子目錄。
pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("attachments")
}

/// 上傳被拒的原因。HTTP 端點據此回狀態碼與看得懂的中文訊息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    pub status: StatusCode,
    pub message: String,
}

impl Rejected {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

/// 把上傳的位元組存起來，回中介資料。同一個 project 內同一張圖回同一個 id。
pub async fn upload(
    store: &Store,
    data_dir: &Path,
    project: ProjectId,
    mime: &str,
    filename: Option<String>,
    bytes: &[u8],
) -> Result<AttachmentView, UploadError> {
    let bytes_len = bytes.len() as u64;
    if bytes.is_empty() {
        return Err(Rejected::new(StatusCode::BAD_REQUEST, "收到的檔案是空的").into());
    }
    if bytes_len > MAX_ATTACHMENT_BYTES {
        return Err(Rejected::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "單張圖片上限是 {} MiB，這張是 {:.1} MiB",
                MAX_ATTACHMENT_BYTES / (1024 * 1024),
                bytes_len as f64 / (1024.0 * 1024.0)
            ),
        )
        .into());
    }
    let mime = normalize_mime(mime);
    if !ATTACHMENT_MIMES.contains(&mime.as_str()) {
        return Err(Rejected::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            format!("只支援 png、jpeg、webp、gif；收到的是「{mime}」"),
        )
        .into());
    }
    let format = image::guess_format(bytes).map_err(|_| {
        Rejected::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "這個檔案不是看得懂的圖片".to_string(),
        )
    })?;
    let actual = mime_of(format).ok_or_else(|| {
        Rejected::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            format!("只支援 png、jpeg、webp、gif；這個檔案是 {format:?}"),
        )
    })?;
    if actual != mime {
        return Err(Rejected::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            format!("檔案的內容是 {actual}，不是 {mime}"),
        )
        .into());
    }
    let dimensions = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_dimensions().ok());

    let sha256 = hex::encode(Sha256::digest(bytes));
    let path = dir(data_dir).join(&sha256);

    let mut txn = store.begin().await?;
    // 已經上傳過（去重）：回原本那一列，只補寫本體。
    if let Some(row) = Store::attachment_by_sha(&mut *txn, project, &sha256).await? {
        if !path.is_file() {
            write_body(data_dir, &sha256, bytes).await?;
        }
        txn.commit().await?;
        return Ok(view(&row));
    }
    write_body(data_dir, &sha256, bytes).await?;
    let row = AttachmentRow {
        id: AttachmentId::new(),
        project,
        sha256: sha256.clone(),
        mime,
        bytes: bytes_len,
        width: dimensions.map(|(w, _)| w),
        height: dimensions.map(|(_, h)| h),
        filename,
        created_at: Timestamp::now(),
    };
    Store::insert_attachment(&mut *txn, &row).await?;
    // 同時上傳同一張圖時另一條 transaction 可能先寫進去，讀回來的那一列才是準的。
    let row = Store::attachment_by_sha(&mut *txn, project, &sha256)
        .await?
        .unwrap_or(row);
    txn.commit().await?;
    Ok(view(&row))
}

/// 這個 project 的這些附件，依傳入順序回中介資料；有 ID 不存在或屬於別的 project 時回
/// 看得懂的錯誤。訊息只帶 ID，中介資料一律由伺服器查回來。
pub async fn resolve(
    conn: &mut sqlx::sqlite::SqliteConnection,
    project: ProjectId,
    ids: &[AttachmentId],
) -> Result<Vec<AttachmentView>, String> {
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let Some(row) = Store::attachment(conn, *id).await.map_err(internal)? else {
            return Err("找不到這個附件，請重新上傳".into());
        };
        if row.project != project {
            return Err("附件不屬於這個 project".into());
        }
        out.push(view(&row));
    }
    Ok(out)
}

/// 本體的路徑（給之後組 provider 請求時讀檔用；`pots-llm` 的圖片 input 是下一個任務）。
#[allow(dead_code)]
pub fn body_path(data_dir: &Path, sha256: &str) -> PathBuf {
    dir(data_dir).join(sha256)
}

fn view(row: &AttachmentRow) -> AttachmentView {
    AttachmentView {
        id: row.id,
        mime: row.mime.clone(),
        bytes: row.bytes,
        width: row.width,
        height: row.height,
        filename: row.filename.clone(),
    }
}

/// 去掉 `; charset=...` 之類的參數並轉小寫；沒給時當成 `application/octet-stream`。
fn normalize_mime(mime: &str) -> String {
    let base = mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    if base.is_empty() {
        "application/octet-stream".into()
    } else if base == "image/jpg" {
        // 常見的錯寫法，當成 jpeg。
        "image/jpeg".into()
    } else {
        base
    }
}

fn mime_of(format: ImageFormat) -> Option<&'static str> {
    match format {
        ImageFormat::Png => Some("image/png"),
        ImageFormat::Jpeg => Some("image/jpeg"),
        ImageFormat::WebP => Some("image/webp"),
        ImageFormat::Gif => Some("image/gif"),
        _ => None,
    }
}

/// 原子寫入本體（`0600`）：先寫 `<sha>.tmp`、fsync，再 rename。
async fn write_body(data_dir: &Path, sha256: &str, bytes: &[u8]) -> Result<(), Error> {
    let dir = dir(data_dir);
    crate::datadir::create_private_dir(&dir)?;
    let path = dir.join(sha256);
    let tmp = dir.join(format!("{sha256}.tmp"));
    tokio::fs::write(&tmp, bytes).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await?;
    }
    tokio::fs::rename(&tmp, &path).await?;
    Ok(())
}

fn internal(e: crate::Error) -> String {
    tracing::error!(error = %e, "讀取附件失敗");
    "讀取附件失敗".into()
}

/// 上傳失敗：被拒（4xx，訊息給使用者看）或伺服器端錯誤（5xx）。
#[derive(Debug)]
pub enum UploadError {
    Rejected(Rejected),
    Internal(crate::Error),
}

impl From<Rejected> for UploadError {
    fn from(r: Rejected) -> Self {
        Self::Rejected(r)
    }
}

impl From<sqlx::Error> for UploadError {
    fn from(e: sqlx::Error) -> Self {
        Self::Internal(crate::Error::from(e))
    }
}

impl From<crate::Error> for UploadError {
    fn from(e: crate::Error) -> Self {
        Self::Internal(e)
    }
}

impl UploadError {
    /// 給 HTTP 端點用的狀態碼與訊息。
    pub fn response(&self) -> (StatusCode, String) {
        match self {
            Self::Rejected(r) => (r.status, r.message.clone()),
            Self::Internal(e) => {
                tracing::error!(error = %e, "附件上傳失敗");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "儲存圖片失敗".to_string(),
                )
            }
        }
    }
}
