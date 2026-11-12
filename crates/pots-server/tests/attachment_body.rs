//! `GET /attachments/{id}`：取回圖片本體（桌面端顯示縮圖／大圖用）。
//!
//! 本體是二進位，`common::http_get` 會把它當字串讀壞，所以這裡自己寫一個回傳原始
//! bytes 與標頭的 GET。

mod common;

use std::net::SocketAddr;

use common::{TestServer, auth_headers, http_post};
use pots_proto::{AttachmentId, AttachmentView, ProjectId};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// 一張最小的 PNG（與 `tests/attachments.rs` 同款），內容不同時 sha256 也不同。
fn png(width: u32, height: u32, seed: u8) -> Vec<u8> {
    let img = image::RgbaImage::from_fn(width, height, |x, y| {
        image::Rgba([seed, (x % 256) as u8, (y % 256) as u8, 255])
    });
    let mut out = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .unwrap();
    out
}

struct RawResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl RawResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

/// 原封不動拿回 body bytes 的 HTTP/1.1 GET。`headers` 要自己帶 `Host`。
async fn http_get_raw(addr: SocketAddr, path: &str, headers: &[(&str, String)]) -> RawResponse {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut req = format!("GET {path} HTTP/1.1\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("Connection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    tokio::time::timeout(common::TIMEOUT, stream.read_to_end(&mut buf))
        .await
        .expect("讀回應逾時")
        .expect("讀回應失敗");

    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("回應沒有標頭分隔");
    let head = String::from_utf8_lossy(&buf[..split]).to_string();
    let body = buf[split + 4..].to_vec();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    RawResponse {
        status,
        headers,
        body,
    }
}

async fn upload_ok(srv: &TestServer, project: ProjectId, body: &[u8]) -> AttachmentView {
    let path = format!("/attachments?project={project}");
    let mut headers = auth_headers(srv);
    headers.push(("Content-Type", "image/png".to_string()));
    let (status, text) = http_post(srv.addr(), &path, &headers, body).await;
    assert_eq!(status, 200, "上傳失敗：{text}");
    serde_json::from_str(&text).expect("回傳的不是 AttachmentView")
}

#[tokio::test]
async fn gets_back_the_uploaded_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = TestServer::start(tmp.path(), []).await;
    let mut c = srv.client().await;
    c.snapshot().await;
    let project = c.create_project("取回").await;

    let bytes = png(5, 4, 3);
    let view = upload_ok(&srv, project, &bytes).await;

    let res = http_get_raw(
        srv.addr(),
        &format!("/attachments/{}", view.id),
        &auth_headers(&srv),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.body_text());
    assert_eq!(res.header("content-type"), Some("image/png"));
    assert_eq!(
        res.header("content-length").map(|v| v.parse::<usize>().unwrap()),
        Some(bytes.len()),
        "Content-Length 要是本體的長度"
    );
    assert_eq!(
        res.header("cache-control"),
        Some("private, max-age=31536000, immutable"),
        "本體是內容定址，可以長期快取"
    );
    assert_eq!(res.body, bytes, "取回的位元組要和上傳的一模一樣");

    // jpeg 也走同一條路（MIME 用上傳時驗過的那個，不是猜的）。
    let img = image::RgbImage::from_fn(3, 2, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
    });
    let mut jpg = Vec::new();
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut std::io::Cursor::new(&mut jpg), image::ImageFormat::Jpeg)
        .unwrap();
    let mut headers = auth_headers(&srv);
    headers.push(("Content-Type", "image/jpeg".to_string()));
    let (status, text) = http_post(
        srv.addr(),
        &format!("/attachments?project={project}&filename=a.jpg"),
        &headers,
        &jpg,
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let view: AttachmentView = serde_json::from_str(&text).unwrap();
    let res = http_get_raw(
        srv.addr(),
        &format!("/attachments/{}", view.id),
        &auth_headers(&srv),
    )
    .await;
    assert_eq!(res.status, 200);
    assert_eq!(res.header("content-type"), Some("image/jpeg"));
    assert_eq!(res.body, jpg);

    srv.stop().await;
}

#[tokio::test]
async fn rejects_missing_and_unknown_ids() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = TestServer::start(tmp.path(), []).await;

    // 不存在的 id → 404（中文訊息）。
    let res = http_get_raw(
        srv.addr(),
        &format!("/attachments/{}", AttachmentId::new()),
        &auth_headers(&srv),
    )
    .await;
    assert_eq!(res.status, 404);
    assert!(
        res.body_text().contains("找不到"),
        "訊息要看得懂：{}",
        res.body_text()
    );

    // 不是合法 ID → 也是 404，不能 500。
    let res = http_get_raw(srv.addr(), "/attachments/not-an-id", &auth_headers(&srv)).await;
    assert_eq!(res.status, 404);

    srv.stop().await;
}

#[tokio::test]
async fn requires_the_token() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = TestServer::start(tmp.path(), []).await;
    let mut c = srv.client().await;
    c.snapshot().await;
    let project = c.create_project("守衛").await;
    let view = upload_ok(&srv, project, &png(2, 2, 1)).await;

    // 沒帶 token → 401（守衛在 router 那層就擋掉，不會走到 handler）。
    let host = ("Host", format!("127.0.0.1:{}", srv.addr().port()));
    let res = http_get_raw(
        srv.addr(),
        &format!("/attachments/{}", view.id),
        &[host.clone()],
    )
    .await;
    assert_eq!(res.status, 401);

    // 錯的 token 一樣 401。
    let res = http_get_raw(
        srv.addr(),
        &format!("/attachments/{}", view.id),
        &[host.clone(), ("Authorization", "Bearer wrong".to_string())],
    )
    .await;
    assert_eq!(res.status, 401);

    // 帶 Origin 的請求 403（瀏覽器跨站）。
    let res = http_get_raw(
        srv.addr(),
        &format!("/attachments/{}", view.id),
        &[
            host.clone(),
            ("Authorization", format!("Bearer {}", srv.token())),
            ("Origin", "https://evil.example".to_string()),
        ],
    )
    .await;
    assert_eq!(res.status, 403);

    // 正確的 token 還是拿得到。
    let res = http_get_raw(
        srv.addr(),
        &format!("/attachments/{}", view.id),
        &auth_headers(&srv),
    )
    .await;
    assert_eq!(res.status, 200);

    srv.stop().await;
}

#[tokio::test]
async fn missing_file_on_disk_is_server_error() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = TestServer::start(tmp.path(), []).await;
    let mut c = srv.client().await;
    c.snapshot().await;
    let project = c.create_project("檔案不見").await;
    let view = upload_ok(&srv, project, &png(2, 2, 2)).await;

    // DB 有列但本體被刪掉：500（資料損毀，不是「這個 id 不存在」）。
    let files: Vec<_> = std::fs::read_dir(tmp.path().join("attachments"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    assert_eq!(files.len(), 1);
    std::fs::remove_file(&files[0]).unwrap();

    let res = http_get_raw(
        srv.addr(),
        &format!("/attachments/{}", view.id),
        &auth_headers(&srv),
    )
    .await;
    assert_eq!(res.status, 500, "{}", res.body_text());

    srv.stop().await;
}
