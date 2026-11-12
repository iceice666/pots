//! 主對話輸入框的待送圖片（貼上、之後的拖放與選檔都走這裡）。
//!
//! 三個入口都只做一件事：把「位元組 + 檔名」丟給 [`PendingAttachments::accept_bytes`]，
//! 由它做大小、格式、張數驗證，BMP/TIFF 轉成 PNG，然後排一個上傳工作出去。
//! 上傳是非同步的（[`crate::connection::Uploads`]），狀態記在每一張圖片上：
//! [`UploadState::Uploading`] → [`UploadState::Uploaded`]（帶伺服器回的中介資料）或
//! [`UploadState::Failed`]（伺服器回的中文原因原文）。
//!
//! 為什麼 BMP/TIFF 要先轉檔：macOS 的截圖幾乎都是 TIFF、Windows 的螢幕剪取是
//! `CF_DIB`（平台層補成 BMP），不轉檔的話「貼上截圖」在 macOS 直接失效。伺服器只收
//! png/jpeg/webp/gif（`pots_proto::ATTACHMENT_MIMES`），所以轉檔在客戶端做。

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui_kit::{ClipboardEntry, ClipboardItem, Image as GpuiImage, ImageFormat as GpuiImageFormat};

use crate::connection::UploadJob;
use crate::model::{AttachmentView, MAX_ATTACHMENT_BYTES, MAX_ATTACHMENTS_PER_MSG};

/// 這裡用到的常數都來自協定，不在客戶端另立一套。
pub const MAX_BYTES: u64 = MAX_ATTACHMENT_BYTES;
pub const MAX_ITEMS: usize = MAX_ATTACHMENTS_PER_MSG;

/// 一張待送圖片現在的狀態。
#[derive(Clone, Debug)]
pub enum UploadState {
    /// 已排進上傳佇列或正在上傳。
    Uploading,
    /// 伺服器收下了，這是它回的中介資料（送出訊息時只帶它的 id）。
    Uploaded(AttachmentView),
    /// 上傳失敗；字串是伺服器回的中文原因原文（或連線問題的說明）。
    Failed(String),
}

/// 待送的一張圖片。
#[derive(Clone)]
pub struct PendingImage {
    /// 客戶端配的號；element id 與上傳結果配對都用它（不是 [`AttachmentView::id`]）。
    pub key: u64,
    /// 使用者原本的檔名；貼上的圖沒有。
    pub filename: Option<String>,
    /// 上傳用的 MIME（BMP/TIFF 已轉成 PNG）。
    pub mime: String,
    /// 上傳用的位元組數（轉檔後）。
    pub bytes: u64,
    /// 預覽用的圖片；`img(ImageSource::Image(..))` 直接畫。
    pub preview: Arc<GpuiImage>,
    pub state: UploadState,
}

impl PendingImage {
    pub fn is_uploaded(&self) -> bool {
        matches!(self.state, UploadState::Uploaded(_))
    }

    /// 上傳失敗的原因；其他狀態是 `None`。
    pub fn failure(&self) -> Option<&str> {
        match &self.state {
            UploadState::Failed(reason) => Some(reason),
            _ => None,
        }
    }
}

/// 輸入框的待送圖片。放在 `Entity` 裡，貼上／上傳回來的結果都經由它更新。
#[derive(Default)]
pub struct PendingAttachments {
    next_key: u64,
    items: Vec<PendingImage>,
    /// 還沒交給上傳執行緒的工作；由 `PotsApp` 取走。
    jobs: Vec<UploadJob>,
    /// 最後一次拒收的原因，顯示在輸入框下方；下一個成功的動作就清掉。
    pub error: Option<String>,
}

/// 待送圖片的唯讀快照：render 期間不必一直借用 `Entity`，畫面照這份畫。
#[derive(Clone, Default)]
pub struct PendingSnapshot {
    pub items: Vec<PendingImage>,
    pub uploading: usize,
    pub error: Option<String>,
}

impl PendingAttachments {
    pub fn new() -> Self {
        Self::default()
    }

    /// 畫面上的那一份（複製成本是幾個 `Arc` 與字串）。
    pub fn snapshot(&self) -> PendingSnapshot {
        PendingSnapshot {
            items: self.items.clone(),
            uploading: self.uploading(),
            error: self.error.clone(),
        }
    }

