//! 平台的 MCP server（`rmcp`，streamable HTTP）：Claude Code worker 透過它呼叫平台工具。
//!
//! 每個 worker run 拿到一個只在這次 run 有效的 capability token（[`Grants::issue`]），
//! 工具清單與範圍檢查都由平台決定，不靠 CLI 的 allowlist。
//!
//! 存取保護（跟 gateway 的本機 token 是分開的兩道門）：
//!
//! - 帶 `Origin` 標頭的請求一律 403：CLI 不送 `Origin`，瀏覽器一定會送。
//! - 每個請求都要帶 `Authorization: Bearer <token>`，對應到一個還有效的 [`Grant`]；
//!   沒帶、不認得或已撤銷都在進 `rmcp` 之前回 401。表裡只存 token 的 SHA-256，
//!   查表比對的是雜湊，不會因比較時間洩漏 token。token 本身不寫進任何 log。
//! - `Host` 接受 loopback 與 `host.docker.internal`（容器裡的 CLI 經由它連回主機），
//!   不限 port；真正的保護是 bearer token。
//!
//! 以無狀態模式運作（不發 `Mcp-Session-Id`、回應用 `application/json`）：每個 POST 各自
//! 驗 token、各自找 grant，所以 grant 一撤銷，下一個請求就失效。`initialize` 照樣回應，
//! 舊版協定（2025-03-26、2025-06-18 等）也接受。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use pots_llm::agent::{Tool, ToolError};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

/// MCP server 的名字；Claude Code 看到的工具名是 `mcp__pots__<name>`。
pub const SERVER_NAME: &str = "pots";

/// 掛載的路徑。
const PATH: &str = "/mcp";

/// 單一請求 body 的上限。
const MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024;

/// 接受的 `Host`（不限 port）。
const ALLOWED_HOSTS: &[&str] = &["127.0.0.1", "localhost", "::1", "host.docker.internal"];

/// token 的 SHA-256；表的 key。
type Key = [u8; 32];

fn key_of(token: &str) -> Key {
    Sha256::digest(token.as_bytes()).into()
}

/// 一個 grant 能用的工具。
struct Granted {
    /// `tools/list` 的回應，順序與 [`Grants::issue`] 給的一致。
    listed: Vec<rmcp::model::Tool>,
    by_name: HashMap<String, Arc<dyn Tool>>,
}

/// 有效的 capability 與各自能用的工具。
#[derive(Default)]
pub struct Grants {
    live: Mutex<HashMap<Key, Arc<Granted>>>,
}

/// 一次 run 的授權；drop 時立即撤銷。
pub struct Grant {
    /// 交給 CLI 的 bearer token（64 個 hex 字元）。不要寫進 log。
    pub token: String,
    key: Key,
    grants: Arc<Grants>,
}

impl std::fmt::Debug for Grant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Grant").finish_non_exhaustive()
    }
}

impl Drop for Grant {
    fn drop(&mut self) {
        self.grants.lock().remove(&self.key);
    }
}

