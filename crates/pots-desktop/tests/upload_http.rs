//! 真正走一次 HTTP 的圖片上傳（integration test）：起一個本機服務，用 desktop 的
//! [`Uploads`] 打 `POST /attachments`，驗證守衛標頭、成功回應與失敗訊息。
//!
//! 這一層補的是 headless 貼上測試碰不到的部分：`reqwest` 真的送出請求、`Target`
//! 的位址與 token 真的是服務要的、伺服器真的回 `AttachmentView`。仍然沒有覆蓋真剪貼簿
//! 與外觀（見 PR 的未驗證項目）。
//!
//! ```sh
//! cargo test -p pots-desktop --test upload_http -j 1
//! ```

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use pots_desktop::connection::{Target, UploadJob, Uploads};
use pots_proto::ProjectId;

/// 總管對話的 project id（`crates/pots-server/src/store.rs` 的
/// `OVERSIGHT_PROJECT_ID`；服務啟動時就會建立，上傳端點只檢查 project 存不存在）。
const OVERSIGHT_PROJECT_ID: ProjectId = ProjectId::from_u64(0x0e5e_4a11);

/// 起一個本機服務（跑在暫存目錄、port 由系統挑）。
struct TestServer {
    handle: pots_server::ServerHandle,
    dir: tempfile::TempDir,
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
        Self { handle, dir }
    }

    fn target(&self) -> Target {
        Target {
            addr: self.handle.addr,
            token: self.handle.token.as_str().to_string(),
        }
    }

    fn data_dir(&self) -> &Path {
        self.dir.path()
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
    let image = image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("寫 PNG");
    out.into_inner()
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_round_trips_through_the_real_endpoint() {
    let server = TestServer::start().await;
    let (uploads, mut results) = Uploads::spawn();
    uploads.upload(
        server.target(),
        OVERSIGHT_PROJECT_ID,
        UploadJob {
            key: 7,
            mime: "image/png".into(),
            filename: Some("shot.png".into()),
            bytes: png_bytes(),
        },
    );
    let (key, result) = results.next().await.expect("上傳結果");
    assert_eq!(key, 7);
    let view = result.expect("上傳要成功");
    assert_eq!(view.mime, "image/png");
    assert_eq!(view.bytes, png_bytes().len() as u64);
    assert_eq!((view.width, view.height), (Some(2), Some(2)));
    assert_eq!(view.filename.as_deref(), Some("shot.png"));
    // 本體真的落地了（內容定址的檔案）。
    let stored = std::fs::read_dir(server.data_dir().join("attachments"))
        .expect("附件目錄")
        .count();
    assert_eq!(stored, 1);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_failure_keeps_the_server_message() {
    let server = TestServer::start().await;
    let (uploads, mut results) = Uploads::spawn();
    // 不是圖片：伺服器回 415 與中文原因，這裡要原樣拿到。
    uploads.upload(
        server.target(),
        OVERSIGHT_PROJECT_ID,
        UploadJob {
            key: 1,
            mime: "image/png".into(),
            filename: None,
            bytes: b"not an image".to_vec(),
        },
    );
    let (key, result) = results.next().await.expect("上傳結果");
    assert_eq!(key, 1);
    let reason = result.expect_err("不是圖片要被拒");
    assert!(reason.contains("不是看得懂的圖片"), "{reason}");
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_to_a_missing_project_reports_the_server_message() {
    let server = TestServer::start().await;
    let (uploads, mut results) = Uploads::spawn();
    uploads.upload(
        server.target(),
        ProjectId::new(),
        UploadJob {
            key: 2,
            mime: "image/png".into(),
            filename: None,
            bytes: png_bytes(),
        },
    );
    let (_, result) = results.next().await.expect("上傳結果");
    let reason = result.expect_err("不存在的 project");
    assert!(reason.contains("找不到這個 project"), "{reason}");
    server.stop().await;
}

/// token 不對時守衛回 401，原因照顯示（不會靜靜地失敗）。
#[tokio::test(flavor = "multi_thread")]
async fn upload_with_a_bad_token_is_reported() {
    let server = TestServer::start().await;
    let (uploads, mut results) = Uploads::spawn();
    let mut target = server.target();
    target.token = "nope".into();
    uploads.upload(
        target,
        OVERSIGHT_PROJECT_ID,
        UploadJob {
            key: 3,
            mime: "image/png".into(),
            filename: None,
            bytes: png_bytes(),
        },
    );
    let (_, result) = results.next().await.expect("上傳結果");
    let reason = result.expect_err("token 不對");
    assert!(reason.contains("需要 token"), "{reason}");
    server.stop().await;
}