    pub fn items(&self) -> &[PendingImage] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 還在等上傳結果的張數。
    pub fn uploading(&self) -> usize {
        self.items
            .iter()
            .filter(|i| matches!(i.state, UploadState::Uploading))
            .count()
    }

    /// 有沒有上傳失敗的圖片（送出前要擋下）。
    pub fn failed(&self) -> Option<&str> {
        self.items.iter().find_map(PendingImage::failure)
    }

    /// 送出前的最後檢查：全部上傳完成才回傳要附上的中介資料。
    /// 還有圖片在上傳或失敗時回 `Err`，訊息直接顯示給使用者。
    pub fn ready_views(&self) -> Result<Vec<AttachmentView>, String> {
        if let Some(reason) = self.failed() {
            return Err(format!("有圖片上傳失敗：{reason}（請移除後再送出）"));
        }
        if self.uploading() > 0 {
            return Err("圖片還在上傳中，請稍候再送出".into());
        }
        Ok(self
            .items
            .iter()
            .filter_map(|i| match &i.state {
                UploadState::Uploaded(view) => Some(view.clone()),
                _ => None,
            })
            .collect())
    }

    /// 取走排好的上傳工作。
    pub fn take_jobs(&mut self) -> Vec<UploadJob> {
        std::mem::take(&mut self.jobs)
    }

    /// 上傳結果回來了。圖片已經被移除時（key 找不到）直接忽略。
    pub fn finish(&mut self, key: u64, result: Result<AttachmentView, String>) {
        let Some(item) = self.items.iter_mut().find(|i| i.key == key) else {
            return;
        };
        item.state = match result {
            Ok(view) => UploadState::Uploaded(view),
            Err(reason) => UploadState::Failed(reason),
        };
    }

    /// 假資料模式：沒有服務可以上傳，直接當成上傳完成，畫面上才有縮圖可看。
    pub fn mark_local(&mut self, key: u64, view: AttachmentView) {
        self.finish(key, Ok(view));
    }

    pub fn remove(&mut self, key: u64) {
        self.items.retain(|i| i.key != key);
        if self.items.is_empty() {
            self.error = None;
        }
    }

    /// 訊息送出去之後清空。
    pub fn clear(&mut self) {
        self.items.clear();
        self.jobs.clear();
        self.error = None;
    }

    /// 收下一組位元組：大小 → 格式（必要時轉 PNG）→ 張數。成功回新圖片的 key。
    pub fn accept_bytes(
        &mut self,
        bytes: Vec<u8>,
        filename: Option<String>,
    ) -> Result<u64, String> {
        match self.push_image(bytes, filename) {
            Ok(key) => {
                self.error = None;
                Ok(key)
            }
            Err(e) => Err(self.reject(e)),
        }
    }

    fn push_image(&mut self, bytes: Vec<u8>, filename: Option<String>) -> Result<u64, String> {
        if self.items.len() >= MAX_ITEMS {
            return Err(format!("一則訊息最多 {MAX_ITEMS} 張圖片，請先移除一張"));
        }
        let (mime, bytes) = prepare(bytes)?;
        let format = gpui_format(&mime).ok_or_else(|| format!("不支援的圖片格式：{mime}"))?;
        let key = self.next_key;
        self.next_key += 1;
        self.items.push(PendingImage {
            key,
            filename: filename.clone(),
            mime: mime.clone(),
            bytes: bytes.len() as u64,
            preview: Arc::new(GpuiImage::from_bytes(format, bytes.clone())),
            state: UploadState::Uploading,
        });
        self.jobs.push(UploadJob {
            key,
            mime,
            filename,
            bytes,
        });
        Ok(key)
    }

    /// 拖進來（或選檔選到）的檔案，`files` 是已經在背景讀完的內容。回傳是否有任一張
    /// 被收下；全部被拒時把所有中文原因串起來放在 [`Self::error`]，不只留最後一個。
    pub fn accept_read(&mut self, files: Vec<DroppedFile>) -> bool {
        let mut took = false;
        let mut reasons: Vec<String> = Vec::new();
        for file in files {
            let name = file.name;
            match file.bytes {
                Ok(bytes) => match self.accept_bytes(bytes, Some(name.clone())) {
                    Ok(_) => took = true,
                    // 多選時「哪個檔案被拒」比「為什麼」更難猜，把檔名接在前面。
                    Err(reason) => reasons.push(format!("{name}：{reason}")),
                },
                Err(reason) => reasons.push(reason),
            }
        }
        if !reasons.is_empty() {
            self.error = Some(reasons.join("；"));
        }
        took
    }