impl Grants {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 發一個新的 capability，只能用 `tools`。名字重複的工具只留第一個。
    pub fn issue(self: &Arc<Self>, tools: Vec<Arc<dyn Tool>>) -> Grant {
        let mut listed = Vec::with_capacity(tools.len());
        let mut by_name = HashMap::with_capacity(tools.len());
        for tool in tools {
            let spec = tool.spec();
            if by_name.contains_key(&spec.name) {
                tracing::warn!(tool = %spec.name, "MCP grant 裡有重複的工具名，略過後者");
                continue;
            }
            listed.push(rmcp::model::Tool::new(
                spec.name.clone(),
                spec.description,
                input_schema(spec.parameters),
            ));
            by_name.insert(spec.name, tool);
        }
        let granted = Arc::new(Granted { listed, by_name });

        let mut live = self.lock();
        loop {
            let mut bytes = [0u8; 32];
            getrandom::fill(&mut bytes).expect("作業系統的亂數來源無法使用");
            let token = hex::encode(bytes);
            let key = key_of(&token);
            // 256 位元的亂數不會撞，但撞了也不能蓋掉別人的 grant。
            if live.contains_key(&key) {
                continue;
            }
            live.insert(key, granted);
            return Grant {
                token,
                key,
                grants: Arc::clone(self),
            };
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<Key, Arc<Granted>>> {
        // 表裡只有插入與移除，panic 中斷也不會留下半套狀態。
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn get(&self, key: &Key) -> Option<Arc<Granted>> {
        self.lock().get(key).cloned()
    }
}

/// `inputSchema` 必須是 JSON object；不是的話退成接受任何 object。
fn input_schema(parameters: serde_json::Value) -> JsonObject {
    match parameters {
        serde_json::Value::Object(schema) => schema,
        _ => {
            let mut schema = JsonObject::new();
            schema.insert("type".into(), "object".into());
            schema
        }
    }
}

/// Claude Code 的 `--mcp-config` 內容。
pub fn client_config(url: &str, token: &str) -> serde_json::Value {
    let mut servers = serde_json::Map::new();
    servers.insert(
        SERVER_NAME.to_owned(),
        serde_json::json!({
            "type": "http",
            "url": url,
            "headers": { "Authorization": format!("Bearer {token}") },
        }),
    );
    serde_json::json!({ "mcpServers": servers })
}

/// 這個請求屬於哪個 grant（由 [`guard`] 放進 request extensions）。
#[derive(Clone, Copy)]
struct GrantKey(Key);

fn bearer(req: &Request) -> Option<&str> {
    let value = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

async fn guard(State(grants): State<Arc<Grants>>, mut req: Request, next: Next) -> Response {
    if req.headers().contains_key(header::ORIGIN) {
        return (StatusCode::FORBIDDEN, "不接受瀏覽器的請求").into_response();
    }
    let key = bearer(&req).map(key_of);
    match key {
        Some(key) if grants.get(&key).is_some() => {
            req.extensions_mut().insert(GrantKey(key));
            next.run(req).await
        }
        _ => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "需要有效的 capability token",
        )
            .into_response(),
    }
}

/// 掛在 `/mcp` 的 router（不經本機 token 與 `Host` 檢查，改驗 capability）。
///
/// 路由本身就是 `/mcp`，用 `Router::merge` 合進主 router。`shutdown` 取消後不再接受請求。
pub fn router(grants: Arc<Grants>, shutdown: CancellationToken) -> Router {
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_allowed_hosts(ALLOWED_HOSTS.iter().copied())
        .with_max_request_body_bytes(MAX_REQUEST_BODY_BYTES)
        .with_cancellation_token(shutdown);
    let handler = Handler {
        grants: Arc::clone(&grants),
    };
    let service = StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(NeverSessionManager::default()),
        config,
    );
    Router::new()
        .route_service(PATH, service)
        .layer(middleware::from_fn_with_state(grants, guard))
}

#[derive(Clone)]
struct Handler {
    grants: Arc<Grants>,
}

impl Handler {
    /// 請求所屬的 grant；[`guard`] 放行之後才撤銷的話回 `None`。
    fn granted(&self, context: &RequestContext<RoleServer>) -> Option<Arc<Granted>> {
        let parts = context.extensions.get::<axum::http::request::Parts>()?;
        let GrantKey(key) = parts.extensions.get::<GrantKey>()?;
        self.grants.get(key)
    }
}

const REVOKED: &str = "這次 run 的授權已失效";

impl ServerHandler for Handler {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(SERVER_NAME, env!("CARGO_PKG_VERSION")))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let granted = self
            .granted(&context)
            .ok_or_else(|| ErrorData::invalid_request(REVOKED, None))?;
        Ok(ListToolsResult::with_all_items(granted.listed.clone()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let Some(granted) = self.granted(&context) else {
            return Ok(tool_error(REVOKED.to_owned()));
        };
        let Some(tool) = granted.by_name.get(request.name.as_ref()).cloned() else {
            return Ok(tool_error(format!("沒有這個工具：{}", request.name)));
        };
        let arguments = serde_json::Value::Object(request.arguments.unwrap_or_default());
        let arguments = arguments.to_string();
        // 另開 task：客戶端斷線時 rmcp 會丟掉這個 future，平台狀態的變更不能做到一半就停。
        let result = tokio::spawn(async move { tool.call(&arguments).await }).await;
        Ok(match result {
            Ok(Ok(output)) => CallToolResult::success(vec![ContentBlock::text(output)]).into(),
            Ok(Err(ToolError(message))) => tool_error(message),
            Err(e) => {
                tracing::error!(tool = %request.name, error = %e, "平台工具執行失敗");
                tool_error("工具執行失敗".to_owned())
            }
        })
    }
}

fn tool_error(message: String) -> CallToolResponse {
    CallToolResult::error(vec![ContentBlock::text(message)]).into()
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use pots_llm::provider::ToolSpec;
    use rmcp::service::RunningService;
    use rmcp::transport::StreamableHttpClientTransport;
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
    use rmcp::{RoleClient, ServiceExt};
    use serde_json::{Value, json};

    use super::*;

    /// 回傳收到的參數字串。
    struct Echo(&'static str);

    #[async_trait::async_trait]
    impl Tool for Echo {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.0.to_owned(),
                description: format!("echo {}", self.0),
                parameters: json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                }),
            }
        }

