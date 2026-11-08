//! 整合測試共用：在暫存目錄啟動服務、WebSocket 客戶端與簡單的 HTTP GET。

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use pots_llm::testing::{ScriptedProvider, ScriptedTurn};
use pots_llm::{LlmError, ModelProvider};
use pots_proto::{
    ClientCommand, ItemId, ModelOption, ProjectId, ProviderId, ProviderKind, SendMode, ServerEvent,
};
use pots_server::{ApiProviderFactory, Config, Deps, FakeModelAuth, ServerHandle};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub const TIMEOUT: Duration = Duration::from_secs(5);

/// 一次 `build` 或 `list_models` 收到的參數。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiCall {
    pub kind: ProviderKind,
    pub base_url: String,
    pub api_key: String,
}

/// 腳本化的 API provider 工廠：`build` 一律回同一個 [`ScriptedProvider`]；`list_models`
/// 回固定清單，key 以 `sk-bad` 開頭時回 `InvalidCredentials`（訊息故意帶著 key）。
pub struct ScriptedApi {
    pub provider: Arc<ScriptedProvider>,
    pub builds: Mutex<Vec<ApiCall>>,
    pub lists: Mutex<Vec<ApiCall>>,
}

impl ScriptedApi {
    pub fn new(turns: impl IntoIterator<Item = ScriptedTurn>) -> Arc<Self> {
        Arc::new(Self {
            provider: ScriptedProvider::named("scripted-api", turns),
            builds: Mutex::new(Vec::new()),
            lists: Mutex::new(Vec::new()),
        })
    }

    pub fn models() -> Vec<ModelOption> {
        vec![
            ModelOption {
                id: "gpt-x".into(),
                display_name: "GPT X".into(),
            },
            ModelOption {
                id: "gpt-y".into(),
                display_name: "GPT Y".into(),
            },
        ]
    }

    pub fn builds(&self) -> Vec<ApiCall> {
        self.builds.lock().unwrap().clone()
    }