    /// 系統檔案對話框開不起來（Linux 缺 `xdg-desktop-portal` 之類）：只留中文提示，
    /// 不動已經在輸入框裡的圖片。使用者取消對話框不算失敗，不會走這裡。
    pub fn note_prompt_failure(&mut self, message: impl Into<String>) {
        self.error = Some(message.into());
    }

    /// 貼上的內容。`true` 代表這次貼上被吃掉了（有圖片被收下），呼叫端不要讓文字插進
    /// 輸入框；全部被拒時回 `false`，讓一般文字貼上照常運作。
    pub fn accept_clipboard(&mut self, item: &ClipboardItem) -> bool {
        let mut took = false;
        for entry in item.entries() {
            match entry {
                ClipboardEntry::Image(image) => {
                    // 剪貼簿給的是格式 + 位元組，跟貼上檔案走同一條驗證。
                    if self.accept_bytes(image.bytes.clone(), None).is_ok() {
                        took = true;
                    }
                }
                ClipboardEntry::ExternalPaths(paths) => {
                    // 複製檔案再貼上（Finder／檔案管理員）：給的是路徑不是位元組。
                    for path in paths.paths() {
                        let name = path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| path.display().to_string());
                        match std::fs::read(path) {
                            Ok(bytes) => {
                                if self.accept_bytes(bytes, Some(name)).is_ok() {
                                    took = true;
                                }
                            }
                            Err(e) => self.error = Some(format!("讀不到 {name}：{e}")),
                        }
                    }
                }
                ClipboardEntry::String(_) => {}
            }
        }
        took
    }

    fn reject(&mut self, message: String) -> String {
        self.error = Some(message.clone());
        message
    }
}

/// 系統的檔案選擇對話框開不起來時給使用者的中文說明；`reason` 是平台回的原因，附在
/// 後面方便回報問題，但前面那句一定要看得懂、而且要講出替代做法。
///
/// Linux 的對話框走 xdg-desktop-portal，缺 portal（或 portal 服務沒在跑）是那裡最常見
/// 的原因，所以只在 Linux 指名它；macOS／Windows 的 `Err` 另有原因，不要亂猜。
pub fn prompt_failure(reason: impl std::fmt::Display) -> String {
    if cfg!(target_os = "linux") {
        format!("系統沒有檔案選擇對話框（缺少 xdg-desktop-portal），請改用拖放或貼上：{reason}")
    } else {
        format!("系統的檔案選擇對話框開不起來，請改用拖放或貼上：{reason}")
    }
}

/// `prompt_for_paths` 那條 oneshot 的結局，翻成呼叫端要做的三件事。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PickOutcome {
    /// 使用者選了這些路徑（保證非空）。
    Paths(Vec<PathBuf>),
    /// 使用者按取消、或回覆管道被丟掉（視窗關了）：不動輸入框、不吵人。
    Cancelled,
    /// 對話框開不起來；字串是給使用者看的中文說明。
    Failed(String),
}

/// 判讀 `prompt_for_paths` 的回覆。抽成純函式是為了測到 headless 平台模擬不出來的
/// 「對話框開不起來」那條分支（`TestPlatform` 只能回「選了」或「取消」）。
///
/// 外層 `Result` 是「回覆管道還在不在」、內層才是對話框的結果。
pub fn pick_outcome<E, C>(result: Result<Result<Option<Vec<PathBuf>>, E>, C>) -> PickOutcome
where
    E: std::fmt::Display,
    C: std::fmt::Display,
{
    match result {
        Ok(Ok(Some(paths))) if !paths.is_empty() => PickOutcome::Paths(paths),
        Ok(Ok(_)) | Err(_) => PickOutcome::Cancelled,
        Ok(Err(reason)) => PickOutcome::Failed(prompt_failure(reason)),
    }
}

/// 拖進來或選進來的一個路徑讀完了：`bytes` 成功是檔案的位元組，失敗是中文原因。
pub struct DroppedFile {
    /// 檔名（或整條路徑，取不到檔名時）；顯示與上傳都用它。
    pub name: String,
    pub bytes: Result<Vec<u8>, String>,
}

