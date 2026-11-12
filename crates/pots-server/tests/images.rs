//! 圖片接進對話路徑的端到端測試：附件 → `om_raw` 的 `Item::Image` → provider 請求裡的 base64。
//!
//! 用真的 `HttpApiProviders`（OpenAI 相容）對一個假的 HTTP server，所以驗證的是正式組裝的
//! 路徑：`start()` 把 `StoreImages` 填進 `Deps::images`，provider 送出請求時才從附件檔讀出
//! base64。

mod common;

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Uri, header};
use axum::response::{IntoResponse, Response};
use common::{Client, http_post};
use pots_llm::testing::ScriptedProvider;
use pots_proto::{
    AttachmentView, ClientCommand, ItemId, ProjectId, ProviderDraft, ProviderKind, SendMode,
    ServerEvent, StreamItemKind,
};
use pots_server::{Config, Deps, FakeModelAuth, ServerHandle};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// 假的 OpenAI 相容 server：記錄每次請求的 body，回一段固定的 SSE 文字。
#[derive(Clone)]
struct FakeChat {
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl FakeChat {
    async fn start() -> (Self, String) {
        let fake = Self {
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let app = Router::new().fallback(record).with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (fake, format!("http://{addr}/v1"))
    }

    fn bodies(&self) -> Vec<serde_json::Value> {
        self.requests.lock().unwrap().clone()
    }
}

async fn record(
    State(fake): State<FakeChat>,
    _headers: HeaderMap,
    _uri: Uri,
    body: Bytes,
) -> Response {
    fake.requests
        .lock()
        .unwrap()
        .push(serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null));
    let chunk = serde_json::json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "choices": [{"index": 0, "delta": {"content": "看到了"}, "finish_reason": null}],
    });
    let body = format!(
        "data: {chunk}\n\ndata: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})
    );
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

/// 一張最小的 PNG。
fn png(seed: u8) -> Vec<u8> {
    let img = image::RgbaImage::from_fn(2, 2, |x, y| {
        image::Rgba([seed, (x % 256) as u8, (y % 256) as u8, 255])
    });
    let mut out = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .unwrap();
    out
}

fn headers(handle: &ServerHandle) -> Vec<(&'static str, String)> {
    vec![
        ("Host", format!("127.0.0.1:{}", handle.addr.port())),
        ("Authorization", format!("Bearer {}", handle.token.as_str())),
    ]
}

/// 正式組裝的服務：ChatGPT 用腳本化的 provider（不會被用到），API provider 走
/// `HttpApiProviders`。
async fn launch(handle_dir: &std::path::Path, base: &str) -> (ServerHandle, Client) {
    let config = Config::desktop(handle_dir).with_default_provider(pots_proto::ProviderId::CHATGPT);
    let deps = Deps::new(
        ScriptedProvider::new([]),
        "test-model",
        Arc::new(FakeModelAuth::signed_in()),
    )
    .with_http_api_providers(reqwest::Client::new(), None);
    let handle = pots_server::start(config, deps).await.unwrap();

    let mut req = format!("ws://{}/ws", handle.addr)
        .into_client_request()
        .unwrap();
    req.headers_mut().insert(
        "authorization",
        format!("Bearer {}", handle.token.as_str()).parse().unwrap(),
    );
    let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let mut c = Client::from_ws(ws);
    c.send(&ClientCommand::Resume { last_seq: 0 }).await;
    c.snapshot().await;

    // 新增並選用一個 OpenAI 相容的 provider（指向假的 server）。
    c.send(&ClientCommand::SaveProvider {
        draft: ProviderDraft {
            id: None,
            kind: ProviderKind::OpenAiCompatible,
            name: "假的".into(),
            base_url: base.into(),
            model: "gpt-x".into(),
            api_key: Some("sk-test".into()),
            headers: Vec::new(),
        },
    })
    .await;
    let providers = match c.next().await {
        ServerEvent::Providers { providers, .. } => providers,
        other => panic!("預期 Providers，收到 {other:?}"),
    };
    let api = providers
        .items
        .iter()
        .find(|entry| entry.kind == ProviderKind::OpenAiCompatible)
        .expect("沒有新增的 provider")
        .id;
    // 預設是 ChatGPT，要明確選用新增的 API provider。
    c.send(&ClientCommand::SetActiveProvider { id: api }).await;
    match c.next().await {
        ServerEvent::Providers { providers, .. } => {
            assert_eq!(providers.active, Some(api));
        }
        other => panic!("預期 Providers，收到 {other:?}"),
    }
    (handle, c)
}

