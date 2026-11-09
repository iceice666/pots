//! HTTP 與 WebSocket gateway。
//!
//! 本機存取保護（`proposal/deployment.md#桌面版app-內嵌服務`）：
//!
//! - 帶 `Origin` 標頭的請求一律 403。原生客戶端不送 `Origin`，瀏覽器一定會送，
//!   所以這同時擋下跨站請求。
//! - `Host` 必須剛好是 `127.0.0.1:<port>` 或 `localhost:<port>`，擋下 DNS rebinding。
//! - 除了 `/healthz`，每個請求（含 WebSocket 升級）都要帶 `Authorization: Bearer <token>`，
//!   以固定時間比較。
//! - 例外：`POST /webhooks/github` 給 GitHub（經 tunnel 或反向代理）呼叫，不帶 token、Host 也不是
//!   loopback；改以 webhook secret 驗 `X-Hub-Signature-256`，沒有設定 secret 時一律 404。

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, Request, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use pots_proto::{ItemId, ProjectId};
use serde::Deserialize;
use subtle::ConstantTimeEq;

use crate::App;

/// 單頁上限；預設 50。
const MAX_PAGE: u32 = 200;
const DEFAULT_PAGE: u32 = 50;

pub(crate) struct Guard {
    pub port: u16,
    pub token: String,
}

impl Guard {
    fn host_ok(&self, headers: &HeaderMap) -> bool {
        let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else {
            return false;
        };
        let Some((name, port)) = host.rsplit_once(':') else {
            return false;
        };
        (name == "127.0.0.1" || name == "localhost") && port == self.port.to_string()
    }

    fn token_ok(&self, headers: &HeaderMap) -> bool {
        let Some(value) = headers
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
        else {
            return false;
        };
        let Some((scheme, token)) = value.split_once(' ') else {
            return false;
        };
        scheme.eq_ignore_ascii_case("bearer")
            && bool::from(token.trim().as_bytes().ct_eq(self.token.as_bytes()))
    }
}

async fn guard(State(g): State<Arc<Guard>>, req: Request, next: Next) -> Response {
    let headers = req.headers();
    if headers.contains_key(header::ORIGIN) {
        return (StatusCode::FORBIDDEN, "不接受瀏覽器的請求").into_response();
    }
    if !g.host_ok(headers) {
        return (StatusCode::FORBIDDEN, "Host 不符").into_response();
    }
    if req.uri().path() != "/healthz" && !g.token_ok(headers) {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "需要 token",
        )
            .into_response();
    }
    next.run(req).await
}

pub(crate) fn router(app: Arc<App>, guard_state: Arc<Guard>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/ws", get(ws))
        .route("/projects/{id}/items", get(items))
        .with_state(app.clone())
        .layer(middleware::from_fn_with_state(guard_state, guard))
        .merge(
            Router::new()
                .route("/webhooks/github", post(github_webhook))
                .route("/webhooks/source/{id}", post(source_webhook))
                .with_state(app),
        )
}

async fn github_webhook(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> Response {
    let (status, text) = app.watcher.github_webhook(&headers, &body).await;
    (status, text).into_response()
}

async fn source_webhook(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (status, text) = crate::watcher::source_webhook(&app.watcher, &id, &headers, &body).await;
    (status, text).into_response()
}

async fn healthz(State(app): State<Arc<App>>) -> impl IntoResponse {
    axum::Json(serde_json::json!({
        "ok": true,
        "instance_id": app.hub.instance().id,
    }))
}

async fn ws(State(app): State<Arc<App>>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| crate::ws::serve(app, socket))
}

#[derive(Deserialize)]
struct PageQuery {
    before: Option<String>,
    limit: Option<u32>,
}

/// `GET /projects/{id}/items?before=<item_id>&limit=N`：`before` 之前的一頁，由舊到新。
/// 沒有 `before` 時是最新的一頁。
async fn items(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Query(q): Query<PageQuery>,
) -> Response {
    let Ok(project) = id.parse::<ProjectId>() else {
        return (StatusCode::NOT_FOUND, "找不到這個 project").into_response();
    };
    let limit = q.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
    match app.store.project(project).await {
        Ok(Some(_)) => {}
        Ok(None) => return (StatusCode::NOT_FOUND, "找不到這個 project").into_response(),
        Err(e) => return internal(e),
    }
    let page = match q.before {
        None => app.store.latest_items(project, limit).await.map(Some),
        Some(before) => {
            let Ok(before) = before.parse::<ItemId>() else {
                return (StatusCode::BAD_REQUEST, "before 不是合法的 ID").into_response();
            };
            app.store.items_before(project, before, limit).await
        }
    };
    match page {
        Ok(Some(items)) => axum::Json(items).into_response(),
        Ok(None) => (StatusCode::BAD_REQUEST, "before 不在這個 project 裡").into_response(),
        Err(e) => internal(e),
    }
}

fn internal(e: crate::Error) -> Response {
    tracing::error!(error = %e, "HTTP 請求失敗");
    (StatusCode::INTERNAL_SERVER_ERROR, "伺服器內部錯誤").into_response()
}
