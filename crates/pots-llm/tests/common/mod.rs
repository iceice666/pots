//! 整合測試共用的假 OIDC 與假 Responses server（axum，綁 127.0.0.1:0）。
#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::rsa::{KeyPair, KeySize};
use aws_lc_rs::signature::{KeyPair as _, RSA_PKCS1_SHA256};
use axum::Router;
use axum::extract::{Form, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;

pub const ISSUED_CLIENT_ID: &str = "oaiapp_test";
pub const RESOURCE: &str = "https://api.openai.com/v1";
pub const FULL_SCOPE: &str =
    "chatgpt.tokens.use.direct email offline_access openid profile resource.invoke";

async fn serve(app: Router) -> Url {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Url::parse(&format!("http://{addr}")).unwrap()
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

// ---------------------------------------------------------------- 假 OIDC

/// refresh grant 的行為。
#[derive(Clone, Debug)]
pub enum RefreshMode {
    Ok,
    /// 回 400 與這個 OAuth 錯誤碼。
    Error(&'static str),
    /// 回 503。
    Unavailable,
    /// 作廢舊的 refresh token（已輪替），但回一個無法解析的 200。
    Garbled,
}

struct CodeGrant {
    client_id: String,
    challenge: String,
    redirect_uri: String,
    nonce: String,
    sub: String,
    email: String,
}

pub struct OidcInner {
    codes: HashMap<String, CodeGrant>,
    /// 有效的 refresh token → (client_id, sub)。
    refresh_tokens: HashMap<String, (String, String)>,
    counter: u64,
    pub token_requests: Vec<HashMap<String, String>>,
    pub refresh_calls: usize,
    pub revoked: Vec<HashMap<String, String>>,
    pub refresh_mode: RefreshMode,
    pub refresh_delay: Duration,
    /// authorization_code 換 token 前的延遲（模擬換 token 進行中）。
    pub code_delay: Duration,
    pub granted_scope: String,
    pub expires_in: i64,
}

struct OidcState {
    issuer: String,
    key: KeyPair,
    inner: Mutex<OidcInner>,
}

#[derive(Clone)]
pub struct FakeOidc {
    pub issuer: Url,
    state: Arc<OidcState>,
}

impl FakeOidc {
    pub async fn start() -> Self {
        // issuer 要先知道 port，所以先綁 listener。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(OidcState {
            issuer: issuer.clone(),
            key: KeyPair::generate(KeySize::Rsa2048).unwrap(),
            inner: Mutex::new(OidcInner {
                codes: HashMap::new(),
                refresh_tokens: HashMap::new(),
                counter: 0,
                token_requests: Vec::new(),
                refresh_calls: 0,
                revoked: Vec::new(),
                refresh_mode: RefreshMode::Ok,
                refresh_delay: Duration::ZERO,
                code_delay: Duration::ZERO,
                granted_scope: FULL_SCOPE.into(),
                expires_in: 3600,
            }),
        });
        let app = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/jwks", get(jwks))
            .route("/token", post(token))
            .route("/revoke", post(revoke))
            .with_state(state.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            issuer: Url::parse(&issuer).unwrap(),
            state,
        }
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut OidcInner) -> R) -> R {
        f(&mut self.state.inner.lock().unwrap())
    }

    pub fn add_refresh_token(&self, token: &str, client_id: &str, sub: &str) {
        self.with(|inner| {
            inner
                .refresh_tokens
                .insert(token.into(), (client_id.into(), sub.into()))
        });
    }

    /// 讓所有已發出的授權碼失效（模擬換 token 時 `invalid_grant`）。
    pub fn expire_codes(&self) {
        self.with(|inner| inner.codes.clear());
    }

    pub fn is_refresh_token_valid(&self, token: &str) -> bool {
        self.with(|inner| inner.refresh_tokens.contains_key(token))
    }

    /// 模擬使用者在瀏覽器同意：登記授權碼，回傳瀏覽器會被導向的 callback URL。
    pub fn approve(&self, authorize_url: &str, sub: &str, email: &str) -> Url {
        self.approve_with_nonce(authorize_url, sub, email, None)
    }

    pub fn approve_with_nonce(
        &self,
        authorize_url: &str,
        sub: &str,
        email: &str,
        nonce: Option<&str>,
    ) -> Url {
        let url = Url::parse(authorize_url).unwrap();
        let params: HashMap<String, String> = url.query_pairs().into_owned().collect();
        let registering = params["client_id"] == "dynamic_agent_client";
        let client_id = if registering {
            ISSUED_CLIENT_ID.to_owned()
        } else {
            params["client_id"].clone()
        };
        let code = self.with(|inner| {
            inner.counter += 1;
            let code = format!("code-{}", inner.counter);
            inner.codes.insert(
                code.clone(),
                CodeGrant {
                    client_id: client_id.clone(),
                    challenge: params["code_challenge"].clone(),
                    redirect_uri: params["redirect_uri"].clone(),
                    nonce: nonce.unwrap_or(&params["nonce"]).to_owned(),
                    sub: sub.into(),
                    email: email.into(),
                },
            );
            code
        });
        let mut callback = Url::parse(&params["redirect_uri"]).unwrap();
        {
            let mut query = callback.query_pairs_mut();
            query
                .append_pair("code", &code)
                .append_pair("scope", FULL_SCOPE)
                .append_pair("state", &params["state"]);
            // 回流登入時官方 callback 可能不帶 client_id。
            if registering {
                query.append_pair("client_id", &client_id);
            }
        }
        callback
    }
}

