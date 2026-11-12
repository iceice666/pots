//! 訊息附圖的取圖與快取（`GET /attachments/{id}` → [`GpuiImage`]）。
//!
//! 訊息只帶中介資料（[`AttachmentView`]），本體存在服務的資料目錄裡；要畫縮圖或大圖
//! 得先把位元組抓回來。這裡做兩件事：
//!
//! 1. [`Fetcher`]：抓圖的執行緒。`reqwest` 需要 tokio 的 reactor，GPUI 的背景 executor
//!    不是 tokio，所以照 [`crate::connection::Uploads`] 的做法自建執行緒＋current-thread
//!    runtime；與連線、上傳分開，抓大圖不會卡住 WebSocket。
//! 2. [`AttachmentImages`]：`附件 id → Loading / Ready / Failed` 的快取。渲染只讀這份
//!    快取（畫的過程中不送請求），要抓哪些圖由 `PotsApp::request_attachment_images`
//!    在每次 render 開頭排定。
//!
//! 沒有連線目標時（假資料模式、還沒收到 `ConnEvent::Target`）不送請求，畫面上只有佔位。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use gpui_kit::{Image as GpuiImage, ImageFormat as GpuiImageFormat};

use crate::connection::Target;
use crate::model::{AttachmentId, AttachmentView, Msg, StreamItemKind};

/// 一張附圖現在能畫什麼。
#[derive(Clone, Debug)]
pub enum ImageState {
    /// 已經送出請求，等位元組回來。
    Loading,
    /// 位元組回來了，`img(ImageSource::Image(..))` 直接畫。
    Ready(Arc<GpuiImage>),
    /// 取不到；字串是服務回的中文原因原文（或連線問題的說明）。可以重試。
    Failed(String),
}

/// 一張要抓的圖。
#[derive(Clone, Debug)]
pub struct FetchJob {
    pub id: AttachmentId,
    /// 伺服器回的 `Content-Type` 就是這個 MIME；解碼時要它才知道格式。
    pub mime: String,
}

/// 抓圖的結果：`(附件 id, MIME, 位元組或中文原因)`。
pub type FetchResult = (AttachmentId, String, Result<Vec<u8>, String>);

/// 抓圖的執行緒把手。丟掉它執行緒就會結束（與 [`crate::connection::Uploads`] 一樣）。
#[derive(Clone)]
pub struct Fetcher {
    jobs: mpsc::UnboundedSender<(Target, FetchJob)>,
}

impl Fetcher {
    pub fn fetch(&self, target: Target, job: FetchJob) {
        let _ = self.jobs.send((target, job));
    }
}

/// 附圖的快取。放在 [`crate::app::PotsApp`] 上，單一 UI 執行緒存取。
#[derive(Default)]
pub struct AttachmentImages {
    entries: HashMap<AttachmentId, ImageState>,
    /// 假資料模式沒有服務可以取圖，就沒有 fetcher。
    fetcher: Option<Fetcher>,
}

impl AttachmentImages {
    /// 不取圖的快取（假資料模式、測試）：只記錄狀態，`get_or_fetch` 一律回 `None`。
    pub fn new() -> Self {
        Self::default()
    }

