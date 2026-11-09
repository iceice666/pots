//! M6b 其他 connector 的整合測試（`proposal/events-and-automation.md`）：RSS／Atom、HTTP 探測、
//! 狀態頁與通用 webhook。上游換成假的 HTTP 伺服器，時間換成手動撥的時鐘。
//!
//! 事件有沒有進來，看 listener 的觸發紀錄（自動化頁的歷史）與對話裡的觸發，不去讀資料庫。

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use common::TestServer;
use hmac::{Hmac, Mac};
use pots_llm::ModelRequest;
use pots_llm::testing::{FnProvider, ScriptedTurn};
use pots_proto::{
    AutomationView, ClientCommand, ListenerActionSpec, ListenerSpec, ProjectId, ProviderId,
    ServerEvent, SourceDraft, SourceKind, Timestamp, TrustLevel, WebhookMapping,
};
use pots_server::{Config, Deps, FakeModelAuth, ManualClock, WatcherConfig};
use serde_json::{Value, json};
use sha2::Sha256;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// ---------------------------------------------------------------------------
// 假的上游（feed、探測、狀態頁）

#[derive(Default)]
struct UpState {
    /// (id, 標題)。
    items: Vec<(String, String)>,
    probe_status: u16,
    incidents: Vec<Value>,
    components: Vec<Value>,
    /// 每個路徑被打過幾次。
    hits: Vec<(&'static str, usize)>,
}

impl UpState {
    fn hit(&mut self, path: &'static str) {
        if let Some(e) = self.hits.iter_mut().find(|(p, _)| *p == path) {
            e.1 += 1;
        } else {
            self.hits.push((path, 1));
        }
    }

    fn hits(&self, path: &str) -> usize {
        self.hits
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, n)| *n)
            .unwrap_or(0)
    }
}

#[derive(Clone)]
struct FakeUpstream {
    addr: SocketAddr,
    state: Arc<Mutex<UpState>>,
}

impl FakeUpstream {
    async fn start() -> Self {
        let state = Arc::new(Mutex::new(UpState {
            probe_status: 500,
            ..Default::default()
        }));
        let app = Router::new()
            .route("/feed.xml", get(feed))
            .route("/broken.xml", get(broken))
            .route("/probe", get(probe))
            .route("/status.json", get(status))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { addr, state }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    fn add_item(&self, id: &str, title: &str) {
        self.state
            .lock()
            .unwrap()
            .items
            .push((id.into(), title.into()));
    }

    fn set_probe(&self, status: u16) {
        self.state.lock().unwrap().probe_status = status;
    }

    fn set_incident(&self, id: &str, name: &str, status: &str, updated_at: &str) {
        let mut s = self.state.lock().unwrap();
        s.incidents = vec![json!({
            "id": id,
            "name": name,
            "status": status,
            "impact": "minor",
            "shortlink": format!("https://status.example.com/incidents/{id}"),
            "updated_at": updated_at,
        })];
        if !s.hits.iter().any(|(p, _)| *p == "/status.json") {
            s.components = vec![json!({
                "id": "api",
                "name": "API",
                "status": "operational",
            })];
        }
    }

    fn set_component(&self, status: &str) {
        self.state.lock().unwrap().components = vec![json!({
            "id": "api",
            "name": "API",
            "status": status,
        })];
    }

    fn hits(&self, path: &str) -> usize {
        self.state.lock().unwrap().hits(path)
    }
}

async fn feed(State(state): State<Arc<Mutex<UpState>>>) -> Response {
    let mut s = state.lock().unwrap();
    s.hit("/feed.xml");
    let items: String = s
        .items
        .iter()
        .map(|(id, title)| {
            format!(
                "<item><title>{title}</title><link>https://example.com/{id}</link>\
                 <guid>{id}</guid><pubDate>Mon, 09 Nov 2026 10:00:00 +0800</pubDate>\
                 <description>{title} 的內容</description></item>"
            )
        })
        .collect();
    let body = format!(
        "<?xml version=\"1.0\"?><rss version=\"2.0\"><channel><title>Release notes</title>{items}</channel></rss>"
    );
    ([("content-type", "application/rss+xml")], body).into_response()
}

async fn broken() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, "壞了").into_response()
}

async fn probe(State(state): State<Arc<Mutex<UpState>>>) -> Response {
    let mut s = state.lock().unwrap();
    s.hit("/probe");
    (StatusCode::from_u16(s.probe_status).unwrap(), "ok").into_response()
}

async fn status(State(state): State<Arc<Mutex<UpState>>>) -> Response {
    let mut s = state.lock().unwrap();
    s.hit("/status.json");
    let body = json!({
        "page": {"id": "up", "name": "上游服務"},
        "components": s.components,
        "incidents": s.incidents,
    });
    axum::Json(body).into_response()
}