/// 讀一個拖進來的路徑。**會阻塞**，呼叫端要在背景執行緒跑（見
/// [`crate::app::PotsApp::accept_dropped`]）。資料夾與讀不到都是中文原因。
pub fn read_dropped(path: &Path) -> DroppedFile {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    if let Ok(meta) = std::fs::metadata(path)
        && meta.is_dir()
    {
        return DroppedFile {
            name: name.clone(),
            bytes: Err(format!("{name} 是資料夾，不是圖片檔")),
        };
    }
    match std::fs::read(path) {
        Ok(bytes) => DroppedFile {
            name,
            bytes: Ok(bytes),
        },
        Err(e) => DroppedFile {
            name: name.clone(),
            bytes: Err(format!("讀不到 {name}：{e}")),
        },
    }
}

/// 驗證位元組並回傳可以上傳的（MIME、位元組）。BMP/TIFF 會轉成 PNG。
fn prepare(bytes: Vec<u8>) -> Result<(String, Vec<u8>), String> {
    if bytes.is_empty() {
        return Err("收到的檔案是空的".into());
    }
    if bytes.len() as u64 > MAX_BYTES {
        return Err(too_large(bytes.len() as u64));
    }
    let format = image::guess_format(&bytes).map_err(|_| "這個檔案不是看得懂的圖片".to_string())?;
    let (mime, bytes) = match format {
        image::ImageFormat::Png => ("image/png".to_string(), bytes),
        image::ImageFormat::Jpeg => ("image/jpeg".to_string(), bytes),
        image::ImageFormat::WebP => ("image/webp".to_string(), bytes),
        image::ImageFormat::Gif => ("image/gif".to_string(), bytes),
        // macOS 截圖（TIFF）與 Windows 螢幕剪取（CF_DIB → BMP）。
        image::ImageFormat::Bmp | image::ImageFormat::Tiff => {
            let png = to_png(&bytes, format)?;
            if png.len() as u64 > MAX_BYTES {
                return Err(too_large(png.len() as u64));
            }
            ("image/png".to_string(), png)
        }
        other => {
            return Err(format!("只支援 png、jpeg、webp、gif；這個檔案是 {other:?}"));
        }
    };
    Ok((mime, bytes))
}

fn to_png(bytes: &[u8], format: image::ImageFormat) -> Result<Vec<u8>, String> {
    let decoded = image::load_from_memory_with_format(bytes, format)
        .map_err(|e| format!("解不開這張圖片：{e}"))?;
    let mut out = Cursor::new(Vec::new());
    decoded
        .write_to(&mut out, image::ImageFormat::Png)
        .map_err(|e| format!("轉成 PNG 失敗：{e}"))?;
    Ok(out.into_inner())
}

fn too_large(bytes: u64) -> String {
    format!(
        "單張圖片上限是 {} MiB，這張是 {:.1} MiB",
        MAX_BYTES / (1024 * 1024),
        bytes as f64 / (1024.0 * 1024.0)
    )
}

