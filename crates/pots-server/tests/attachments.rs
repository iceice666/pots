//! 圖片附件：上傳端點、sha256 去重、上限，以及訊息只帶 ID／中介資料（不含本體）。

mod common;

use std::path::Path;

use common::{TestServer, auth_headers, http_get, http_post};
use pots_llm::testing::ScriptedTurn;
use pots_proto::{
    AttachmentId, AttachmentView, ClientCommand, ItemId, MAX_ATTACHMENT_BYTES, ProjectId, SendMode,
    ServerEvent, StreamItemKind, StreamItemView,
};

/// 一張最小的 PNG（`image` 編出來的），內容不同時 sha256 也不同。
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

fn jpeg(width: u32, height: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
    });
    let mut out = Vec::new();
    image::DynamicImage::ImageRgb8(img)
        .write_to(
            &mut std::io::Cursor::new(&mut out),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
    out
}

async fn upload(
    srv: &TestServer,
    project: ProjectId,
    mime: &str,
    filename: Option<&str>,
    body: &[u8],
) -> (u16, String) {
    let mut path = format!("/attachments?project={project}");
    if let Some(name) = filename {
        path.push_str(&format!("&filename={name}"));
    }
    let mut headers = auth_headers(srv);
    headers.push(("Content-Type", mime.to_string()));
    http_post(srv.addr(), &path, &headers, body).await
}

async fn upload_ok(
    srv: &TestServer,
    project: ProjectId,
    mime: &str,
    filename: Option<&str>,
    body: &[u8],
) -> AttachmentView {
    let (status, body) = upload(srv, project, mime, filename, body).await;
    assert_eq!(status, 200, "上傳失敗：{body}");
    serde_json::from_str(&body).expect("回傳的不是 AttachmentView")
}

/// 落地檔案的數量（本體是內容定址，只有一份）。
fn stored_files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir.join("attachments"))
        .map(|it| {
            it.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[tokio::test]
async fn upload_dedups_by_sha256() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = TestServer::start(tmp.path(), []).await;
    let mut c = srv.client().await;
    c.snapshot().await;
    let project = c.create_project("圖片").await;

    let bytes = png(4, 3, 7);
    let first = upload_ok(&srv, project, "image/png", Some("cat.png"), &bytes).await;
    assert_eq!(first.mime, "image/png");
    assert_eq!(first.bytes, bytes.len() as u64);
    assert_eq!((first.width, first.height), (Some(4), Some(3)));
    assert_eq!(first.filename.as_deref(), Some("cat.png"));

    // 同一張圖再傳一次：回同一個 id，只落地一份。
    let second = upload_ok(&srv, project, "image/png", Some("copy.png"), &bytes).await;
    assert_eq!(second.id, first.id);
    assert_eq!(
        second.filename.as_deref(),
        Some("cat.png"),
        "去重時保留第一次的檔名"
    );
    let files = stored_files(tmp.path());
    assert_eq!(files.len(), 1, "同圖只落地一份：{files:?}");

    // 不同的圖是另一個 id。
    let other = upload_ok(&srv, project, "image/png", None, &png(4, 3, 9)).await;
    assert_ne!(other.id, first.id);
    assert_eq!(stored_files(tmp.path()).len(), 2);

    // 另一個 project 有自己的 id（附件屬於 project），但本體仍是同一份。
    let p2 = c.create_project("另一個").await;
    let elsewhere = upload_ok(&srv, p2, "image/png", None, &bytes).await;
    assert_ne!(elsewhere.id, first.id);
    assert_eq!(stored_files(tmp.path()).len(), 2, "本體是內容定址");

    srv.stop().await;
}

#[tokio::test]
async fn rejects_oversize_and_bad_mime() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = TestServer::start(tmp.path(), []).await;
    let mut c = srv.client().await;
    c.snapshot().await;
    let project = c.create_project("上限").await;

    // 超過 10 MiB：413，訊息說明上限。
    let big = vec![0u8; usize::try_from(MAX_ATTACHMENT_BYTES).unwrap() + 1];
    let (status, body) = upload(&srv, project, "image/png", None, &big).await;
    assert_eq!(status, 413, "{body}");
    assert!(body.contains("10 MiB"), "{body}");
    assert!(stored_files(tmp.path()).is_empty(), "被拒的不會落地");

    // 空檔案。
    let (status, _) = upload(&srv, project, "image/png", None, &[]).await;
    assert_eq!(status, 400);

    // 不接受的類型。
    let bytes = png(2, 2, 1);
    let (status, body) = upload(&srv, project, "image/bmp", None, &bytes).await;
    assert_eq!(status, 415, "{body}");

    // 內容和 MIME 對不上（png 當成 jpeg）。
    let (status, body) = upload(&srv, project, "image/jpeg", None, &bytes).await;
    assert_eq!(status, 415, "{body}");
    assert!(body.contains("image/png"), "{body}: 要指出實際格式");

    // 不是圖片的位元組。
    let (status, _) = upload(&srv, project, "image/png", None, b"not an image").await;
    assert_eq!(status, 415);

    // 不存在的 project。
    let (status, _) = upload(&srv, ProjectId::new(), "image/png", None, &bytes).await;
    assert_eq!(status, 404);

    // 合法的一張還是可以上傳（確認上面的拒絕沒有把端點弄壞）。
    let ok = upload_ok(&srv, project, "image/png", None, &bytes).await;
    assert_eq!(ok.filename, None);
    let jpg = jpeg(3, 2);
    let ok = upload_ok(&srv, project, "image/jpeg", Some("a.jpg"), &jpg).await;
    assert_eq!((ok.width, ok.height), (Some(3), Some(2)));
    assert_eq!(ok.mime, "image/jpeg");

    srv.stop().await;
}