// ---------------------------------------------------------------------------
// 啟動服務

async fn launch(dir: &Path, clock: &ManualClock) -> TestServer {
    let model = FnProvider::new("fn", |_: &ModelRequest| ScriptedTurn::reply("收到"));
    let mut config = Config::desktop(dir).with_default_provider(ProviderId::CHATGPT);
    config.worker.run_queue = false;
    config.watcher = WatcherConfig {
        poll_interval: Duration::from_millis(50),
        sweep_interval: Duration::from_millis(100),
        // 這些測試不用 GitHub。
        github_api: "http://127.0.0.1:1".into(),
        clock: clock.clock(),
    };
    let deps = Deps::new(model, "test-model", Arc::new(FakeModelAuth::signed_in()));
    let handle = pots_server::start(config, deps)
        .await
        .expect("服務啟動失敗");
    TestServer {
        handle,
        provider: pots_llm::testing::ScriptedProvider::new([]),
        api: common::ScriptedApi::new([]),
        auth: Arc::new(FakeModelAuth::signed_in()),
    }
}

fn rss_draft(id: &str, url: String) -> SourceDraft {
    SourceDraft {
        id: id.into(),
        kind: SourceKind::Rss,
        name: "Release notes".into(),
        url: Some(url),
        interval_secs: Some(30),
        trust: TrustLevel::Public,
        secret: None,
        mapping: WebhookMapping::default(),
        enabled: true,
    }
}

fn listener(kinds: &[&str]) -> ListenerSpec {
    ListenerSpec {
        project: ProjectId(Default::default()),
        kinds: kinds.iter().map(|k| k.to_string()).collect(),
        resource: None,
        fields: vec![],
        action: ListenerActionSpec::NotifyChat,
        max_per_hour: 20,
        budget_per_trigger_cents: None,
    }
}

/// 等到自動化頁的條件成立（最多 `wait`）。
async fn wait_automation(
    c: &mut common::Client,
    mut pred: impl FnMut(&AutomationView) -> bool,
    wait: Duration,
) -> Option<AutomationView> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return None;
        }
        match c.try_next(left).await {
            Some(ServerEvent::Automation { automation, .. }) => {
                if pred(&automation) {
                    return Some(automation);
                }
            }
            Some(_) => {}
            None => return None,
        }
    }
}

async fn until_automation(
    c: &mut common::Client,
    pred: impl FnMut(&AutomationView) -> bool,
) -> AutomationView {
    wait_automation(c, pred, Duration::from_secs(5))
        .await
        .expect("等不到自動化頁的更新")
}

fn history(a: &AutomationView) -> usize {
    a.listeners.iter().map(|l| l.history.len()).sum()
}

async fn save_source(c: &mut common::Client, draft: SourceDraft) {
    let id = draft.id.clone();
    c.send(&ClientCommand::SaveSource { draft }).await;
    until_automation(c, |a| a.sources.iter().any(|s| s.id == id)).await;
}

/// 撥一次時鐘，讓來源到了下一次輪詢的時間。
async fn tick(clock: &ManualClock) {
    clock.advance(60);
    tokio::time::sleep(Duration::from_millis(250)).await;
}

// ---------------------------------------------------------------------------
// RSS／Atom

#[tokio::test]
async fn rss_records_a_snapshot_then_triggers_on_new_items() {
    let tmp = tempfile::tempdir().unwrap();
    let up = FakeUpstream::start().await;
    let clock = ManualClock::new(Timestamp::now());
    let srv = launch(tmp.path(), &clock).await;
    let mut c = srv.client().await;
    let project = c.create_project("看 release note").await;

    let mut spec = listener(&["feed.item"]);
    spec.project = project;
    c.send(&ClientCommand::SaveListener { id: None, spec })
        .await;
    until_automation(&mut c, |a| a.listeners.len() == 1).await;

    up.add_item("v1", "第一版");
    save_source(&mut c, rss_draft("rss:notes", up.url("/feed.xml"))).await;
    // 第一次輪詢只記下現況。
    wait_for(|| up.hits("/feed.xml") >= 1).await;
    assert!(
        wait_automation(&mut c, |a| history(a) > 0, Duration::from_millis(300))
            .await
            .is_none(),
        "第一次輪詢不該把舊項目當新事件"
    );

    up.add_item("v2", "第二版");
    tick(&clock).await;
    let a = until_automation(&mut c, |a| history(a) == 1).await;
    let t = &a.listeners[0].history[0];
    assert!(t.title.contains("Release notes：第二版"), "{:?}", t.title);

    // 同一個項目再輪詢一次不會重複觸發。
    let before = up.hits("/feed.xml");
    tick(&clock).await;
    wait_for(|| up.hits("/feed.xml") > before).await;
    assert!(
        wait_automation(&mut c, |a| history(a) > 1, Duration::from_millis(300))
            .await
            .is_none(),
        "同一個項目不該再觸發"
    );
    srv.stop().await;
}

