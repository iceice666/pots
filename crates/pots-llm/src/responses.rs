//! OpenAI Responses API provider，給 ChatGPT Plan（SIWC）用。
//!
//! 請求限制（`proposal/agent-runtime.md`）：`store: false`、`stream: true`，完整歷史放
//! `input`，不用 `previous_response_id`；指示放 `instructions`，不送 system item；工具放進
//! namespace；不送 `temperature`、`max_output_tokens` 等不支援的參數。只有
//! `response.completed` 代表成功。
//!
//! - [`build_request_body`]：純函式，把 [`ModelRequest`] 轉成請求 JSON。
//! - [`SseDecoder`]／[`ResponsesEventMapper`]：純狀態機，把 SSE 位元組轉成 [`StreamEvent`]。
//! - [`ApiError`]：錯誤碼對應到 [`LlmError`]。

mod body;
mod error;
mod sse;

use std::collections::VecDeque;
use std::sync::Arc;

use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use url::Url;

pub use self::body::{PROVIDER_NAME, ResponsesOptions, build_request_body};
pub use self::error::ApiError;
pub use self::sse::{ResponsesEventMapper, SseDecoder, SseMessage};
use crate::provider::{EventStream, LlmError, ModelProvider, ModelRequest, StreamEvent};
use crate::siwc::{AccessToken, TokenSource};

/// 正式的 Responses API base URL。
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// 模型清單的一筆（`GET {base}/models` 中 `visibility == "list"` 的項目）。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelInfo {
    /// 送進 [`ModelRequest::model`] 的 ID。
    pub slug: String,
    /// 給使用者看的名稱；伺服器沒給時等於 `slug`。
    pub display_name: String,
}

pub struct ResponsesProvider {
    base_url: Url,
    auth: Arc<dyn TokenSource>,
    http: reqwest::Client,
    options: ResponsesOptions,
}

impl ResponsesProvider {
    /// `base_url` 預設 [`DEFAULT_BASE_URL`]；測試時指向假的 server。
    /// `http` 由呼叫端建立（可與 [`crate::siwc::SiwcAuth`] 共用）。
    pub fn new(base_url: Url, auth: Arc<dyn TokenSource>, http: reqwest::Client) -> Self {
        Self {
            base_url,
            auth,
            http,
            options: ResponsesOptions::default(),
        }
    }

    pub fn with_options(mut self, options: ResponsesOptions) -> Self {
        self.options = options;
        self
    }

    pub fn options(&self) -> &ResponsesOptions {
        &self.options
    }

    /// 這個帳號可用的模型，依伺服器順序。清單不代表 entitlement，實際能否推論以請求結果為準。
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>, LlmError> {
        let url = endpoint(&self.base_url, "models");
        let response = self
            .send_authorized(|token| self.http.get(url.clone()).bearer_auth(&token.0))
            .await?;
        let body: Value = response
            .json()
            .await
            .map_err(|error| LlmError::Unavailable(format!("無法讀取模型清單：{error}")))?;
        parse_models(&body)
    }

    /// 送出請求；401 時呼叫 [`TokenSource::token_rejected`] 換 token 重送一次。
    /// 成功（2xx）才回傳 response，其他狀態轉成 [`LlmError`]。
    async fn send_authorized(
        &self,
        build: impl Fn(&AccessToken) -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, LlmError> {
        let token = self.auth.access_token().await?;
        let mut response = send(build(&token)).await?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            tracing::info!("Responses 回 401，強制 refresh 後重送一次");
            let token = self.auth.token_rejected(&token).await?;
            response = send(build(&token)).await?;
        }
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(http_error(response).await)
        }
    }
}

#[async_trait::async_trait]
impl ModelProvider for ResponsesProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, LlmError> {
        let url = endpoint(&self.base_url, "responses");
        let body = build_request_body(&request, &self.options);
        let send = self.send_authorized(|token| {
            self.http
                .post(url.clone())
                .bearer_auth(&token.0)
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .json(&body)
        });
        let response = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(LlmError::Cancelled),
            response = send => response?,
        };
        Ok(Box::pin(event_stream(
            response.bytes_stream().boxed(),
            cancel,
        )))
    }
}

/// 串流狀態：位元組 → SSE 訊息 → [`StreamEvent`]。
struct StreamState {
    bytes: BoxStream<'static, reqwest::Result<bytes::Bytes>>,
    decoder: SseDecoder,
    mapper: ResponsesEventMapper,
    pending: VecDeque<Result<StreamEvent, LlmError>>,
    cancel: CancellationToken,
    done: bool,
}