async fn upload(handle: &ServerHandle, project: ProjectId, bytes: &[u8]) -> AttachmentView {
    let mut h = headers(handle);
    h.push(("Content-Type", "image/png".to_string()));
    let (status, body) = http_post(
        handle.addr,
        &format!("/attachments?project={project}"),
        &h,
        bytes,
    )
    .await;
    assert_eq!(status, 200, "上傳失敗：{body}");
    serde_json::from_str(&body).unwrap()
}

async fn send_image(
    c: &mut Client,
    project: ProjectId,
    text: &str,
    attachment: &AttachmentView,
) -> ItemId {
    let id = ItemId::new();
    c.send(&ClientCommand::Send {
        project,
        id,
        text: text.into(),
        mode: SendMode::Queue,
        attachments: vec![attachment.id],
    })
    .await;
    let events = c.until_idle(project).await;
    let status = events
        .iter()
        .find_map(|e| match e {
            ServerEvent::ItemDone { item, .. } if matches!(item.kind, StreamItemKind::Agent(_)) => {
                match &item.kind {
                    StreamItemKind::Agent(m) => Some(m.status),
                    _ => None,
                }
            }
            _ => None,
        })
        .expect("沒有 agent 訊息");
    assert_eq!(
        status,
        pots_proto::MsgStatus::Done,
        "run 應該成功：{events:?}"
    );
    id
}

/// `om_raw` 裡的歷史（直接讀資料庫；`om_raw` 沒有 HTTP 端點）。
async fn history(dir: &std::path::Path, project: ProjectId) -> Vec<pots_llm::Item> {
    let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", dir.join("pots.db").display()))
        .await
        .unwrap();
    let rows: Vec<String> =
        sqlx::query_scalar("SELECT item FROM om_raw WHERE project_id = ? ORDER BY seq")
            .bind(project.to_string())
            .fetch_all(&pool)
            .await
            .unwrap();
    pool.close().await;
    rows.iter()
        .map(|row| serde_json::from_str(row).unwrap())
        .collect()
}

#[tokio::test]
async fn attachments_reach_the_model_as_base64_on_every_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let (fake, base) = FakeChat::start().await;
    let (handle, mut c) = launch(tmp.path(), &base).await;
    let project = c.create_project("看圖").await;

    let bytes = png(7);
    let attachment = upload(&handle, project, &bytes).await;
    send_image(&mut c, project, "這張圖是什麼？", &attachment).await;

    // 歷史裡只有參照（沒有 base64）。
    let stored = history(tmp.path(), project).await;
    assert!(
        matches!(
            stored.as_slice(),
            [
                pots_llm::Item::User { text },
                pots_llm::Item::Image { attachment_id, mime },
                pots_llm::Item::Assistant { .. },
            ] if text == "這張圖是什麼？"
                && attachment_id == &attachment.id.to_string()
                && mime == "image/png"
        ),
        "{stored:?}"
    );

    // 模型真的收到圖：user message 的 content 是 text + image_url data URL。
    let bodies = fake.bodies();
    assert_eq!(bodies.len(), 1, "{bodies:?}");
    let expected = format!(
        "data:image/png;base64,{}",
        pots_llm::image::base64_bytes(&bytes)
    );
    let content = bodies[0]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "user")
        .expect("沒有 user message")["content"]
        .clone();
    let parts = content.as_array().expect("有圖片時 content 是陣列");
    assert_eq!(
        parts[0],
        serde_json::json!({"type": "text", "text": "這張圖是什麼？"})
    );
    assert_eq!(
        parts[1],
        serde_json::json!({"type": "image_url", "image_url": {"url": expected}})
    );
    // 資料庫與事件日誌都不該出現本體。
    for item in &stored {
        let json = serde_json::to_string(item).unwrap();
        assert!(
            !json.contains("base64") && !json.contains("data:"),
            "{json}"
        );
    }

    // 第二輪：歷史整份重送，圖片也跟著再送一次（每次都從檔案讀，不快取）。
    send_image(&mut c, project, "再說一次", &attachment).await;
    let bodies = fake.bodies();
    assert_eq!(bodies.len(), 2, "{bodies:?}");
    let text = serde_json::to_string(&bodies[1]).unwrap();
    // 歷史整份重送：第一輪的圖（同一張）與這一輪的圖都在，base64 每次都重新從檔案讀。
    assert_eq!(text.matches(&expected).count(), 2, "{text}");

    handle.shutdown();
    handle.wait().await.unwrap();
}