#[tokio::test]
async fn a_failing_feed_marks_the_source_as_broken() {
    let tmp = tempfile::tempdir().unwrap();
    let up = FakeUpstream::start().await;
    let clock = ManualClock::new(Timestamp::now());
    let srv = launch(tmp.path(), &clock).await;
    let mut c = srv.client().await;

    save_source(&mut c, rss_draft("rss:broken", up.url("/broken.xml"))).await;
    let a = until_automation(&mut c, |a| {
        a.sources.iter().any(|s| {
            s.id == "rss:broken" && matches!(s.health, pots_proto::SourceHealth::Error { .. })
        })
    })
    .await;
    let s = a.sources.iter().find(|s| s.id == "rss:broken").unwrap();
    assert!(matches!(s.health, pots_proto::SourceHealth::Error { .. }));
    srv.stop().await;
}

// ---------------------------------------------------------------------------
// HTTP 探測

#[tokio::test]
async fn probe_debounces_failures_and_recoveries() {
    let tmp = tempfile::tempdir().unwrap();
    let up = FakeUpstream::start().await;
    let clock = ManualClock::new(Timestamp::now());
    let srv = launch(tmp.path(), &clock).await;
    let mut c = srv.client().await;
    let project = c.create_project("顧 API").await;

    let mut spec = listener(&["probe.failed", "probe.recovered"]);
    spec.project = project;
    c.send(&ClientCommand::SaveListener { id: None, spec })
        .await;
    until_automation(&mut c, |a| a.listeners.len() == 1).await;

    save_source(
        &mut c,
        SourceDraft {
            id: "probe:api".into(),
            kind: SourceKind::Probe,
            name: "API".into(),
            url: Some(up.url("/probe")),
            interval_secs: Some(30),
            trust: TrustLevel::Owned,
            secret: None,
            mapping: WebhookMapping::default(),
            enabled: true,
        },
    )
    .await;

    // 前兩次失敗不發事件（防抖）。
    for n in 1..=2 {
        tick(&clock).await;
        let got = wait_automation(&mut c, |a| history(a) > 0, Duration::from_millis(300)).await;
        assert!(got.is_none(), "第 {n} 次失敗就發事件了：{got:?}");
    }
    tick(&clock).await;
    let a = until_automation(&mut c, |a| history(a) == 1).await;
    assert!(a.listeners[0].history[0].title.contains("探測失敗"));

    // 一次成功還不算恢復，第二次才發。
    up.set_probe(200);
    tick(&clock).await;
    assert!(
        wait_automation(&mut c, |a| history(a) > 1, Duration::from_millis(300))
            .await
            .is_none()
    );
    tick(&clock).await;
    let a = until_automation(&mut c, |a| history(a) == 2).await;
    assert!(a.listeners[0].history[0].title.contains("恢復"));
    srv.stop().await;
}

// ---------------------------------------------------------------------------
// 狀態頁

#[tokio::test]
async fn status_page_reports_incidents_and_components() {
    let tmp = tempfile::tempdir().unwrap();
    let up = FakeUpstream::start().await;
    let clock = ManualClock::new(Timestamp::now());
    let srv = launch(tmp.path(), &clock).await;
    let mut c = srv.client().await;
    let project = c.create_project("看上游").await;

    let mut spec = listener(&[
        "status.incident.opened",
        "status.incident.updated",
        "status.incident.resolved",
        "status.component.changed",
    ]);
    spec.project = project;
    c.send(&ClientCommand::SaveListener { id: None, spec })
        .await;
    until_automation(&mut c, |a| a.listeners.len() == 1).await;

    up.set_incident("i1", "API 變慢", "investigating", "2026-11-09T02:00:00Z");
    save_source(
        &mut c,
        SourceDraft {
            id: "status:up".into(),
            kind: SourceKind::StatusPage,
            name: "上游服務".into(),
            url: Some(up.url("/status.json")),
            interval_secs: Some(30),
            trust: TrustLevel::Public,
            secret: None,
            mapping: WebhookMapping::default(),
            enabled: true,
        },
    )
    .await;
    // 第一次輪詢只記現況。
    wait_for(|| up.hits("/status.json") >= 1).await;
    assert!(
        wait_automation(&mut c, |a| history(a) > 0, Duration::from_millis(300))
            .await
            .is_none(),
        "第一次輪詢不該把舊 incident 當新事件"
    );

    // 更新：五分鐘內的多次更新合併成一則。
    up.set_incident("i1", "API 變慢", "identified", "2026-11-09T02:01:00Z");
    tick(&clock).await;
    let a = until_automation(&mut c, |a| history(a) == 1).await;
    assert!(
        a.listeners[0].history[0].title.contains("更新"),
        "{:?}",
        a.listeners[0].history[0].title
    );
    up.set_incident("i1", "API 變慢", "monitoring", "2026-11-09T02:02:00Z");
    tick(&clock).await;
    assert!(
        wait_automation(&mut c, |a| history(a) > 1, Duration::from_millis(400))
            .await
            .is_none(),
        "五分鐘內的更新要合併"
    );

    // 元件狀態變化與解決。
    up.set_component("degraded");
    tick(&clock).await;
    let a = until_automation(&mut c, |a| history(a) == 2).await;
    assert!(a.listeners[0].history[0].title.contains("元件"));
    up.set_incident("i1", "API 變慢", "resolved", "2026-11-09T02:10:00Z");
    tick(&clock).await;
    let a = until_automation(&mut c, |a| history(a) == 3).await;
    assert!(a.listeners[0].history[0].title.contains("已解決"));
    srv.stop().await;
}

