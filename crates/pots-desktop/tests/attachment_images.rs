//! 真的走一次 HTTP 的附圖取回（integration test）：起一個本機服務，先上傳一張 PNG，
//! 再用 desktop 的 [`AttachmentImages`]／[`Fetcher`] 打 `GET /attachments/{id}`，驗證
//! 位元組真的回來、解碼成可畫的圖片，以及 404 的中文原因有被保留下來。
//!
//! 這一層補的是 headless render 測試碰不到的部分：`reqwest` 真的送出請求、`Target` 的
//! 位址與 token 真的是服務要的。仍然沒有覆蓋外觀（無 GPU）與真剪貼簿／拖放。
//!
//! ```sh
//! cargo test -p pots-desktop --test attachment_images -j 1
//! ```

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use pots_desktop::connection::{Target, UploadJob, Uploads};
use pots_desktop::images::{AttachmentImages, FetchEvent, ImageState};
use pots_proto::{AttachmentId, ProjectId};

/// 總管對話的 project id（服務啟動時就建立；上傳端點只檢查 project 存不存在）。
const OVERSIGHT_PROJECT_ID: ProjectId = ProjectId::from_u64(0x0e5e_4a11);

struct TestServer {
    handle: pots_server::ServerHandle,
    _dir: tempfile::TempDir,
}

impl TestServer {
    async fn start() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = pots_llm::testing::ScriptedProvider::new([]);
        let deps = pots_server::Deps::new(
            provider,
            "test-model",
            Arc::new(pots_server::FakeModelAuth::signed_in()),
        );
        let handle = pots_server::start(pots_server::Config::desktop(dir.path()), deps)
            .await
            .expect("服務啟動失敗");
        Self { handle, _dir: dir }
    }

    fn target(&self) -> Target {
        Target {
            addr: self.handle.addr,
            token: self.handle.token.as_str().to_string(),
        }
    }

    async fn stop(self) {
        self.handle.shutdown();
        tokio::time::timeout(Duration::from_secs(15), self.handle.wait())
            .await
            .expect("關閉逾時")
            .expect("關閉失敗");
    }
}

/// 一張 2×2 的 PNG。
fn png_bytes() -> Vec<u8> {
    let image = image::RgbaImage::from_pixel(2, 2, image::Rgba([9, 8, 7, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("寫 PNG");
    out.into_inner()
}

/// 上傳一張圖，回傳伺服器給的中介資料。
async fn upload_one(target: &Target) -> pots_proto::AttachmentView {
    let (uploads, mut results) = Uploads::spawn();
    uploads.upload(
        target.clone(),
        OVERSIGHT_PROJECT_ID,
        UploadJob {
            key: 1,
            mime: "image/png".into(),
            filename: None,
            bytes: png_bytes(),
        },
    );
    results
        .next()
        .await
        .expect("上傳結果")
        .1
        .expect("上傳要成功")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_image_is_fetched_and_becomes_drawable() {
    let server = TestServer::start().await;
    let view = upload_one(&server.target()).await;

    let (mut images, mut fetched) = AttachmentImages::spawn();
    assert!(matches!(
        images.get_or_fetch(view.id, &view.mime, Some(&server.target())),
        Some(ImageState::Loading)
    ));
    let FetchEvent::Fetched { id, mime, result } = fetched.next().await.expect("取圖結果")
    else {
        panic!("取圖機制不該壞掉");
    };
    assert_eq!(id, view.id);
    assert_eq!(mime, "image/png");
    images.finish(id, &mime, result);

    match images.state(&view.id) {
        Some(ImageState::Ready(image)) => {
            assert_eq!(image.bytes, png_bytes(), "伺服器回的就是原本上傳的位元組");
        }
        other => panic!("應該是可畫的圖：{other:?}"),
    }

    // 快取命中：不會再排一次請求。
    assert!(matches!(
        images.get_or_fetch(view.id, &view.mime, Some(&server.target())),
        Some(ImageState::Ready(_))
    ));
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_attachment_keeps_the_server_message() {
    let server = TestServer::start().await;
    let (mut images, mut fetched) = AttachmentImages::spawn();
    let missing = AttachmentId::new();
    images.get_or_fetch(missing, "image/png", Some(&server.target()));
    let FetchEvent::Fetched { id, mime, result } = fetched.next().await.expect("取圖結果")
    else {
        panic!("取圖機制不該壞掉");
    };
    assert_eq!(id, missing);
    images.finish(id, &mime, result);
    match images.state(&missing) {
        Some(ImageState::Failed(reason)) => {
            assert!(reason.contains("找不到這個附件"), "{reason}")
        }
        other => panic!("應該是失敗：{other:?}"),
    }
    server.stop().await;
}

/// token 不對時守衛回 401，原因照顯示。
#[tokio::test(flavor = "multi_thread")]
async fn fetching_with_a_bad_token_is_reported() {
    let server = TestServer::start().await;
    let view = upload_one(&server.target()).await;
    let mut target = server.target();
    target.token = "nope".into();

    let (mut images, mut fetched) = AttachmentImages::spawn();
    images.get_or_fetch(view.id, &view.mime, Some(&target));
    let FetchEvent::Fetched { id, mime, result } = fetched.next().await.expect("取圖結果")
    else {
        panic!("取圖機制不該壞掉");
    };
    images.finish(id, &mime, result);
    match images.state(&view.id) {
        Some(ImageState::Failed(reason)) => assert!(reason.contains("需要 token"), "{reason}"),
        other => panic!("應該是失敗：{other:?}"),
    }
    server.stop().await;
}