fn gpui_format(mime: &str) -> Option<GpuiImageFormat> {
    Some(match mime {
        "image/png" => GpuiImageFormat::Png,
        "image/jpeg" => GpuiImageFormat::Jpeg,
        "image/webp" => GpuiImageFormat::Webp,
        "image/gif" => GpuiImageFormat::Gif,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一張 1×1 的 PNG。
    pub fn tiny_png() -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(1, 1, image::Rgba([255, 0, 0, 255]));
        let mut out = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("寫 PNG");
        out.into_inner()
    }

    fn encode(format: image::ImageFormat) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(2, 3, image::Rgba([0, 128, 255, 255]));
        let mut out = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut out, format)
            .expect("寫圖片");
        out.into_inner()
    }

    #[test]
    fn png_is_accepted_as_is() {
        let mut pending = PendingAttachments::new();
        let png = tiny_png();
        let key = pending.accept_bytes(png.clone(), None).expect("收下");
        let item = &pending.items()[0];
        assert_eq!(key, item.key);
        assert_eq!(item.mime, "image/png");
        assert_eq!(item.bytes, png.len() as u64);
        assert!(matches!(item.state, UploadState::Uploading));
        let jobs = pending.take_jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].mime, "image/png");
        assert_eq!(jobs[0].bytes, png);
    }

    #[test]
    fn tiff_and_bmp_become_png() {
        for format in [image::ImageFormat::Tiff, image::ImageFormat::Bmp] {
            let mut pending = PendingAttachments::new();
            let source = encode(format);
            let key = pending
                .accept_bytes(source.clone(), Some("shot".into()))
                .unwrap_or_else(|e| panic!("{format:?} 應該收下：{e}"));
            let jobs = pending.take_jobs();
            assert_eq!(jobs[0].mime, "image/png", "{format:?} 要轉成 PNG 上傳");
            assert_eq!(jobs[0].key, key);
            assert_eq!(jobs[0].filename.as_deref(), Some("shot"));
            assert_eq!(
                image::guess_format(&jobs[0].bytes).expect("轉出來的東西是圖片"),
                image::ImageFormat::Png
            );
            assert_eq!(pending.items()[0].mime, "image/png");
        }
    }

    #[test]
    fn unsupported_or_broken_input_is_rejected() {
        let mut pending = PendingAttachments::new();
        let err = pending
            .accept_bytes(b"not an image".to_vec(), None)
            .expect_err("不是圖片");
        assert_eq!(err, "這個檔案不是看得懂的圖片");
        assert_eq!(pending.error.as_deref(), Some(err.as_str()));
        assert!(pending.is_empty());
        assert!(pending.take_jobs().is_empty());

        // 看得懂、但不在白名單裡的格式（ICO 的 magic number）。
        let ico = vec![0, 0, 1, 0, 0, 0, 0, 0];
        let err = pending.accept_bytes(ico, None).expect_err("不收 ICO");
        assert!(err.contains("只支援 png、jpeg、webp、gif"), "{err}");

        // 空檔案。
        let err = pending.accept_bytes(Vec::new(), None).expect_err("空檔案");
        assert_eq!(err, "收到的檔案是空的");
    }

    #[test]
    fn size_limit_is_inclusive() {
        let mut pending = PendingAttachments::new();
        // 剛好 10 MiB：大小檢查會過，接著因為不是圖片被拒（證明邊界不是 `>=`）。
        let boundary = vec![0u8; MAX_BYTES as usize];
        let err = pending.accept_bytes(boundary, None).expect_err("不是圖片");
        assert_eq!(err, "這個檔案不是看得懂的圖片");

        // 10 MiB + 1 byte：先用大小擋下來，訊息裡有上限與實際大小。
        let too_big = vec![0u8; MAX_BYTES as usize + 1];
        let err = pending.accept_bytes(too_big, None).expect_err("太大");
        assert!(err.contains("上限是 10 MiB"), "{err}");
        assert!(err.contains("10.0 MiB"), "{err}");
    }

    #[test]
    fn fifth_image_is_rejected() {
        let mut pending = PendingAttachments::new();
        for i in 0..MAX_ITEMS {
            pending.accept_bytes(tiny_png(), None).unwrap_or_else(|e| {
                panic!("第 {} 張應該收下：{e}", i + 1);
            });
        }
        let err = pending
            .accept_bytes(tiny_png(), None)
            .expect_err("第 5 張要拒收");
        assert_eq!(err, "一則訊息最多 4 張圖片，請先移除一張");
        assert_eq!(pending.len(), MAX_ITEMS);
    }

    #[test]
    fn send_is_blocked_until_every_image_is_uploaded() {
        let mut pending = PendingAttachments::new();
        let key = pending.accept_bytes(tiny_png(), None).expect("收下");
        assert_eq!(
            pending.ready_views().expect_err("還在上傳"),
            "圖片還在上傳中，請稍候再送出"
        );

        let view = AttachmentView {
            id: pots_proto::AttachmentId::from_u64(7),
            mime: "image/png".into(),
            bytes: 68,
            width: Some(1),
            height: Some(1),
            filename: None,
        };
        pending.finish(key, Ok(view.clone()));
        assert_eq!(pending.ready_views().expect("可以送了"), vec![view]);

        // 失敗的圖片擋住送出，原因照伺服器原文顯示。
        let key = pending.accept_bytes(tiny_png(), None).expect("收下");
        pending.finish(
            key,
            Err("只支援 png、jpeg、webp、gif；收到的是「image/svg+xml」".into()),
        );
        let err = pending.ready_views().expect_err("失敗要擋");
        assert!(err.contains("image/svg+xml"), "{err}");
    }

    #[test]
    fn read_dropped_reports_folders_and_missing_files_in_chinese() {
        let dir = tempfile::tempdir().expect("tempdir");
        let png = dir.path().join("shot.png");
        std::fs::write(&png, tiny_png()).expect("寫檔");
        let read = read_dropped(&png);
        assert_eq!(read.name, "shot.png");
        assert!(read.bytes.is_ok(), "讀得到的檔案要回位元組");

        let folder = dir.path().join("截圖");
        std::fs::create_dir(&folder).expect("建資料夾");
        let read = read_dropped(&folder);
        assert!(read.bytes.is_err());
        assert!(
            read.bytes.unwrap_err().contains("是資料夾"),
            "資料夾要說清楚"
        );

        let read = read_dropped(&dir.path().join("不存在.png"));
        assert!(read.bytes.unwrap_err().contains("讀不到"), "讀不到要說清楚");
    }

    #[test]
    fn accept_read_keeps_every_reason_not_just_the_last() {
        let mut pending = PendingAttachments::new();
        let took = pending.accept_read(vec![
            DroppedFile {
                name: "ok.png".into(),
                bytes: Ok(tiny_png()),
            },
            DroppedFile {
                name: "notes.txt".into(),
                bytes: Err("讀不到 notes.txt：沒有這個檔案".into()),
            },
            DroppedFile {
                name: "bad.txt".into(),
                bytes: Ok(b"not an image".to_vec()),
            },
        ]);
        assert!(took, "有一張收下就算有接受");
        assert_eq!(pending.len(), 1);
        let error = pending.error.as_deref().unwrap_or_default();
        assert!(error.contains("讀不到 notes.txt"), "{error}");
        assert!(error.contains("不是看得懂的圖片"), "{error}");

        // 全部都成功時把先前的錯誤清掉。
        let took = pending.accept_read(vec![DroppedFile {
            name: "second.png".into(),
            bytes: Ok(tiny_png()),
        }]);
        assert!(took);
        assert!(pending.error.is_none());
    }

    #[test]
    fn pick_outcome_covers_every_ending() {
        let paths = vec![PathBuf::from("/tmp/a.png")];
        assert_eq!(
            pick_outcome::<&str, &str>(Ok(Ok(Some(paths.clone())))),
            PickOutcome::Paths(paths),
            "選了路徑就照路徑走"
        );
        assert_eq!(
            pick_outcome::<&str, &str>(Ok(Ok(None))),
            PickOutcome::Cancelled,
            "按取消不動輸入框"
        );
        assert_eq!(
            pick_outcome::<&str, &str>(Ok(Ok(Some(Vec::new())))),
            PickOutcome::Cancelled,
            "空清單等於沒選"
        );
        assert_eq!(
            pick_outcome::<&str, &str>(Err("receiver dropped")),
            PickOutcome::Cancelled,
            "回覆管道被丟掉（視窗關了）當成取消"
        );
        match pick_outcome::<&str, &str>(Ok(Err("no dbus session"))) {
            PickOutcome::Failed(message) => {
                assert!(message.contains("請改用拖放或貼上"), "{message}");
                assert!(
                    message.contains("no dbus session"),
                    "要留平台原文：{message}"
                );
                if cfg!(target_os = "linux") {
                    assert!(message.contains("xdg-desktop-portal"), "{message}");
                }
            }
            other => panic!("對話框開不起來要回 Failed，得到 {other:?}"),
        }
    }

    #[test]
    fn a_missing_dialog_only_sets_the_error() {
        let mut pending = PendingAttachments::new();
        pending.accept_bytes(tiny_png(), None).expect("先收一張");
        pending.note_prompt_failure(prompt_failure("沒有 dbus"));
        assert_eq!(pending.len(), 1, "對話框開不起來不該動到既有的圖片");
        let error = pending.error.as_deref().unwrap_or_default();
        assert!(error.contains("系統沒有檔案選擇對話框"), "{error}");
    }

    #[test]
    fn removed_keys_are_ignored_when_the_upload_finishes() {
        let mut pending = PendingAttachments::new();
        let key = pending.accept_bytes(tiny_png(), None).expect("收下");
        pending.remove(key);
        assert!(pending.is_empty());
        // 上傳執行緒還是會回報結果；圖片已經不在了，不要 panic 也不要長回來。
        pending.finish(key, Err("太慢".into()));
        assert!(pending.is_empty());
    }
}
