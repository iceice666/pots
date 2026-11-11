//! OpenAI Responses API provider，給 ChatGPT Plan（SIWC）用。
//!
//! 請求限制（`proposal/agent-runtime.md`）：`store: false`、`stream: true`，完整歷史放
//! `input`，不用 `previous_response_id`；指示放 `instructions`，不送 system item；工具放進
//! namespace；不送 `temperature`、`max_output_tokens` 等不支援的參數。只有
//! `response.completed` 代表成功。有設定 [`ModelRequest::reasoning`] 時送
//! `reasoning: {"effort": ...}`，沒設定時用模型預設值。
//!
//! - [`build_request_body`]：純函式，把 [`ModelRequest`] 轉成請求 JSON。
//! - [`SseDecoder`]／[`ResponsesEventMapper`]：純狀態機，把 SSE 位元組轉成 [`StreamEvent`]。
//! - [`ApiError`]：錯誤碼對應到 [`LlmError`]。

mod body;
mod error;
mod sse;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::StreamExt;
use futures::stream::BoxStream;
use reqwest::header::{HeaderMap, HeaderName};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use url::Url;

pub use self::body::{PROVIDER_NAME, ResponsesOptions, build_request_body};
pub use self::error::ApiError;
pub use self::sse::{ResponsesEventMapper, SseDecoder, SseMessage};
use crate::api_common::with_extra_headers;
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
    /// 伺服器拒絕過 `include`（見 [`ApiError::rejects_include`]）；之後的請求都不再送。
    include_rejected: AtomicBool,
    /// 使用者設定的額外 header，排在認證 header 後面。
    headers: HeaderMap,
    /// 有設定時，把 [`ModelRequest::session_id`] 放進這個 header。
    session_header: Option<HeaderName>,
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
            include_rejected: AtomicBool::new(false),
            headers: HeaderMap::new(),
            session_header: None,
        }
    }

    /// 每次請求額外送出的 header。排在 `Authorization` 後面，所以可以覆寫預設值。
    pub fn with_headers(mut self, headers: HeaderMap) -> Self {
        self.headers = headers;
        self
    }

    /// 把 `ModelRequest::session_id` 放進 `name` header；主要給 OpenCode 用。
    pub fn with_session_header(mut self, name: &'static str) -> Self {
        self.session_header = Some(HeaderName::from_static(name));
        self
    }

    /// 用一般 OpenAI 平台 API key 呼叫 Responses API（不是 ChatGPT plan）。
    ///
    /// 與 plan 的差別只在授權與工具形狀：Bearer key，工具平整地放在頂層 `tools`、
    /// `function_call` 不帶 `namespace`。沒有設定 [`ModelRequest::reasoning`] 時推理用模型預設值；
    /// Responses API 允許工具與推理並用。
    pub fn with_api_key(base_url: Url, api_key: String, http: reqwest::Client) -> Self {
        Self::new(base_url, Arc::new(ApiKeyToken(api_key)), http).with_options(ResponsesOptions {
            namespace: None,
            ..ResponsesOptions::default()
        })
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
            .send_authorized(
                |token| self.http.get(url.clone()).bearer_auth(&token.0),
                None,
            )
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
        session: Option<&str>,
    ) -> Result<reqwest::Response, LlmError> {
        self.send_checked(build, session)
            .await?
            .map_err(ApiError::into_llm_error)
    }

    /// 加上額外 header 與這次對話的 session header。
    fn with_extra(
        &self,
        request: reqwest::RequestBuilder,
        session: Option<&str>,
    ) -> Result<reqwest::RequestBuilder, LlmError> {
        with_extra_headers(
            request,
            &self.headers,
            self.session_header.as_ref(),
            session,
        )
    }

    /// 同 [`Self::send_authorized`]，但非 2xx 時回傳解析過的 [`ApiError`]，讓呼叫端決定要不要重送。
    async fn send_checked(
        &self,
        build: impl Fn(&AccessToken) -> reqwest::RequestBuilder,
        session: Option<&str>,
    ) -> Result<Result<reqwest::Response, ApiError>, LlmError> {
        let token = self.auth.access_token().await?;
        let mut response = send(self.with_extra(build(&token), session)?).await?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            tracing::info!("Responses 回 401，強制 refresh 後重送一次");
            let token = self.auth.token_rejected(&token).await?;
            response = send(self.with_extra(build(&token), session)?).await?;
        }
        if response.status().is_success() {
            Ok(Ok(response))
        } else {
            Ok(Err(api_error(response).await))
        }
    }

    /// 送出 `POST /responses`。伺服器拒絕 `include` 時拿掉它重送一次，並記住結果：
    /// `include` 只是要加密的 reasoning 內容，不影響正確性。
    async fn send_responses(
        &self,
        url: &Url,
        request: &ModelRequest,
    ) -> Result<reqwest::Response, LlmError> {
        let mut options = self.options.clone();
        options.include_encrypted_reasoning &= !self.include_rejected.load(Ordering::Relaxed);
        let first = self.post_responses(url, request, &options).await?;
        let error = match first {
            Ok(response) => return Ok(response),
            Err(error) => error,
        };
        if !(options.include_encrypted_reasoning && error.rejects_include()) {
            return Err(error.into_llm_error());
        }
        self.include_rejected.store(true, Ordering::Relaxed);
        tracing::info!(
            code = error.code.as_deref().unwrap_or_default(),
            request_id = error.request_id.as_deref().unwrap_or_default(),
            "Responses 不接受 include（reasoning.encrypted_content），拿掉後重送；之後的請求不再送"
        );
        options.include_encrypted_reasoning = false;
        self.post_responses(url, request, &options)
            .await?
            .map_err(ApiError::into_llm_error)
    }

    async fn post_responses(
        &self,
        url: &Url,
        request: &ModelRequest,
        options: &ResponsesOptions,
    ) -> Result<Result<reqwest::Response, ApiError>, LlmError> {
        let body = build_request_body(request, options);
        self.send_checked(
            |token| {
                self.http
                    .post(url.clone())
                    .bearer_auth(&token.0)
                    .header(reqwest::header::ACCEPT, "text/event-stream")
                    .json(&body)
            },
            request.session_id.as_deref(),
        )
        .await
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
        let send = self.send_responses(&url, &request);
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

/// 固定的 API key，當成 [`TokenSource`]；key 不會出現在 `Debug` 或錯誤訊息。
struct ApiKeyToken(String);

#[async_trait::async_trait]
impl TokenSource for ApiKeyToken {
    async fn access_token(&self) -> Result<AccessToken, LlmError> {
        Ok(AccessToken(self.0.clone()))
    }

    /// API key 沒有更新機制：401 就直接回報憑證錯誤。
    async fn token_rejected(&self, _: &AccessToken) -> Result<AccessToken, LlmError> {
        Err(LlmError::InvalidCredentials("API key 被拒絕（401）".into()))
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

async fn api_error(response: reqwest::Response) -> ApiError {
    let status = response.status().as_u16();
    let request_id = response
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = response.text().await.unwrap_or_default();
    ApiError::from_http(status, &body, request_id)
}

fn parse_models(body: &Value) -> Result<Vec<ModelInfo>, LlmError> {
    // ChatGPT plan 回 `{"models": [{"slug", "visibility"}]}`；一般平台 API 的
    // `GET /v1/models` 回 `{"data": [{"id"}]}`。兩種都接受。
    if let Some(models) = body.get("models").and_then(Value::as_array) {
        return Ok(models
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
            .collect());
    }
    let data = body
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| LlmError::Other("模型清單格式不符：缺少 `models` 或 `data` 陣列".into()))?;
    Ok(data
        .iter()
        .filter_map(|model| model.get("id").and_then(Value::as_str))
        .filter(|id| !id.is_empty() && is_chat_model(id))
        .map(|id| ModelInfo {
            slug: id.to_owned(),
            display_name: id.to_owned(),
        })
        .collect())
}

/// 平台模型清單包含 embeddings、語音等不能用在 Responses 的模型，先篩掉。
fn is_chat_model(id: &str) -> bool {
    const NOT_CHAT: [&str; 6] = [
        "text-embedding",
        "whisper",
        "tts-",
        "dall-e",
        "omni-moderation",
        "sora-",
    ];
    !NOT_CHAT.iter().any(|prefix| id.starts_with(prefix))
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
        assert!(parse_models(&json!({"models": []})).is_ok());
        assert!(parse_models(&json!({"data": []})).is_ok());
        assert!(parse_models(&json!({"object": "list"})).is_err());
    }

    #[test]
    fn platform_models_use_data_ids() {
        let body = json!({"object": "list", "data": [
            {"id": "gpt-6-luna", "object": "model"},
            {"id": "text-embedding-3-large", "object": "model"},
            {"id": "whisper-1", "object": "model"},
            {"object": "model"},
            {"id": "", "object": "model"},
        ]});
        assert_eq!(
            parse_models(&body).unwrap(),
            vec![ModelInfo {
                slug: "gpt-6-luna".into(),
                display_name: "gpt-6-luna".into()
            }]
        );
    }
}
