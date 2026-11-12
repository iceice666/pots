//! 圖片附件的內容來源：把歷史裡的 [`Item::Image`](pots_llm::Item::Image) 讀成本體的 base64。
//!
//! 歷史（`om_raw`）只存附件參照（attachment id 與 MIME），base64 在 provider 送出請求的
//! 那一刻才由這裡從 `<data_dir>/attachments/<sha256>` 讀出來（見 [`pots_llm::image`]）：
//! 歷史每一輪整份重送，base64 放進資料庫、事件日誌或 `stream_items` 都會爆量。
//!
//! 讀不到附件（id 不存在、檔案被刪、資料庫出錯）時回看得懂的中文 [`LlmError::Other`]：整個
//! 請求失敗，不會默默送出一份少了圖的歷史；這裡也**不 panic**，provider 的錯誤處理路徑照舊
//! 運作（[`LlmError::UnsupportedImage`] 只留給「模型不支援圖片」）。

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use pots_llm::LlmError;
use pots_llm::image::{ImageLoader, base64_bytes};
use pots_proto::AttachmentId;

use crate::attachments;
use crate::store::Store;

/// 之後才填好的圖片來源。
///
/// 組裝端建立 ChatGPT provider 與 [`HttpApiProviders`](crate::service::HttpApiProviders) 時還
/// 沒有資料庫（`service::chatgpt_deps` 只拿得到資料目錄），只能先給一個空的槽；
/// [`start`](crate::start) 開好資料庫之後把 [`StoreImages`] 填進去。provider 建好之後仍共用
/// 同一個槽，所以填進去的來源就是正式推論路徑用的來源（見 [`Deps::images`](crate::Deps::images)）。
///
/// 沒有被填時回「沒有設定圖片來源」：歷史裡沒有圖片的一般對話不受影響，有圖片則整個請求
/// 失敗，不會默默送出一份少了圖的歷史。
#[derive(Clone, Default)]
pub struct ImageSlot {
    loader: Arc<OnceLock<Arc<dyn ImageLoader>>>,
}

impl ImageSlot {
    pub fn new() -> Self {
        Self::default()
    }

    /// 填上真正的來源。只填一次，已經填過時保留原本的（重複啟動不會換掉正在用的）。
    pub fn fill(&self, loader: Arc<dyn ImageLoader>) {
        if self.loader.set(loader).is_err() {
            tracing::debug!("圖片來源已經填過，保留原本的");
        }
    }

    /// 目前填上的來源；還沒填時是 `None`。
    pub fn get(&self) -> Option<Arc<dyn ImageLoader>> {
        self.loader.get().cloned()
    }
}

#[async_trait::async_trait]
impl ImageLoader for ImageSlot {
    async fn base64(&self, attachment_id: &str) -> Result<String, LlmError> {
        match self.get() {
            Some(loader) => loader.base64(attachment_id).await,
            None => Err(LlmError::Unsupported(
                "這個 provider 沒有設定圖片來源，無法送出圖片".into(),
            )),
        }
    }
}

/// 從資料目錄讀附件的 [`ImageLoader`]：attachment id → `attachments` 表的中介資料 →
/// `<data_dir>/attachments/<sha256>` → base64。
#[derive(Clone)]
pub struct StoreImages {
    store: Store,
    data_dir: PathBuf,
}

impl StoreImages {
    pub fn new(store: Store, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            store,
            data_dir: data_dir.into(),
        }
    }
}

#[async_trait::async_trait]
impl ImageLoader for StoreImages {
    async fn base64(&self, attachment_id: &str) -> Result<String, LlmError> {
        let id: AttachmentId = attachment_id
            .parse()
            .map_err(|_| LlmError::Other(format!("圖片附件的 ID 格式不對：「{attachment_id}」")))?;
        let row = {
            let mut conn = self.store.acquire().await.map_err(|e| {
                tracing::error!(error = %e, "讀取圖片附件時無法取得資料庫連線");
                LlmError::Other("讀取圖片附件失敗".into())
            })?;
            Store::attachment(&mut conn, id).await.map_err(|e| {
                tracing::error!(error = %e, attachment = %attachment_id, "查詢圖片附件失敗");
                LlmError::Other("讀取圖片附件失敗".into())
            })?
        };
        let Some(row) = row else {
            return Err(LlmError::Other(format!(
                "找不到圖片附件「{attachment_id}」，請重新上傳"
            )));
        };
        let path = attachments::body_path(&self.data_dir, &row.sha256);
        let bytes = tokio::fs::read(&path).await.map_err(|e| {
            tracing::error!(
                error = %e,
                path = %path.display(),
                attachment = %attachment_id,
                "讀不到圖片附件的內容"
            );
            LlmError::Other(format!(
                "讀不到圖片附件的內容（附件「{attachment_id}」），請重新上傳"
            ))
        })?;
        Ok(base64_bytes(&bytes))
    }
}