    pub fn lists(&self) -> Vec<ApiCall> {
        self.lists.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl ApiProviderFactory for ScriptedApi {
    fn build(
        &self,
        kind: ProviderKind,
        base_url: &url::Url,
        api_key: &str,
    ) -> Result<Arc<dyn ModelProvider>, String> {
        self.builds.lock().unwrap().push(ApiCall {
            kind,
            base_url: base_url.to_string(),
            api_key: api_key.into(),
        });
        Ok(self.provider.clone())
    }

    async fn list_models(
        &self,
        kind: ProviderKind,
        base_url: &url::Url,
        api_key: &str,
    ) -> Result<Vec<ModelOption>, LlmError> {
        self.lists.lock().unwrap().push(ApiCall {
            kind,
            base_url: base_url.to_string(),
            api_key: api_key.into(),
        });
        if api_key.starts_with("sk-bad") {
            return Err(LlmError::InvalidCredentials(format!(
                "401：{api_key} 被拒絕"
            )));
        }
        Ok(Self::models())
    }
}

pub struct TestServer {
    pub handle: ServerHandle,
    /// 內建 ChatGPT 方案的 provider。
    pub provider: Arc<ScriptedProvider>,
    pub api: Arc<ScriptedApi>,
    pub auth: Arc<FakeModelAuth>,
}

impl TestServer {
    pub async fn start(dir: &Path, turns: impl IntoIterator<Item = ScriptedTurn>) -> Self {
        Self::start_with(dir, turns, Arc::new(FakeModelAuth::signed_in())).await
    }

    /// 預設使用 ChatGPT（`default_provider = CHATGPT`），沒有 API provider 的腳本。
    pub async fn start_with(
        dir: &Path,
        turns: impl IntoIterator<Item = ScriptedTurn>,
        auth: Arc<FakeModelAuth>,
    ) -> Self {
        Self::launch(
            Config::desktop(dir).with_default_provider(ProviderId::CHATGPT),
            turns,
            ScriptedApi::new([]),
            auth,
        )
        .await
    }

    pub async fn launch(
        config: Config,
        turns: impl IntoIterator<Item = ScriptedTurn>,
        api: Arc<ScriptedApi>,
        auth: Arc<FakeModelAuth>,
    ) -> Self {
        let provider = ScriptedProvider::new(turns);
        let deps =
            Deps::new(provider.clone(), "test-model", auth.clone()).with_api_providers(api.clone());
        let handle = pots_server::start(config, deps)
            .await
            .expect("服務啟動失敗");
        Self {
            handle,
            provider,
            api,
            auth,
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.handle.addr
    }

    pub fn token(&self) -> &str {
        self.handle.token.as_str()
    }

    pub async fn client(&self) -> Client {
        Client::connect(self, 0).await
    }

    pub async fn stop(self) {
        self.handle.shutdown();
        tokio::time::timeout(Duration::from_secs(15), self.handle.wait())
            .await
            .expect("關閉逾時")
            .expect("關閉失敗");
    }
}

pub struct Client {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    pub last_seq: u64,
}

impl Client {
    pub async fn connect(srv: &TestServer, last_seq: u64) -> Self {
        let mut req = format!("ws://{}/ws", srv.addr())
            .into_client_request()
            .unwrap();
        req.headers_mut().insert(
            "authorization",
            format!("Bearer {}", srv.token()).parse().unwrap(),
        );
        let (ws, _) = tokio_tungstenite::connect_async(req)
            .await
            .expect("WebSocket 連線失敗");
        let mut client = Self { ws, last_seq };
        client.send(&ClientCommand::Resume { last_seq }).await;
        client
    }

    /// 已經連好、還沒送 `Resume` 的連線。
    pub fn from_ws(ws: WebSocketStream<MaybeTlsStream<TcpStream>>) -> Self {
        Self { ws, last_seq: 0 }
    }

    pub async fn send(&mut self, cmd: &ClientCommand) {
        let json = serde_json::to_string(cmd).unwrap();
        self.ws.send(Message::Text(json.into())).await.unwrap();
    }

    pub async fn send_raw(&mut self, text: &str) {
        self.ws
            .send(Message::Text(text.to_string().into()))
            .await
            .unwrap();
    }

    /// 下一個事件；`None` 表示連線關閉。
    pub async fn try_next(&mut self, wait: Duration) -> Option<ServerEvent> {
        loop {
            let msg = tokio::time::timeout(wait, self.ws.next()).await.ok()??;
            match msg {
                Ok(Message::Text(text)) => {
                    let ev: ServerEvent = serde_json::from_str(&text).unwrap();
                    if let Some(seq) = ev.seq() {
                        self.last_seq = seq;
                    }
                    return Some(ev);
                }
                Ok(Message::Close(_)) | Err(_) => return None,
                Ok(_) => {}
            }
        }
    }

    pub async fn next(&mut self) -> ServerEvent {
        self.try_next(TIMEOUT).await.expect("等不到事件")
    }

    /// 收事件直到 `pred` 成立（含該事件）。
    pub async fn until(&mut self, mut pred: impl FnMut(&ServerEvent) -> bool) -> Vec<ServerEvent> {
        let mut out = Vec::new();
        loop {
            let ev = self.next().await;
            let done = pred(&ev);
            out.push(ev);
            if done {
                return out;
            }
        }
    }

    /// 一段時間內沒有任何事件。
    pub async fn assert_quiet(&mut self, wait: Duration) {
        if let Some(ev) = self.try_next(wait).await {
            panic!("預期沒有事件，收到 {ev:?}");
        }
    }

    pub async fn snapshot(&mut self) -> (u64, pots_proto::InstanceSnapshot) {
        match self.next().await {
            ServerEvent::Snapshot { seq, instance } => (seq, *instance),
            other => panic!("預期 Snapshot，收到 {other:?}"),
        }
    }

    /// 建立 project，收到 `ProjectAdded` 與同一筆寫入的 Project Notes（`ArtifactChanged`）為止。
    pub async fn create_project(&mut self, name: &str) -> ProjectId {
        self.send(&ClientCommand::CreateProject { name: name.into() })
            .await;
        let evs = self
            .until(|e| matches!(e, ServerEvent::ProjectAdded { .. }))
            .await;
        let id = match evs.last() {
            Some(ServerEvent::ProjectAdded { project, .. }) => project.id,
            _ => unreachable!(),
        };
        self.until(|e| {
            matches!(e, ServerEvent::ArtifactChanged { project, artifact, .. }
            if *project == id && artifact.kind == pots_proto::ArtifactKind::ProjectNotes)
        })
        .await;
        id
    }

    pub async fn say(&mut self, project: ProjectId, text: &str, mode: SendMode) -> ItemId {
        let id = ItemId::new();
        self.send(&ClientCommand::Send {
            project,
            id,
            text: text.into(),
            mode,
        })
        .await;
        id
    }

    /// 收到這個 project 的 `AgentState { Idle }` 為止。
    pub async fn until_idle(&mut self, project: ProjectId) -> Vec<ServerEvent> {
        self.until(|e| is_state(e, project, pots_proto::AgentState::Idle))
            .await
    }
}

pub fn is_state(e: &ServerEvent, p: ProjectId, s: pots_proto::AgentState) -> bool {
    matches!(e, ServerEvent::AgentState { project, state, .. } if *project == p && *state == s)
}

/// 最簡單的 HTTP/1.1 GET。`headers` 要自己帶 `Host`。
pub async fn http_get(addr: SocketAddr, path: &str, headers: &[(&str, String)]) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut req = format!("GET {path} HTTP/1.1\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("Connection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    tokio::time::timeout(TIMEOUT, stream.read_to_end(&mut buf))
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

/// 帶正確 `Host` 與 token 的標頭。
pub fn auth_headers(srv: &TestServer) -> Vec<(&'static str, String)> {
    vec![
        ("Host", format!("127.0.0.1:{}", srv.addr().port())),
        ("Authorization", format!("Bearer {}", srv.token())),
    ]
}