        async fn call(&self, arguments: &str) -> Result<String, ToolError> {
            Ok(format!("{}:{arguments}", self.0))
        }
    }

    /// 一律回 ToolError。
    struct Fail;

    #[async_trait::async_trait]
    impl Tool for Fail {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "fail".to_owned(),
                description: "always fails".to_owned(),
                parameters: json!({ "type": "object" }),
            }
        }

        async fn call(&self, _arguments: &str) -> Result<String, ToolError> {
            Err(ToolError("壞掉了".to_owned()))
        }
    }

    /// 睡一下才把旗標設起來，模擬會改平台狀態的慢工具。
    struct Slow(Arc<std::sync::atomic::AtomicBool>);

    #[async_trait::async_trait]
    impl Tool for Slow {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "slow".to_owned(),
                description: "slow".to_owned(),
                parameters: json!({ "type": "object" }),
            }
        }

        async fn call(&self, _arguments: &str) -> Result<String, ToolError> {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok("done".to_owned())
        }
    }

    struct Server {
        addr: SocketAddr,
        shutdown: CancellationToken,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.shutdown.cancel();
        }
    }

    impl Server {
        fn url(&self) -> String {
            format!("http://{}{PATH}", self.addr)
        }
    }

    async fn serve(grants: &Arc<Grants>) -> Server {
        let shutdown = CancellationToken::new();
        let app = router(Arc::clone(grants), shutdown.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = shutdown.clone();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(stop.cancelled_owned())
                .await
                .unwrap();
        });
        Server { addr, shutdown }
    }

    // 測試用的 helper：錯誤型別是 rmcp 的，大小不是我們能決定的。
    #[allow(clippy::result_large_err)]
    async fn connect_with(
        http: reqwest::Client,
        url: String,
        token: &str,
    ) -> Result<RunningService<RoleClient, ()>, rmcp::service::ClientInitializeError> {
        let config = StreamableHttpClientTransportConfig::with_uri(url).auth_header(token);
        ().serve(StreamableHttpClientTransport::with_client(http, config))
            .await
    }

    async fn connect(server: &Server, token: &str) -> RunningService<RoleClient, ()> {
        connect_with(reqwest::Client::new(), server.url(), token)
            .await
            .expect("MCP client 連不上")
    }

    fn args(value: Value) -> JsonObject {
        value.as_object().unwrap().clone()
    }

    fn text(result: &CallToolResult) -> &str {
        &result.content[0].as_text().expect("文字內容").text
    }

    async fn tool_names(client: &RunningService<RoleClient, ()>) -> Vec<String> {
        client
            .list_all_tools()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.name.into_owned())
            .collect()
    }

    /// 直接打一個 JSON-RPC POST。
    async fn post(
        url: &str,
        token: Option<&str>,
        extra: &[(&str, &str)],
        body: Value,
    ) -> reqwest::Response {
        let mut req = reqwest::Client::new()
            .post(url)
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json");
        if let Some(token) = token {
            req = req.bearer_auth(token);
        }
        for (name, value) in extra {
            req = req.header(*name, *value);
        }
        req.body(body.to_string()).send().await.unwrap()
    }

    fn initialize(version: &str) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": version,
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "1.0" },
            },
        })
    }

    #[test]
    fn client_config_shape() {
        assert_eq!(
            client_config("http://127.0.0.1:1/mcp", "abc"),
            json!({
                "mcpServers": {
                    "pots": {
                        "type": "http",
                        "url": "http://127.0.0.1:1/mcp",
                        "headers": { "Authorization": "Bearer abc" },
                    }
                }
            })
        );
    }

    #[test]
    fn tokens_are_random_and_redacted() {
        let grants = Grants::new();
        let a = grants.issue(vec![]);
        let b = grants.issue(vec![]);
        assert_eq!(a.token.len(), 64);
        assert_ne!(a.token, b.token);
        assert!(!format!("{a:?}").contains(&a.token));
        drop(a);
        assert_eq!(grants.lock().len(), 1);
    }

    #[tokio::test]
    async fn list_and_call_tools() {
        let grants = Grants::new();
        let grant = grants.issue(vec![Arc::new(Echo("echo")), Arc::new(Fail)]);
        let server = serve(&grants).await;
        let client = connect(&server, &grant.token).await;

        let info = client.peer_info().expect("server info");
        assert_eq!(info.server_info.as_ref().unwrap().name, SERVER_NAME);

        let tools = client.list_all_tools().await.unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].description.as_deref(), Some("echo echo"));
        assert_eq!(
            Value::Object((*tools[0].input_schema).clone()),
            Echo("echo").spec().parameters
        );
        assert_eq!(tools[1].name, "fail");

        let ok = client
            .call_tool(
                CallToolRequestParams::new("echo").with_arguments(args(json!({ "text": "hi" }))),
            )
            .await
            .unwrap();
        assert_eq!(ok.is_error, Some(false));
        assert_eq!(text(&ok), r#"echo:{"text":"hi"}"#);

        // 沒給 arguments 時工具收到空 object。
        let empty = client
            .call_tool(CallToolRequestParams::new("echo"))
            .await
            .unwrap();
        assert_eq!(text(&empty), "echo:{}");

        let failed = client
            .call_tool(CallToolRequestParams::new("fail"))
            .await
            .unwrap();
        assert_eq!(failed.is_error, Some(true));
        assert_eq!(text(&failed), "壞掉了");

        let unknown = client
            .call_tool(CallToolRequestParams::new("nope"))
            .await
            .unwrap();
        assert_eq!(unknown.is_error, Some(true));

        client.cancel().await.unwrap();
    }

    #[tokio::test]
    async fn grants_only_see_their_own_tools() {
        let grants = Grants::new();
        let a = grants.issue(vec![Arc::new(Echo("a"))]);
        let b = grants.issue(vec![Arc::new(Echo("b"))]);
        let server = serve(&grants).await;
        let client_a = connect(&server, &a.token).await;
        let client_b = connect(&server, &b.token).await;

        assert_eq!(tool_names(&client_a).await, ["a"]);
        assert_eq!(tool_names(&client_b).await, ["b"]);

        let cross = client_a
            .call_tool(CallToolRequestParams::new("b"))
            .await
            .unwrap();
        assert_eq!(cross.is_error, Some(true));
        let own = client_b
            .call_tool(CallToolRequestParams::new("b"))
            .await
            .unwrap();
        assert_eq!(text(&own), "b:{}");
    }

    #[tokio::test]
    async fn rejects_missing_wrong_and_revoked_tokens() {
        let grants = Grants::new();
        let grant = grants.issue(vec![Arc::new(Echo("echo"))]);
        let server = serve(&grants).await;
        let url = server.url();
        let body = initialize("2025-06-18");

        let missing = post(&url, None, &[], body.clone()).await;
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(missing.headers()["www-authenticate"], "Bearer");

        let wrong = "0".repeat(64);
        let wrong = post(&url, Some(&wrong), &[], body.clone()).await;
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

        let basic = post(
            &url,
            None,
            &[("authorization", &format!("Basic {}", grant.token))],
            body.clone(),
        )
        .await;
        assert_eq!(basic.status(), StatusCode::UNAUTHORIZED);

        let origin = post(
            &url,
            Some(&grant.token),
            &[("origin", "http://evil.example")],
            body.clone(),
        )
        .await;
        assert_eq!(origin.status(), StatusCode::FORBIDDEN);

        assert!(
            connect_with(reqwest::Client::new(), url.clone(), "nope")
                .await
                .is_err()
        );

        // 先連上、列出工具，撤銷之後就不能再呼叫。
        let client = connect(&server, &grant.token).await;
        assert_eq!(tool_names(&client).await, ["echo"]);
        let token = grant.token.clone();
        drop(grant);
        assert!(
            client
                .call_tool(CallToolRequestParams::new("echo"))
                .await
                .is_err()
        );
        let revoked = post(&url, Some(&token), &[], body).await;
        assert_eq!(revoked.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn accepts_docker_host() {
        let grants = Grants::new();
        let grant = grants.issue(vec![Arc::new(Echo("echo"))]);
        let server = serve(&grants).await;
        let http = reqwest::Client::builder()
            .resolve("host.docker.internal", server.addr)
            .build()
            .unwrap();
        let url = format!("http://host.docker.internal:{}{PATH}", server.addr.port());
        let client = connect_with(http, url, &grant.token).await.unwrap();
        assert_eq!(tool_names(&client).await, ["echo"]);

        // 其他 Host 一律擋下。
        let other = post(
            &server.url(),
            Some(&grant.token),
            &[("host", &format!("evil.example:{}", server.addr.port()))],
            initialize("2025-06-18"),
        )
        .await;
        assert_eq!(other.status(), StatusCode::FORBIDDEN);
    }

    /// Claude Code 的流程：initialize → notifications/initialized → tools/list → tools/call，
    /// 協定版本用舊版。
    #[tokio::test]
    async fn legacy_protocol_versions() {
        let grants = Grants::new();
        let grant = grants.issue(vec![Arc::new(Echo("echo"))]);
        let server = serve(&grants).await;
        let url = server.url();
        let token = Some(grant.token.as_str());

        for version in ["2025-03-26", "2025-06-18"] {
            let init = post(&url, token, &[], initialize(version)).await;
            assert_eq!(init.status(), StatusCode::OK);
            assert!(init.headers().get("mcp-session-id").is_none());
            let init: Value = init.json().await.unwrap();
            assert_eq!(init["result"]["protocolVersion"], version);
            assert_eq!(init["result"]["serverInfo"]["name"], SERVER_NAME);

            let headers = [("mcp-protocol-version", version)];
            let initialized = post(
                &url,
                token,
                &headers,
                json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            )
            .await;
            assert_eq!(initialized.status(), StatusCode::ACCEPTED);

            let list: Value = post(
                &url,
                token,
                &headers,
                json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
            )
            .await
            .json()
            .await
            .unwrap();
            assert_eq!(list["result"]["tools"][0]["name"], "echo");
            assert_eq!(
                list["result"]["tools"][0]["inputSchema"],
                Echo("echo").spec().parameters
            );

            let call: Value = post(
                &url,
                token,
                &headers,
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "tools/call",
                    "params": { "name": "echo", "arguments": { "text": "x" } },
                }),
            )
            .await
            .json()
            .await
            .unwrap();
            assert_eq!(call["result"]["isError"], false);
            assert_eq!(call["result"]["content"][0]["text"], r#"echo:{"text":"x"}"#);
        }
    }

    /// 客戶端斷線後工具仍跑完。hyper 在等回應時不一定察覺斷線，所以這個測試擋的是行為退化，
    /// 不保證能抓到拿掉 `tokio::spawn` 的改動。
    #[tokio::test]
    async fn client_disconnect_does_not_abort_tool() {
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let grants = Grants::new();
        let grant = grants.issue(vec![Arc::new(Slow(Arc::clone(&done)))]);
        let server = serve(&grants).await;
        let url = server.url();
        let request = post(
            &url,
            Some(&grant.token),
            &[("mcp-protocol-version", "2025-06-18")],
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": { "name": "slow" },
            }),
        );
        // 客戶端等不及就斷線。
        let gave_up = tokio::time::timeout(std::time::Duration::from_millis(50), request).await;
        assert!(gave_up.is_err());
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        assert!(done.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn rejects_oversized_body() {
        let grants = Grants::new();
        let grant = grants.issue(vec![Arc::new(Echo("echo"))]);
        let server = serve(&grants).await;
        let big = "x".repeat(MAX_REQUEST_BODY_BYTES + 1);
        let response = post(
            &server.url(),
            Some(&grant.token),
            &[("mcp-protocol-version", "2025-06-18")],
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": { "name": "echo", "arguments": { "text": big } },
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
}