#[cfg(test)]
mod tests {
    use pots_proto::{ProjectId, Timestamp};
    use sha2::{Digest as _, Sha256};

    use super::*;
    use crate::store::{AttachmentRow, ProjectRow};

    const PROJECT: ProjectId = ProjectId::from_u64(7);

    /// 一張最小的附件：寫檔案 + `attachments` 一列（`attachments.project_id` 指向它）。
    async fn attachment(store: &Store, dir: &std::path::Path, bytes: &[u8]) -> AttachmentId {
        let sha256 = hex::encode(Sha256::digest(bytes));
        std::fs::create_dir_all(attachments::dir(dir)).unwrap();
        std::fs::write(attachments::body_path(dir, &sha256), bytes).unwrap();
        let row = AttachmentRow {
            id: AttachmentId::new(),
            project: PROJECT,
            sha256,
            mime: "image/png".into(),
            bytes: bytes.len() as u64,
            width: None,
            height: None,
            filename: None,
            created_at: Timestamp::now(),
        };
        let mut tx = store.begin().await.unwrap();
        Store::insert_attachment(&mut tx, &row).await.unwrap();
        tx.commit().await.unwrap();
        row.id
    }

    async fn store(dir: &std::path::Path) -> Store {
        let store = Store::open_sqlite(&dir.join("pots.db")).await.unwrap();
        let project = ProjectRow {
            id: PROJECT,
            name: "圖片".into(),
            instructions: String::new(),
            agent_name: "Pots".into(),
            workdir: None,
            created_at: Timestamp::now(),
        };
        let mut tx = store.begin().await.unwrap();
        Store::insert_project(&mut tx, &project).await.unwrap();
        tx.commit().await.unwrap();
        store
    }

    #[tokio::test]
    async fn reads_the_body_and_returns_base64() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path()).await;
        let bytes = b"\x89PNG\r\n\x1a\nfake png body";
        let id = attachment(&store, tmp.path(), bytes).await;

        let images = StoreImages::new(store.clone(), tmp.path());
        assert_eq!(
            images.base64(&id.to_string()).await.unwrap(),
            base64_bytes(bytes)
        );

        // 同一個附件讀兩次結果一樣（每次都真的讀檔，不快取跨請求的內容）。
        assert_eq!(
            images.base64(&id.to_string()).await.unwrap(),
            base64_bytes(bytes)
        );
    }

    #[tokio::test]
    async fn slot_without_a_loader_says_so_without_panicking() {
        let slot = ImageSlot::new();
        assert!(slot.get().is_none());
        let error = slot.base64("anything").await.unwrap_err();
        assert!(
            matches!(&error, LlmError::Unsupported(m) if m.contains("沒有設定圖片來源")),
            "{error:?}"
        );

        // 填上來源之後走真正的讀檔，而且重複填不會換掉已經在用的。
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path()).await;
        let id = attachment(&store, tmp.path(), b"png").await;
        slot.fill(Arc::new(StoreImages::new(store, tmp.path())));
        assert_eq!(
            slot.base64(&id.to_string()).await.unwrap(),
            base64_bytes(b"png")
        );
        slot.fill(Arc::new(pots_llm::image::FnImages::new(
            |_: String| async move { Ok("之後填的不會生效".into()) },
        )));
        assert_eq!(
            slot.base64(&id.to_string()).await.unwrap(),
            base64_bytes(b"png")
        );
    }

    #[tokio::test]
    async fn unknown_id_and_missing_file_are_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path()).await;
        let images = StoreImages::new(store.clone(), tmp.path());

        // 不是合法的 ID：看得懂的訊息，不是 panic。
        let error = images.base64("not-an-id").await.unwrap_err();
        assert!(
            matches!(&error, LlmError::Other(m) if m.contains("ID 格式不對")),
            "{error:?}"
        );

        // 格式對但資料庫沒有這一列。
        let error = images
            .base64(&AttachmentId::new().to_string())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, LlmError::Other(m) if m.contains("找不到圖片附件")),
            "{error:?}"
        );

        // 資料庫有列但檔案不見了（例如被手動刪掉）：仍然是 Err。
        let id = attachment(&store, tmp.path(), b"png").await;
        std::fs::remove_dir_all(attachments::dir(tmp.path())).unwrap();
        let error = images.base64(&id.to_string()).await.unwrap_err();
        assert!(
            matches!(&error, LlmError::Other(m) if m.contains("讀不到圖片附件的內容")),
            "{error:?}"
        );
    }
}