impl OidcState {
    fn sign_id_token(
        &self,
        client_id: &str,
        sub: &str,
        email: &str,
        nonce: Option<&str>,
    ) -> String {
        let header = json!({"alg": "RS256", "typ": "JWT", "kid": "test-key"});
        let mut claims = json!({
            "iss": self.issuer,
            "sub": sub,
            "aud": [client_id],
            "email": email,
            "iat": now(),
            "exp": now() + 3600,
        });
        if let Some(nonce) = nonce {
            claims["nonce"] = json!(nonce);
        }
        let message = format!(
            "{}.{}",
            b64(header.to_string().as_bytes()),
            b64(claims.to_string().as_bytes())
        );
        let mut signature = vec![0; self.key.public_modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                message.as_bytes(),
                &mut signature,
            )
            .unwrap();
        format!("{message}.{}", b64(&signature))
    }

    /// 發一組新 token，並登記新的 refresh token。
    fn issue(
        &self,
        inner: &mut OidcInner,
        client_id: &str,
        sub: &str,
        email: &str,
        nonce: Option<&str>,
    ) -> Value {
        inner.counter += 1;
        let n = inner.counter;
        let refresh = format!("rt-{n}");
        inner
            .refresh_tokens
            .insert(refresh.clone(), (client_id.into(), sub.into()));
        json!({
            "access_token": format!("at-{n}"),
            "refresh_token": refresh,
            "id_token": self.sign_id_token(client_id, sub, email, nonce),
            "token_type": "Bearer",
            "expires_in": inner.expires_in,
            "scope": inner.granted_scope,
            "earliest_refresh_at": now() + 60,
        })
    }
}

fn oauth_error(status: StatusCode, code: &str) -> Response {
    (status, axum::Json(json!({"error": code}))).into_response()
}

async fn discovery(State(state): State<Arc<OidcState>>) -> Response {
    let issuer = &state.issuer;
    axum::Json(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "revocation_endpoint": format!("{issuer}/revoke"),
        "jwks_uri": format!("{issuer}/jwks"),
        "response_types_supported": ["code"],
        "id_token_signing_alg_values_supported": ["RS256"],
    }))
    .into_response()
}

async fn jwks(State(state): State<Arc<OidcState>>) -> Response {
    let public = state.key.public_key();
    axum::Json(json!({"keys": [{
        "kty": "RSA",
        "kid": "test-key",
        "alg": "RS256",
        "use": "sig",
        "n": b64(public.modulus().big_endian_without_leading_zero()),
        "e": b64(public.exponent().big_endian_without_leading_zero()),
    }]}))
    .into_response()
}