#[tokio::test]
async fn message_carries_only_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = TestServer::start(tmp.path(), [ScriptedTurn::reply("看到了")]).await;
    let mut c = srv.client().await;
    c.snapshot().await;
    let project = c.create_project("看圖").await;

    let bytes = png(3, 2, 5);
    let a = upload_ok(&srv, project, "image/png", Some("shot.png"), &bytes).await;
    let b = upload_ok(&srv, project, "image/png", Some("shot2.png"), &png(2, 2, 6)).await;

    let id = ItemId::new();
    c.send(&ClientCommand::Send {
        project,
        id,
        text: "這張圖是什麼？".into(),
        mode: SendMode::Queue,
        attachments: vec![a.id, b.id],
    })
    .await;
    let evs = c.until_idle(project).await;

    // 廣播出去的訊息帶著中介資料（客戶端要顯示縮圖），但沒有本體。
    let human = evs
        .iter()
        .find_map(|e| match e {
            ServerEvent::Item { item, .. } if item.id == id => Some(item.clone()),
            _ => None,
        })
        .expect("沒有收到自己的訊息");
    let StreamItemKind::Human(msg) = &human.kind else {
        panic!("預期人類訊息：{:?}", human.kind);
    };
    assert_eq!(msg.attachments.len(), 2);
    assert_eq!(msg.attachments[0].id, a.id);
    assert_eq!(msg.attachments[0].mime, "image/png");
    assert_eq!(msg.attachments[0].width, Some(3));
    assert_eq!(msg.attachments[1].filename.as_deref(), Some("shot2.png"));

    // 落庫的訊息本體只有 ID 與中介資料：沒有任何圖片位元組／base64。
    let (status, body) = http_get(
        srv.addr(),
        &format!("/projects/{project}/items"),
        &auth_headers(&srv),
    )
    .await;
    assert_eq!(status, 200);
    let items: Vec<StreamItemView> = serde_json::from_str(&body).unwrap();
    let stored = items.iter().find(|i| i.id == id).expect("訊息沒有落庫");
    let json = serde_json::to_string(stored).unwrap();
    assert!(!json.contains("base64"), "{json}");
    assert!(
        !json.contains("iVBORw0KGgo"),
        "PNG 的 base64 前綴不該出現：{json}"
    );
    assert_eq!(json.matches(a.id.to_string().as_str()).count(), 1);

    srv.stop().await;
}

