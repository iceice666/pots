//! API key provider 用的假 server：任何路徑都記錄請求（method、path、header、body），
//! 依序回放排好的回應。

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use url::Url;

use super::Reply;

/// 收到的一次請求。
#[derive(Clone, Debug)]
pub struct ApiRequest {
    pub method: String,
    /// 路徑加 query，例如 `/v1/models?limit=1000`。
    pub path: String,
    /// header 名稱一律小寫。
    pub headers: HashMap<String, String>,
    pub body: Value,
}

impl ApiRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

#[derive(Default)]
struct Inner {
    replies: VecDeque<Reply>,
    requests: Vec<ApiRequest>,
}

#[derive(Clone)]
pub struct FakeApi {
    /// server 根目錄，例如 `http://127.0.0.1:1234/`。
    pub root: Url,
    inner: Arc<Mutex<Inner>>,
}

impl FakeApi {
    pub async fn start(replies: impl IntoIterator<Item = Reply>) -> Self {
        let inner = Arc::new(Mutex::new(Inner {
            replies: replies.into_iter().collect(),
            ..Inner::default()
        }));
        let app = Router::new().fallback(handle).with_state(inner.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            root: Url::parse(&format!("http://{addr}/")).unwrap(),
            inner,
        }
    }

    /// `root` 加上路徑，例如 `url("v1")` → `http://127.0.0.1:1234/v1`。
    pub fn url(&self, path: &str) -> Url {
        self.root.join(path).unwrap()
    }

    pub fn requests(&self) -> Vec<ApiRequest> {
        self.inner.lock().unwrap().requests.clone()
    }
}

async fn handle(
    State(inner): State<Arc<Mutex<Inner>>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut inner = inner.lock().unwrap();
    inner.requests.push(ApiRequest {
        method: method.to_string(),
        path: uri
            .path_and_query()
            .map_or_else(|| uri.path().to_owned(), |p| p.as_str().to_owned()),
        headers: headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    value.to_str().unwrap_or_default().to_owned(),
                )
            })
            .collect(),
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    });
    match inner.replies.pop_front() {
        Some(Reply::Sse(body)) => {
            ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
        }
        Some(Reply::Status(status, body)) => {
            (StatusCode::from_u16(status).unwrap(), axum::Json(body)).into_response()
        }
        None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