    /// 真的會取圖的快取：自己的執行緒＋tokio runtime，結果由呼叫端的 GPUI task 讀。
    pub fn spawn() -> (Self, futures::channel::mpsc::UnboundedReceiver<FetchResult>) {
        let (jobs, mut job_rx) = mpsc::unbounded_channel::<(Target, FetchJob)>();
        let (res_tx, res_rx) = futures::channel::mpsc::unbounded();
        std::thread::Builder::new()
            .name("pots-attachment-images".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = res_tx.unbounded_send((
                            AttachmentId::new(),
                            String::new(),
                            Err(format!("無法啟動取圖：{e}")),
                        ));
                        return;
                    }
                };
                runtime.block_on(async move {
                    let client = match reqwest::Client::builder()
                        .timeout(Duration::from_secs(60))
                        .build()
                    {
                        Ok(client) => client,
                        Err(e) => {
                            let _ = res_tx.unbounded_send((
                                AttachmentId::new(),
                                String::new(),
                                Err(format!("無法建立 HTTP client：{e}")),
                            ));
                            return;
                        }
                    };
                    while let Some((target, job)) = job_rx.recv().await {
                        let mime = job.mime.clone();
                        let id = job.id;
                        let result = fetch_one(&client, &target, job).await;
                        if res_tx.unbounded_send((id, mime, result)).is_err() {
                            return;
                        }
                    }
                });
            })
            .expect("無法建立取圖執行緒");
        let images = Self {
            entries: HashMap::new(),
            fetcher: Some(Fetcher { jobs }),
        };
        (images, res_rx)
    }

    pub fn has_fetcher(&self) -> bool {
        self.fetcher.is_some()
    }

    /// 不啟執行緒的快取，回傳 fetcher 的收件端：測試可以直接看排了哪些工作，不必真的
    /// 連服務（integration test 也用它）。
    pub fn with_fake_fetcher() -> (
        Self,
        mpsc::UnboundedReceiver<(crate::connection::Target, FetchJob)>,
    ) {
        let (jobs, job_rx) = mpsc::unbounded_channel();
        (
            Self {
                entries: HashMap::new(),
                fetcher: Some(Fetcher { jobs }),
            },
            job_rx,
        )
    }

    /// 這張圖現在能畫什麼；`None` 是還沒抓過（也沒有請求在跑）。
    pub fn state(&self, id: &AttachmentId) -> Option<&ImageState> {
        self.entries.get(id)
    }

    /// 快取裡沒有這張圖就排一個取圖工作。`target` 或 fetcher 缺一個就不排、回 `None`
    /// （假資料模式、連線還沒好）。已經在抓或已經失敗的不重排——失敗要用
    /// [`Self::retry`] 才會再抓一次，否則每一帧都會重送同一個請求。
    pub fn get_or_fetch(
        &mut self,
        id: AttachmentId,
        mime: &str,
        target: Option<&Target>,
    ) -> Option<&ImageState> {
        if self.entries.contains_key(&id) {
            return self.entries.get(&id);
        }
        let (Some(target), Some(fetcher)) = (target, self.fetcher.as_ref()) else {
            return None;
        };
        fetcher.fetch(
            target.clone(),
            FetchJob {
                id,
                mime: mime.to_string(),
            },
        );
        self.entries.entry(id).or_insert(ImageState::Loading);
        self.entries.get(&id)
    }

    /// 失敗的那張圖重新抓一次（使用者點「無法載入」的縮圖時）。
    pub fn retry(&mut self, id: AttachmentId) {
        self.entries.remove(&id);
    }

    /// 取圖結果回來了：解碼成 [`GpuiImage`] 放進快取。
    pub fn finish(&mut self, id: AttachmentId, mime: &str, result: Result<Vec<u8>, String>) {
        let state = match result {
            Ok(bytes) => match format_for_mime(mime) {
                Some(format) => ImageState::Ready(Arc::new(GpuiImage::from_bytes(format, bytes))),
                None => ImageState::Failed(format!("不支援的圖片格式：{mime}")),
            },
            Err(reason) => ImageState::Failed(reason),
        };
        self.entries.insert(id, state);
    }
}