#[tokio::test]
async fn send_validates_attachment_list() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = TestServer::start(tmp.path(), [ScriptedTurn::reply("看到了")]).await;
    let mut c = srv.client().await;
    c.snapshot().await;
    let project = c.create_project("驗證").await;

    let a = upload_ok(&srv, project, "image/png", None, &png(2, 2, 1)).await;
    let b = upload_ok(&srv, project, "image/png", None, &png(2, 2, 2)).await;

    // 超過 4 張被拒。
    let mut many = vec![a.id, b.id];
    for seed in 10..13 {
        many.push(
            upload_ok(&srv, project, "image/png", None, &png(2, 2, seed))
                .await
                .id,
        );
    }
    c.send(&ClientCommand::Send {
        project,
        id: ItemId::new(),
        text: "五張".into(),
        mode: SendMode::Queue,
        attachments: many,
    })
    .await;
    match c.next().await {
        ServerEvent::Error { message } => assert!(message.contains("4 張"), "{message}"),
        other => panic!("預期錯誤，收到 {other:?}"),
    }

    // 重複的 ID 被拒。
    c.send(&ClientCommand::Send {
        project,
        id: ItemId::new(),
        text: "重複".into(),
        mode: SendMode::Queue,
        attachments: vec![a.id, a.id],
    })
    .await;
    match c.next().await {
        ServerEvent::Error { message } => assert!(message.contains("重複"), "{message}"),
        other => panic!("預期錯誤，收到 {other:?}"),
    }

    // 別的 project 的附件被拒。
    let other_project = c.create_project("別的").await;
    let foreign = upload_ok(&srv, other_project, "image/png", None, &png(2, 2, 3)).await;
    c.send(&ClientCommand::Send {
        project,
        id: ItemId::new(),
        text: "借別人的圖".into(),
        mode: SendMode::Queue,
        attachments: vec![foreign.id],
    })
    .await;
    match c.next().await {
        ServerEvent::Error { message } => assert!(message.contains("project"), "{message}"),
        other => panic!("預期錯誤，收到 {other:?}"),
    }

    // 不存在的附件被拒。
    c.send(&ClientCommand::Send {
        project,
        id: ItemId::new(),
        text: "不存在的圖".into(),
        mode: SendMode::Queue,
        attachments: vec![AttachmentId::new()],
    })
    .await;
    match c.next().await {
        ServerEvent::Error { message } => assert!(message.contains("重新上傳"), "{message}"),
        other => panic!("預期錯誤，收到 {other:?}"),
    }

    // 空的訊息（沒有文字也沒有圖）仍然被拒。
    c.send(&ClientCommand::Send {
        project,
        id: ItemId::new(),
        text: "  ".into(),
        mode: SendMode::Queue,
        attachments: Vec::new(),
    })
    .await;
    match c.next().await {
        ServerEvent::Error { message } => assert!(message.contains("空的"), "{message}"),
        other => panic!("預期錯誤，收到 {other:?}"),
    }

    // 只帶圖片、文字空白也可以送（附件本身就是內容）。
    let with_image_only = ItemId::new();
    c.send(&ClientCommand::Send {
        project,
        id: with_image_only,
        text: String::new(),
        mode: SendMode::Queue,
        attachments: vec![a.id],
    })
    .await;
    let ev = c.next().await;
    match &ev {
        ServerEvent::Item { item, .. } if item.id == with_image_only => {
            let StreamItemKind::Human(msg) = &item.kind else {
                panic!("預期人類訊息：{:?}", item.kind);
            };
            assert_eq!(msg.text, "");
            assert_eq!(msg.attachments.len(), 1);
            assert_eq!(msg.attachments[0].id, a.id);
        }
        other => panic!("預期自己的訊息，收到 {other:?}"),
    }
    c.until_idle(project).await;

    srv.stop().await;
}