impl StreamState {
    fn feed(&mut self, messages: impl IntoIterator<Item = SseMessage>) {
        for message in messages {
            if let Some(event) = self.mapper.handle(&message) {
                self.pending.push_back(event);
            }
        }
    }
}

fn event_stream(
    bytes: BoxStream<'static, reqwest::Result<bytes::Bytes>>,
    cancel: CancellationToken,
) -> impl futures::Stream<Item = Result<StreamEvent, LlmError>> + Send {
    let state = StreamState {
        bytes,
        decoder: SseDecoder::new(),
        mapper: ResponsesEventMapper::new(),
        pending: VecDeque::new(),
        cancel,
        done: false,
    };
    futures::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(event) = state.pending.pop_front() {
                // 終止事件（完成或錯誤）之後不再讀取。
                if matches!(event, Err(_) | Ok(StreamEvent::Completed { .. })) {
                    state.pending.clear();
                    state.done = true;
                }
                return Some((event, state));
            }
            if state.done {
                return None;
            }
            let chunk = tokio::select! {
                biased;
                () = state.cancel.cancelled() => {
                    state.done = true;
                    return Some((Err(LlmError::Cancelled), state));
                }
                chunk = state.bytes.next() => chunk,
            };
            match chunk {
                Some(Ok(bytes)) => {
                    let messages = state.decoder.push(&bytes);
                    state.feed(messages);
                }
                Some(Err(error)) => {
                    state.done = true;
                    return Some((
                        Err(LlmError::Interrupted(format!("讀取串流失敗：{error}"))),
                        state,
                    ));
                }
                None => {
                    let last = state.decoder.finish();
                    state.feed(last);
                    if !state.mapper.is_terminated() {
                        state.pending.push_back(Err(LlmError::Interrupted(
                            "串流在 response.completed 之前結束".into(),
                        )));
                    }
                    state.done = true;
                }
            }
        }
    })
}

/// `{base}/{path}`，保留 base 既有的路徑（`/v1`）。
pub(crate) fn endpoint(base: &Url, path: &str) -> Url {
    let mut url = base.clone();
    let joined = format!("{}/{}", base.path().trim_end_matches('/'), path);
    url.set_path(&joined);
    url
}

async fn send(request: reqwest::RequestBuilder) -> Result<reqwest::Response, LlmError> {
    request
        .send()
        .await
        .map_err(|error| LlmError::Unavailable(format!("連線失敗：{}", error.without_url())))
}

async fn http_error(response: reqwest::Response) -> LlmError {
    let status = response.status().as_u16();
    let request_id = response
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = response.text().await.unwrap_or_default();
    ApiError::from_http(status, &body, request_id).into_llm_error()
}

fn parse_models(body: &Value) -> Result<Vec<ModelInfo>, LlmError> {
    let models = body
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| LlmError::Other("模型清單格式不符：缺少 `models` 陣列".into()))?;
    Ok(models
        .iter()
        .filter(|model| model.get("visibility").and_then(Value::as_str) == Some("list"))
        .filter_map(|model| {
            let slug = model.get("slug").and_then(Value::as_str)?.to_owned();
            let display_name = model
                .get("display_name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .map_or_else(|| slug.clone(), str::to_owned);
            Some(ModelInfo { slug, display_name })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn endpoint_keeps_base_path() {
        let base = Url::parse("https://api.openai.com/v1").unwrap();
        assert_eq!(
            endpoint(&base, "responses").as_str(),
            "https://api.openai.com/v1/responses"
        );
        let base = Url::parse("http://127.0.0.1:9/v1/").unwrap();
        assert_eq!(
            endpoint(&base, "models").as_str(),
            "http://127.0.0.1:9/v1/models"
        );
    }

    #[test]
    fn models_filtered_in_server_order() {
        let body = json!({"models": [
            {"slug": "b", "display_name": "B", "visibility": "list"},
            {"slug": "hidden", "display_name": "H", "visibility": "hide"},
            {"slug": "a", "visibility": "list"},
            {"display_name": "no slug", "visibility": "list"},
        ]});
        assert_eq!(
            parse_models(&body).unwrap(),
            vec![
                ModelInfo {
                    slug: "b".into(),
                    display_name: "B".into()
                },
                ModelInfo {
                    slug: "a".into(),
                    display_name: "a".into()
                },
            ]
        );
        assert!(parse_models(&json!({"data": []})).is_err());
    }
}
