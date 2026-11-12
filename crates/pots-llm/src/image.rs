//! 圖片歷史項目送出請求時的資料來源。
//!
//! [`Item::Image`](crate::Item::Image) 只存附件參照（attachment id 與 MIME）：base64 在組請求的
//! 那一刻才從本機檔案讀出來，**不進資料庫、不進 `om_raw`、也不進 `stream_items`**。歷史每一輪
//! 都整份重送，base64 放進去會讓儲存與請求一起爆量。
//!
//! 一般的接法：[`ImageLoader`] 由呼叫端實作（讀附件目錄），provider 用
//! `with_images` 掛上去，三個 body 產生器在序列化時查 [`ImagePayloads`]。
//! 沒有掛 loader 但歷史裡有圖片時，provider 直接回錯誤，不會默默把使用者的圖丟掉。

use std::path::{Path, PathBuf};

use crate::provider::{Item, LlmError, ModelRequest};

/// 這次請求要送的圖片：attachment id → base64（不含 data URL 前綴）。
pub type ImagePayloads = std::collections::BTreeMap<String, String>;

/// 不會看圖的地方（Observational Memory 的對話紀錄、token 估算、沒有圖片來源時的降級）
/// 用的佔位符。
pub const IMAGE_PLACEHOLDER: &str = "[圖片]";

/// 模型不支援圖片輸入時給使用者看的話。
pub const UNSUPPORTED_MESSAGE: &str = "目前選用的模型不支援圖片輸入";

/// 這次請求有沒有圖片項目。
pub fn has_images(items: &[Item]) -> bool {
    items.iter().any(|item| matches!(item, Item::Image { .. }))
}

/// 依這個請求的歷史產生 [`ImagePayloads`]。
///
/// 沒有掛 loader 時：歷史裡沒有圖片就回空的（一般文字對話不受影響）；有圖片時回
/// [`LlmError::Unsupported`]，不會默默把圖丟掉送出一個少了圖的請求。
pub fn payloads_for(
    loader: Option<&dyn ImageLoader>,
    items: &[Item],
) -> Result<ImagePayloads, LlmError> {
    match loader {
        Some(loader) => payloads(loader, items),
        None if has_images(items) => Err(LlmError::Unsupported(
            "這個 provider 沒有設定圖片來源，無法送出圖片".into(),
        )),
        None => Ok(ImagePayloads::new()),
    }
}

/// provider 回報「看不懂圖片」時，換成給使用者看的中文說明。
///
/// 只在這個請求真的帶圖片時才轉譯；比對的是 provider 的錯誤字串（各家寫法不同，見
/// [`looks_like_unsupported_image`]）。其他錯誤原樣回傳。
pub fn explain_unsupported(error: LlmError, request: &ModelRequest) -> LlmError {
    if !has_images(&request.input) {
        return error;
    }
    let text = match &error {
        LlmError::Unsupported(message) | LlmError::Other(message) => message,
        _ => return error,
    };
    if looks_like_unsupported_image(text) {
        LlmError::UnsupportedImage
    } else {
        error
    }
}