/// `GET /attachments/{id}`：成功回位元組，其他一律回中文原因（伺服器的錯誤本來就是
/// 純文字中文，照顯示）。守衛與上傳相同：`Host` 要是 loopback、帶
/// `Authorization: Bearer <token>`、**不能送 `Origin`**（`reqwest` 不會自己送）。
async fn fetch_one(
    client: &reqwest::Client,
    target: &Target,
    job: FetchJob,
) -> Result<Vec<u8>, String> {
    let url = format!("http://{}/attachments/{}", target.addr, job.id);
    let response = client
        .get(&url)
        .bearer_auth(&target.token)
        .send()
        .await
        .map_err(|e| format!("連不上服務：{e}"))?;
    let status = response.status();
    if status == reqwest::StatusCode::OK {
        return response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| format!("讀不到這張圖片：{e}"));
    }
    let message = response.text().await.unwrap_or_else(|_| status.to_string());
    Err(if message.trim().is_empty() {
        format!("取不到這張圖片（HTTP {}）", status.as_u16())
    } else {
        message.trim().to_string()
    })
}

/// 四種能畫的 MIME → GPUI 的格式；其他一律 `None`（伺服器只收四種，正常不會遇到）。
pub fn format_for_mime(mime: &str) -> Option<GpuiImageFormat> {
    Some(match mime {
        "image/png" => GpuiImageFormat::Png,
        "image/jpeg" => GpuiImageFormat::Jpeg,
        "image/webp" => GpuiImageFormat::Webp,
        "image/gif" => GpuiImageFormat::Gif,
        _ => return None,
    })
}

/// 訊息要畫的圖片。沒有附件時是 `None`：呼叫端就不會多畫一層空的容器。
pub fn message_images(msg: &Msg) -> Option<&[AttachmentView]> {
    (!msg.attachments.is_empty()).then_some(msg.attachments.as_slice())
}

/// 目前畫面上可能出現的所有附圖（`(id, mime)`，已去重）。
///
/// 一次走完整份 `Store`：訊息可能出現在 project 頻道、總管的對話、任務 thread 與
/// Goal 留言，逐處登記會漏。每帧走一遍只是幾個 HashMap 查詢的成本，換來「只要畫得到
/// 就一定抓得到」。
pub fn wanted_images(store: &crate::model::Store) -> Vec<(AttachmentId, String)> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut push = |msg: &Msg| {
        for a in &msg.attachments {
            if seen.insert(a.id) {
                out.push((a.id, a.mime.clone()));
            }
        }
    };
    for instance in &store.instances {
        for item in &instance.overseer.items {
            if let StreamItemKind::Human(msg) | StreamItemKind::Agent(msg) = &item.kind {
                push(msg);
            }
        }
        for project in &instance.projects {
            for item in &project.items {
                if let StreamItemKind::Human(msg) | StreamItemKind::Agent(msg) = &item.kind {
                    push(msg);
                }
            }
            for goal in &project.goals {
                for task in &goal.tasks {
                    for item in &task.thread {
                        if let StreamItemKind::Human(msg) | StreamItemKind::Agent(msg) = &item.kind
                        {
                            push(msg);
                        }
                    }
                }
            }
        }
    }
    out
}

/// 縮圖元素的 id。同一則訊息可能有多張圖，同一個畫面也可能同時出現兩則訊息、
/// 甚至同一則訊息在頻道與右欄 thread 各畫一次，所以 id 帶上訊息的 scope 與序號
/// （慣例見 `render_card`：`msg-{item}-{index}`）。
pub fn thumb_element_id(item: impl std::fmt::Display, index: usize) -> String {
    format!("msg-image-{item}-{index}")
}

/// 縮圖的邊長（像素）。`max` 是上限：寬高比不同時等比縮進去，至少 [`THUMB_MIN`]。
pub const THUMB_MAX_W: f32 = 168.;
pub const THUMB_MAX_H: f32 = 120.;
const THUMB_MIN: f32 = 48.;