#[tokio::test]
async fn messages_without_attachments_stay_plain_text() {
    let tmp = tempfile::tempdir().unwrap();
    let (fake, base) = FakeChat::start().await;
    let (handle, mut c) = launch(tmp.path(), &base).await;
    let project = c.create_project("純文字").await;

    c.say(project, "沒有圖", SendMode::Queue).await;
    c.until_idle(project).await;

    let stored = history(tmp.path(), project).await;
    assert!(
        matches!(
            stored.as_slice(),
            [
                pots_llm::Item::User { text },
                pots_llm::Item::Assistant { text: reply },
            ] if text == "沒有圖" && reply == "看到了"
        ),
        "{stored:?}"
    );
    let bodies = fake.bodies();
    let user = bodies[0]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "user")
        .unwrap();
    // 沒有圖片時 content 仍是一般的字串（與加圖片之前完全一樣）。
    assert_eq!(user["content"], serde_json::json!("沒有圖"));

    handle.shutdown();
    handle.wait().await.unwrap();
}

/// 附件檔被刪掉：整個請求失敗並回看得懂的中文說明，不會默默送出少了圖的歷史。
#[tokio::test]
async fn a_missing_attachment_body_fails_the_run_with_a_chinese_message() {
    let tmp = tempfile::tempdir().unwrap();
    let (fake, base) = FakeChat::start().await;
    let (handle, mut c) = launch(tmp.path(), &base).await;
    let project = c.create_project("檔案不見").await;

    let attachment = upload(&handle, project, &png(9)).await;
    std::fs::remove_dir_all(tmp.path().join("attachments")).unwrap();
    let id = ItemId::new();
    c.send(&ClientCommand::Send {
        project,
        id,
        text: "看這張".into(),
        mode: SendMode::Queue,
        attachments: vec![attachment.id],
    })
    .await;
    let events = c.until_idle(project).await;
    let text = events
        .iter()
        .find_map(|e| match e {
            ServerEvent::ItemDone { item, .. } => match &item.kind {
                StreamItemKind::Agent(m) if m.status == pots_proto::MsgStatus::Failed => {
                    Some(m.text.clone())
                }
                _ => None,
            },
            _ => None,
        })
        .expect("應該有失敗的 agent 訊息");
    assert!(text.contains("讀不到圖片附件的內容"), "{text}");
    assert!(fake.bodies().is_empty(), "讀不到本體就不該送出請求");

    handle.shutdown();
    handle.wait().await.unwrap();
}

/// 沒掛圖片來源的 provider（例如只注入 ScriptedProvider 的組裝）：歷史一有圖就明確失敗，
/// 不會默默送一份少了圖的請求。這裡用 `Deps::new` 的預設（空槽沒被 `start` 填過）。
#[tokio::test]
async fn the_empty_slot_reports_a_missing_image_source() {
    let slot = pots_server::ImageSlot::new();
    use pots_llm::image::ImageLoader as _;
    let error = slot.base64("whatever").await.unwrap_err();
    assert!(
        matches!(&error, pots_llm::LlmError::Other(m) if m.contains("沒有接上圖片來源")),
        "{error:?}"
    );
}