/// 錯誤訊息像不像「這個模型不接受圖片」。規則保守：一定要提到 image，再配上拒絕的字眼。///
/// 已知的寫法（未逐一實測，之後遇到新的再補）：
/// - OpenAI：「Invalid content type. image_url is only supported by certain models.」
/// - OpenAI Responses：「The model does not support image input.」
/// - Anthropic：「this model does not support image input」
/// - 相容服務（Ollama 等）：「does not support images」、「unknown variant `image_url`」
pub fn looks_like_unsupported_image(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    if !message.contains("image") {
        return false;
    }
    [
        "not supported",
        "does not support",
        "doesn't support",
        "unsupported",
        "only supported",
        "unknown variant",
        "invalid content type",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

/// `data:{mime};base64,{base64}`。Chat Completions 與 Responses 的 `image_url` 直接收 data URL。
pub fn data_url(mime: &str, base64: &str) -> String {
    format!("data:{};base64,{base64}", normalize_mime(mime))
}

/// MIME 正規化：拿掉參數（`image/png; charset=binary`）並把 `image/jpg` 換成 `image/jpeg`
/// （Anthropic 只認 `jpeg`）。認不得的型別原樣回傳，由 provider 回報錯誤。
pub fn normalize_mime(mime: &str) -> &str {
    let mime = mime.split(';').next().unwrap_or(mime).trim();
    match mime.to_ascii_lowercase().as_str() {
        "image/jpg" => "image/jpeg",
        _ => mime,
    }
}

/// 把 [`Item::Image`] 的參照變成 base64。實作由呼叫端提供（讀本機附件檔）。
///
/// 這是同步介面：provider 在組請求時呼叫，讀的是本機小檔案（單張上限 10 MiB），
/// 和後面把 base64 序列化成 JSON 的成本同一個量級。
pub trait ImageLoader: Send + Sync {
    /// `attachment_id` 的 base64；讀不到時回錯誤，整個請求不要送出。
    fn base64(&self, attachment_id: &str) -> Result<String, LlmError>;
}

/// 依歷史順序把圖片讀成 [`ImagePayloads`]；同一個附件只讀一次。
pub fn payloads(loader: &dyn ImageLoader, items: &[Item]) -> Result<ImagePayloads, LlmError> {
    let mut payloads = ImagePayloads::new();
    for item in items {
        let Item::Image { attachment_id, .. } = item else {
            continue;
        };
        if payloads.contains_key(attachment_id) {
            continue;
        }
        payloads.insert(attachment_id.clone(), loader.base64(attachment_id)?);
    }
    Ok(payloads)
}

/// 取一個 [`Item::Image`] 的 base64；`images` 少了它時回 [`LlmError::Unsupported`]。
pub fn payload<'a>(images: &'a ImagePayloads, attachment_id: &str) -> Result<&'a str, LlmError> {
    images
        .get(attachment_id)
        .map(String::as_str)
        .ok_or_else(|| {
            LlmError::Unsupported(format!(
                "缺少圖片附件「{attachment_id}」的內容，無法送出請求"
            ))
        })
}

/// 讀 `{dir}/{attachment_id}` 的原始位元組並轉成 base64。
///
/// 檔案以 attachment id 命名；附件目錄的位置與命名由伺服器的附件儲存決定
/// （`pots-server` 的 `POST /attachments`），layout 不同時自己實作 [`ImageLoader`]。
#[derive(Clone, Debug)]
pub struct DirectoryImages {
    dir: PathBuf,
}

impl DirectoryImages {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl ImageLoader for DirectoryImages {
    fn base64(&self, attachment_id: &str) -> Result<String, LlmError> {
        use base64::Engine as _;
        let path = self.dir.join(attachment_id);
        let bytes = std::fs::read(&path).map_err(|error| {
            LlmError::Unsupported(format!("讀不到圖片附件「{attachment_id}」：{error}"))
        })?;
        Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_is_normalized() {
        assert_eq!(normalize_mime("image/jpg"), "image/jpeg");
        assert_eq!(normalize_mime("image/PNG; charset=binary"), "image/PNG");
        assert_eq!(
            data_url("image/png", "AAA"),
            "data:image/png;base64,AAA".to_string()
        );
    }

    #[test]
    fn directory_loader_reads_by_attachment_id() {
        let dir = tempfile_dir();
        std::fs::write(dir.join("att-1"), b"png").unwrap();
        let loader = DirectoryImages::new(dir.clone());
        use base64::Engine as _;
        assert_eq!(
            loader.base64("att-1").unwrap(),
            base64::engine::general_purpose::STANDARD.encode(b"png")
        );
        assert!(matches!(
            loader.base64("missing"),
            Err(LlmError::Unsupported(m)) if m.contains("讀不到圖片附件")
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn payloads_deduplicate_and_only_read_images() {
        struct Fixed;
        impl ImageLoader for Fixed {
            fn base64(&self, id: &str) -> Result<String, LlmError> {
                Ok(format!("<{id}>"))
            }
        }
        let items = [
            Item::User {
                text: "看這張".into(),
            },
            Item::Image {
                attachment_id: "a".into(),
                mime: "image/png".into(),
            },
            Item::Image {
                attachment_id: "a".into(),
                mime: "image/png".into(),
            },
            Item::Image {
                attachment_id: "b".into(),
                mime: "image/jpeg".into(),
            },
        ];
        let map = payloads(&Fixed, &items).unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map["a"], "<a>");
        assert_eq!(map["b"], "<b>");
        assert!(payloads(&Fixed, &items[..1]).unwrap().is_empty());
    }

    /// 測試用的暫存目錄，不引入額外依賴。
    fn tempfile_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pots-images-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