/// 縮圖依原圖的寬高比縮進 [`THUMB_MAX_W`]×[`THUMB_MAX_H`] 的框；中介資料沒有寬高
/// （上傳端解不出來時）就退回正方形。純函式：無 GPU 也能測。
pub fn thumb_box(image: &AttachmentView) -> (f32, f32) {
    let (Some(w), Some(h)) = (image.width, image.height) else {
        return (THUMB_MAX_H, THUMB_MAX_H);
    };
    if w == 0 || h == 0 {
        return (THUMB_MAX_H, THUMB_MAX_H);
    }
    let (w, h) = (w as f32, h as f32);
    // 等比縮進框裡；太窄的一端（超長條圖）墊到 `THUMB_MIN`，免得縮圖變成一條線。
    let scale = (THUMB_MAX_W / w).min(THUMB_MAX_H / h);
    (
        (w * scale).clamp(THUMB_MIN, THUMB_MAX_W),
        (h * scale).clamp(THUMB_MIN, THUMB_MAX_H),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(id: u64, mime: &str, width: Option<u32>, height: Option<u32>) -> AttachmentView {
        AttachmentView {
            id: AttachmentId::from_u64(id),
            mime: mime.into(),
            bytes: 4,
            width,
            height,
            filename: None,
        }
    }

    /// 不啟執行緒的 fetcher：測試直接看排了哪些工作。
    fn fake() -> (
        AttachmentImages,
        mpsc::UnboundedReceiver<(Target, FetchJob)>,
    ) {
        AttachmentImages::with_fake_fetcher()
    }

    fn target() -> Target {
        Target {
            addr: "127.0.0.1:9".parse().expect("位址"),
            token: "t".into(),
        }
    }

    #[test]
    fn format_for_mime_covers_the_four_accepted_types() {
        assert_eq!(format_for_mime("image/png"), Some(GpuiImageFormat::Png));
        assert_eq!(format_for_mime("image/jpeg"), Some(GpuiImageFormat::Jpeg));
        assert_eq!(format_for_mime("image/webp"), Some(GpuiImageFormat::Webp));
        assert_eq!(format_for_mime("image/gif"), Some(GpuiImageFormat::Gif));
        assert_eq!(format_for_mime("image/tiff"), None);
        assert_eq!(format_for_mime("application/pdf"), None);
    }

    #[test]
    fn cache_moves_from_loading_to_ready_and_failed_can_be_retried() {
        let (mut images, mut jobs) = fake();
        let id = AttachmentId::from_u64(7);
        assert!(images.state(&id).is_none(), "一開始沒有");

        // 第一次：排一個工作，狀態是載入中。
        assert!(matches!(
            images.get_or_fetch(id, "image/png", Some(&target())),
            Some(ImageState::Loading)
        ));
        let (_, job) = jobs.try_recv().expect("排了取圖工作");
        assert_eq!(job.id, id);
        assert_eq!(job.mime, "image/png");

        // 再要一次不會重送請求。
        assert!(matches!(images.state(&id), Some(ImageState::Loading)));
        assert!(jobs.try_recv().is_err(), "載入中不重送");

        // 失敗：留在 Failed，不會自己重抓（否則每一帧都重送）。
        images.finish(id, "image/png", Err("找不到這個附件".into()));
        match images.state(&id) {
            Some(ImageState::Failed(reason)) => assert_eq!(reason, "找不到這個附件"),
            other => panic!("應該是失敗：{other:?}"),
        }
        assert!(
            images
                .get_or_fetch(id, "image/png", Some(&target()))
                .is_some_and(|s| matches!(s, ImageState::Failed(_)))
        );
        assert!(jobs.try_recv().is_err(), "失敗的不自動重抓");

        // 使用者按重試：清掉狀態，下一次重新排工作。
        images.retry(id);
        assert!(images.state(&id).is_none());
        assert!(matches!(
            images.get_or_fetch(id, "image/png", Some(&target())),
            Some(ImageState::Loading)
        ));
        assert!(jobs.try_recv().is_ok(), "重試要真的再排一次");

        // 成功：換成 Ready。
        images.finish(id, "image/png", Ok(vec![1, 2, 3]));
        assert!(matches!(images.state(&id), Some(ImageState::Ready(_))));
    }

    #[test]
    fn unsupported_mime_becomes_a_failure_not_a_panic() {
        let (mut images, _jobs) = fake();
        let id = AttachmentId::from_u64(3);
        images.finish(id, "image/tiff", Ok(vec![1, 2, 3]));
        match images.state(&id) {
            Some(ImageState::Failed(reason)) => {
                assert!(reason.contains("不支援的圖片格式"), "{reason}")
            }
            other => panic!("應該是失敗：{other:?}"),
        }
    }

    #[test]
    fn nothing_is_fetched_without_a_target_or_a_fetcher() {
        // 假資料模式：沒有 fetcher。
        let mut images = AttachmentImages::new();
        let id = AttachmentId::from_u64(1);
        assert!(
            images
                .get_or_fetch(id, "image/png", Some(&target()))
                .is_none()
        );
        assert!(images.state(&id).is_none(), "沒有 fetcher 不記錄狀態");

        // 有 fetcher，但連線目標還沒到手（`ConnEvent::Target` 之前）。
        let (mut images, mut jobs) = fake();
        assert!(images.get_or_fetch(id, "image/png", None).is_none());
        assert!(jobs.try_recv().is_err(), "沒有目標不能送請求");
        assert!(images.state(&id).is_none());
    }

    #[test]
    fn messages_without_attachments_have_no_image_row() {
        let msg = Msg {
            author: crate::model::Author::Human,
            text: "只有文字".into(),
            narration: Vec::new(),
            status: crate::model::MsgStatus::Done,
            tools: None,
            cards: Vec::new(),
            steer: false,
            attachments: Vec::new(),
        };
        assert!(message_images(&msg).is_none());

        let with = Msg {
            attachments: vec![view(1, "image/png", None, None)],
            ..msg
        };
        assert_eq!(message_images(&with).map(<[_]>::len), Some(1));
    }

    #[test]
    fn thumb_ids_are_scoped_by_message_and_index() {
        assert_eq!(thumb_element_id(11, 0), "msg-image-11-0");
        assert_eq!(thumb_element_id(11, 1), "msg-image-11-1");
        assert_ne!(thumb_element_id(11, 0), thumb_element_id(12, 0));
    }

    /// 浮點比較：縮圖尺寸是像素，差 0.01 以內就算一樣。
    fn close(got: (f32, f32), want: (f32, f32)) {
        assert!(
            (got.0 - want.0).abs() < 0.01 && (got.1 - want.1).abs() < 0.01,
            "得到 {got:?}，預期 {want:?}"
        );
    }

    #[test]
    fn thumb_box_follows_the_aspect_ratio_within_the_limit() {
        // 寬圖：貼齊寬度上限，高度等比。
        close(
            thumb_box(&view(1, "image/png", Some(400), Some(200))),
            (THUMB_MAX_W, THUMB_MAX_W / 2.),
        );

        // 高圖：貼齊高度上限。
        close(
            thumb_box(&view(1, "image/png", Some(200), Some(400))),
            (THUMB_MAX_H / 2., THUMB_MAX_H),
        );

        // 很小的圖會被放大到框內（縮圖要看得見）。
        close(
            thumb_box(&view(1, "image/png", Some(20), Some(10))),
            (THUMB_MAX_W, THUMB_MAX_W / 2.),
        );

        // 超長條圖：短邊墊到下限，不會變成一條線。
        close(
            thumb_box(&view(1, "image/png", Some(1000), Some(1))),
            (THUMB_MAX_W, THUMB_MIN),
        );

        // 沒有寬高（上傳端解不出來）：正方形。
        close(
            thumb_box(&view(1, "image/png", None, None)),
            (THUMB_MAX_H, THUMB_MAX_H),
        );
        // 退化資料不 panic。
        close(
            thumb_box(&view(1, "image/png", Some(0), Some(10))),
            (THUMB_MAX_H, THUMB_MAX_H),
        );
    }
}