// ---------------------------------------------------------------------------
// 通用 webhook

const SECRET: &str = "0123456789abcdef";

fn sign(body: &[u8], secret: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

async fn post_webhook(addr: SocketAddr, path: &str, body: &str, signature: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
         X-Pots-Signature: {signature}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf))
        .await
        .unwrap()
        .unwrap();
    let text = String::from_utf8_lossy(&buf).to_string();
    let status = text
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

#[tokio::test]
async fn generic_webhook_verifies_signature_and_maps_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let clock = ManualClock::new(Timestamp::now());
    let srv = launch(tmp.path(), &clock).await;
    let mut c = srv.client().await;
    let project = c.create_project("收 alert").await;

    let mut spec = listener(&["alert.triggered"]);
    spec.project = project;
    c.send(&ClientCommand::SaveListener { id: None, spec })
        .await;
    until_automation(&mut c, |a| a.listeners.len() == 1).await;

    save_source(
        &mut c,
        SourceDraft {
            id: "webhook:alert".into(),
            kind: SourceKind::Webhook,
            name: "告警".into(),
            url: None,
            interval_secs: None,
            trust: TrustLevel::Members,
            secret: Some(SECRET.into()),
            mapping: WebhookMapping {
                kind: "/type".into(),
                resource: "/issue/id".into(),
                summary: "/issue/title".into(),
                dedupe: "/issue/id".into(),
                body: "/issue/detail".into(),
            },
            enabled: true,
        },
    )
    .await;

    let body = json!({
        "type": "alert.triggered",
        "issue": {"id": "42", "title": "磁碟快滿了", "detail": "剩 3%"},
    })
    .to_string();
    let path = "/webhooks/source/webhook:alert";

    // 簽章不對：拒絕，也不要有事件。
    let (status, _) = post_webhook(srv.addr(), path, &body, "sha256=00").await;
    assert_eq!(status, 401);
    assert!(
        wait_automation(&mut c, |a| history(a) > 0, Duration::from_millis(300))
            .await
            .is_none(),
        "驗簽失敗不該有事件"
    );

    let (status, _) = post_webhook(srv.addr(), path, &body, &sign(body.as_bytes(), SECRET)).await;
    assert_eq!(status, 202);
    let a = until_automation(&mut c, |a| history(a) == 1).await;
    assert_eq!(a.listeners[0].history[0].title, "磁碟快滿了");
    // 看過的自訂種類會列出來，新的 listener 表單才選得到。
    let a = until_automation(&mut c, |a| {
        a.custom_kinds.iter().any(|k| k == "alert.triggered")
    })
    .await;
    assert!(a.custom_kinds.iter().any(|k| k == "alert.triggered"));

    // 同一個 id 重送（例如對方重試）只算一次。
    let (status, _) = post_webhook(srv.addr(), path, &body, &sign(body.as_bytes(), SECRET)).await;
    assert_eq!(status, 202);
    assert!(
        wait_automation(&mut c, |a| history(a) > 1, Duration::from_millis(300))
            .await
            .is_none(),
        "同一個 dedupe key 不該再觸發"
    );

    // 沒有 secret 的來源、不存在的來源。
    let (status, _) = post_webhook(srv.addr(), "/webhooks/source/nope", &body, "x").await;
    assert_eq!(status, 404);
    srv.stop().await;
}

/// 等一個條件成立（最多 5 秒）。
async fn wait_for(mut f: impl FnMut() -> bool) {
    for _ in 0..100 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("等不到條件成立");
}