async fn token(
    State(state): State<Arc<OidcState>>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (mode, delay, code_delay) = {
        let mut inner = state.inner.lock().unwrap();
        inner.token_requests.push(form.clone());
        if form.get("grant_type").map(String::as_str) == Some("refresh_token") {
            inner.refresh_calls += 1;
        }
        (
            inner.refresh_mode.clone(),
            inner.refresh_delay,
            inner.code_delay,
        )
    };
    if form.get("resource").map(String::as_str) != Some(RESOURCE)
        || form.contains_key("client_secret")
    {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    match form.get("grant_type").map(String::as_str) {
        Some("authorization_code") => {
            tokio::time::sleep(code_delay).await;
            let mut inner = state.inner.lock().unwrap();
            let Some(grant) = inner.codes.remove(&form["code"]) else {
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant");
            };
            let challenge = b64(&Sha256::digest(form["code_verifier"].as_bytes()));
            if grant.client_id != form["client_id"]
                || grant.challenge != challenge
                || grant.redirect_uri != form["redirect_uri"]
            {
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant");
            }
            let body = state.issue(
                &mut inner,
                &grant.client_id,
                &grant.sub,
                &grant.email,
                Some(&grant.nonce),
            );
            axum::Json(body).into_response()
        }
        Some("refresh_token") => {
            tokio::time::sleep(delay).await;
            match mode {
                RefreshMode::Ok => {}
                RefreshMode::Error(code) => return oauth_error(StatusCode::BAD_REQUEST, code),
                RefreshMode::Unavailable => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
                RefreshMode::Garbled => {
                    state
                        .inner
                        .lock()
                        .unwrap()
                        .refresh_tokens
                        .remove(&form["refresh_token"]);
                    return (
                        [(header::CONTENT_TYPE, "application/json")],
                        "{\"access_token\": ",
                    )
                        .into_response();
                }
            }
            let mut inner = state.inner.lock().unwrap();
            let Some((client_id, sub)) = inner.refresh_tokens.remove(&form["refresh_token"]) else {
                return oauth_error(StatusCode::BAD_REQUEST, "refresh_token_reused");
            };
            if client_id != form["client_id"] {
                return oauth_error(StatusCode::UNAUTHORIZED, "invalid_client");
            }
            let body = state.issue(&mut inner, &client_id, &sub, "u@example.com", None);
            axum::Json(body).into_response()
        }
        _ => oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type"),
    }
}

async fn revoke(
    State(state): State<Arc<OidcState>>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let mut inner = state.inner.lock().unwrap();
    if let Some(token) = form.get("token") {
        inner.refresh_tokens.remove(token);
    }
    inner.revoked.push(form);
    StatusCode::OK.into_response()
}

// ---------------------------------------------------------------- 假 Responses

/// 假 Responses server 對一次請求的回應。
pub enum Reply {
    /// 200 + `text/event-stream`，body 原樣送出。
    Sse(String),
    /// 指定狀態碼與 JSON body。
    Status(u16, Value),
}

/// 收到的一次請求。
#[derive(Clone, Debug)]
pub struct Recorded {
    pub authorization: Option<String>,
    pub body: Value,
}

#[derive(Default)]
struct ResponsesInner {
    replies: VecDeque<Reply>,
    requests: Vec<Recorded>,
    models: Value,
}

#[derive(Clone)]
pub struct FakeResponses {
    /// 例如 `http://127.0.0.1:1234/v1`。
    pub base: Url,
    inner: Arc<Mutex<ResponsesInner>>,
}

impl FakeResponses {
    pub async fn start(replies: impl IntoIterator<Item = Reply>) -> Self {
        let inner = Arc::new(Mutex::new(ResponsesInner {
            replies: replies.into_iter().collect(),
            ..ResponsesInner::default()
        }));
        let app = Router::new()
            .route("/v1/responses", post(responses))
            .route("/v1/models", get(models))
            .with_state(inner.clone());
        let base = serve(app).await.join("v1").unwrap();
        Self { base, inner }
    }

    pub fn set_models(&self, models: Value) {
        self.inner.lock().unwrap().models = models;
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.inner.lock().unwrap().requests.clone()
    }
}

fn authorization(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

async fn responses(
    State(inner): State<Arc<Mutex<ResponsesInner>>>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    let mut inner = inner.lock().unwrap();
    inner.requests.push(Recorded {
        authorization: authorization(&headers),
        body,
    });
    match inner.replies.pop_front() {
        Some(Reply::Sse(body)) => {
            ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
        }
        Some(Reply::Status(status, body)) => (
            StatusCode::from_u16(status).unwrap(),
            [("x-request-id", "req_test")],
            axum::Json(body),
        )
            .into_response(),
        None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn models(State(inner): State<Arc<Mutex<ResponsesInner>>>, headers: HeaderMap) -> Response {
    let mut inner = inner.lock().unwrap();
    inner.requests.push(Recorded {
        authorization: authorization(&headers),
        body: Value::Null,
    });
    axum::Json(inner.models.clone()).into_response()
}

/// 把事件串成 SSE body（`event:` 用事件的 `type`）。
pub fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect()
}

pub fn text_delta(delta: &str) -> Value {
    json!({"type": "response.output_text.delta", "delta": delta})
}

pub fn item_done(item: Value) -> Value {
    json!({"type": "response.output_item.done", "item": item})
}

pub fn message(text: &str) -> Value {
    item_done(json!({
        "type": "message", "role": "assistant", "id": "msg_1",
        "content": [{"type": "output_text", "text": text}]
    }))
}

pub fn completed(input_tokens: u64, output_tokens: u64) -> Value {
    json!({"type": "response.completed", "response": {
        "id": "resp_1", "status": "completed",
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens}
    }})
}
