//! Anthropic Messages API provider（API key），也適用提供相同介面的相容服務。
//!
//! base URL 例如 `https://api.anthropic.com`；請求送到 `{base}/v1/messages`。使用者已經在
//! base URL 結尾寫了 `/v1` 時不會重複。
//!
//! - [`build_request_body`]：純函式，把 [`ModelRequest`] 轉成請求 JSON。
//! - [`MessagesEventMapper`]：純狀態機，把 SSE 事件轉成 [`StreamEvent`](crate::StreamEvent)。
//!
//! 推理強度（[`ModelRequest::reasoning`]）：沒設定時不送 `thinking`，用模型預設值。`Low`／
//! `Medium`／`High` 送 `thinking: {type: "adaptive"}` 加 `output_config: {effort}`（Claude 4.6
//! 以後）；`Off` 送 `thinking: {type: "disabled"}`。被拒絕時重送一次，重送成功就記在 provider
//! 上（依模型分開記），之後這個模型直接用後備：
//!
//! - adaptive 被拒（Claude 4.5 以前只支援 extended thinking）→ `thinking: {type: "enabled",
//!   budget_tokens}`，不送 `output_config`，`max_tokens` 提高到放得下 budget
//!   （[`ThinkingConfig::Budget`]）。
//! - `disabled` 被拒（thinking 一直開著的模型）→ 不送 `thinking`。
//!
//! 重送也失敗時不記住，回報重送的錯誤；比對規則寫錯也不會讓之後的請求一直失敗。錯誤訊息的
//! 比對規則寫在 `adaptive_rejected` 與 `disabled_rejected` 的說明。
//!
//! thinking／redacted_thinking block 送成 [`Item::Opaque`](crate::Item::Opaque)，下一輪原樣放回
//! assistant message：搭配工具時 API 要求它們完整、不經修改地回送。
//!
//! API key 只放在 `x-api-key` header，不會出現在 `Debug`、log 或錯誤訊息。

mod body;
mod stream;

use std::collections::HashMap;
use std::sync::Mutex;

use reqwest::header::{HeaderMap, HeaderName};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use url::Url;

pub use self::body::{
    DEFAULT_MAX_TOKENS, PROVIDER_NAME, ThinkingConfig, build_request_body,
    build_request_body_with_thinking,
};
pub use self::stream::MessagesEventMapper;
use crate::api_common::{event_stream, read_json, send, with_extra_headers};
use crate::provider::{EventStream, LlmError, ModelProvider, ModelRequest, Reasoning};
pub use crate::responses::ModelInfo;

/// Anthropic 官方的 base URL。
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// 送出的 `anthropic-version` header。
pub const API_VERSION: &str = "2023-06-01";

/// `list_models` 最多翻幾頁（每頁 1000 筆），避免伺服器一直回 `has_more`。
const MAX_MODEL_PAGES: usize = 20;

pub struct AnthropicProvider {
    base_url: Url,
    api_key: String,
    http: reqwest::Client,
    max_tokens: u32,
    /// 使用者設定的額外 header，排在 provider 的預設值後面。
    headers: HeaderMap,
    /// 有設定時，把 [`ModelRequest::session_id`] 放進這個 header。
    session_header: Option<HeaderName>,
    /// 各模型已經確認要用的 thinking 後備（見模組說明）。key 是模型 ID。
    thinking_fallbacks: Mutex<HashMap<String, ThinkingFallback>>,
    /// 圖片附件的內容來源（`Item::Image` 只有參照）；沒有設定時，歷史裡出現圖片就回錯誤。
    images: Option<std::sync::Arc<dyn crate::image::ImageLoader>>,
}

/// 一個模型被拒絕過、重送成功的 thinking 設定。
#[derive(Clone, Copy, Debug, Default)]
struct ThinkingFallback {
    /// adaptive 被拒：改用 `enabled` + `budget_tokens`。
    budget_instead_of_adaptive: bool,
    /// `disabled` 被拒：不送 `thinking`。
    omit_instead_of_disabled: bool,
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicProvider")
            .field("base_url", &self.base_url.as_str())
            .field("api_key", &"<redacted>")
            .field("max_tokens", &self.max_tokens)
            .field("extra_headers", &self.headers.keys())
            .field("session_header", &self.session_header)
            .finish_non_exhaustive()
    }
}

impl AnthropicProvider {
    /// `base_url` 例如 [`DEFAULT_BASE_URL`]；`http` 由呼叫端建立。
    /// `max_tokens` 預設 [`DEFAULT_MAX_TOKENS`]。
    pub fn new(base_url: Url, api_key: String, http: reqwest::Client) -> Self {
        Self {
            base_url,
            api_key,
            http,
            max_tokens: DEFAULT_MAX_TOKENS,
            headers: HeaderMap::new(),
            session_header: None,
            thinking_fallbacks: Mutex::new(HashMap::new()),
            images: None,
        }
    }

    /// 圖片附件的內容來源：歷史裡的 [`Item::Image`](crate::Item::Image) 只有附件參照，
    /// 送出前由它讀出 base64（見 [`crate::image`]）。
    pub fn with_images(mut self, loader: std::sync::Arc<dyn crate::image::ImageLoader>) -> Self {
        self.images = Some(loader);
        self
    }

    /// 每次請求額外送出的 header。排在認證 header 後面，所以可以覆寫預設值。
    pub fn with_headers(mut self, headers: HeaderMap) -> Self {
        self.headers = headers;
        self
    }

    /// 把 `ModelRequest::session_id` 放進 `name` header；主要給 OpenCode 用。
    pub fn with_session_header(mut self, name: &'static str) -> Self {
        self.session_header = Some(HeaderName::from_static(name));
        self
    }

    /// 每次請求的 `max_tokens`（Messages API 必填）。
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    pub fn max_tokens(&self) -> u32 {
        self.max_tokens
    }

    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    /// `GET {base}/v1/models` 的 `data[].id` 與 `display_name`，依伺服器順序（會跟著分頁）。
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>, LlmError> {
        let mut models = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..MAX_MODEL_PAGES {
            let mut url = endpoint(&self.base_url, "models");
            {
                let mut query = url.query_pairs_mut();
                query.append_pair("limit", "1000");
                if let Some(after) = &after {
                    query.append_pair("after_id", after);
                }
            }
            let request = self.authorized(self.http.get(url))?;
            let request = with_extra_headers(request, &self.headers, None, None)?;
            let response = send(request, &self.api_key).await?;
            let body = read_json(response, "模型清單").await?;
            models.extend(parse_models(&body)?);
            let has_more = body.get("has_more").and_then(Value::as_bool) == Some(true);
            let last_id = body.get("last_id").and_then(Value::as_str);
            match last_id {
                Some(last) if has_more && after.as_deref() != Some(last) => {
                    after = Some(last.to_owned());
                }
                _ => break,
            }
        }
        Ok(models)
    }

    /// 加上 `x-api-key`（標成 sensitive）與 `anthropic-version`。
    fn authorized(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, LlmError> {
        let mut key = reqwest::header::HeaderValue::from_str(&self.api_key)
            .map_err(|_| LlmError::InvalidCredentials("API key 含有無效字元".into()))?;
        key.set_sensitive(true);
        Ok(request
            .header("x-api-key", key)
            .header("anthropic-version", API_VERSION))
    }

    fn fallback(&self, model: &str) -> ThinkingFallback {
        self.thinking_fallbacks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(model)
            .copied()
            .unwrap_or_default()
    }

    fn remember(&self, model: &str, update: impl FnOnce(&mut ThinkingFallback)) {
        let mut fallbacks = self
            .thinking_fallbacks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        update(fallbacks.entry(model.to_owned()).or_default());
    }

    /// 這個請求第一次要送的 thinking 設定：預設對應，套用已經記住的後備。
    fn initial_thinking(&self, request: &ModelRequest) -> ThinkingConfig {
        let Some(reasoning) = request.reasoning else {
            return ThinkingConfig::ModelDefault;
        };
        let fallback = self.fallback(&request.model);
        match reasoning {
            Reasoning::Off if fallback.omit_instead_of_disabled => ThinkingConfig::ModelDefault,
            reasoning if fallback.budget_instead_of_adaptive => ThinkingConfig::budget(reasoning)
                .unwrap_or_else(|| ThinkingConfig::from_reasoning(Some(reasoning))),
            reasoning => ThinkingConfig::from_reasoning(Some(reasoning)),
        }
    }

    /// 送出一次 `POST /v1/messages`，回傳 2xx 的 response。
    async fn post(
        &self,
        url: &Url,
        body: &Value,
        session: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<reqwest::Response, LlmError> {
        // `.json()` 會設定 `content-type: application/json`；額外 header 最後套用，
        // 讓使用者可以覆寫。
        let builder = self
            .authorized(self.http.post(url.clone()))?
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(body);
        let builder = with_extra_headers(
            builder,
            &self.headers,
            self.session_header.as_ref(),
            session,
        )?;
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(LlmError::Cancelled),
            response = send(builder, &self.api_key) => response,
        }
    }
}

/// 被拒絕的 thinking 設定要改用什麼重送；不是 thinking 設定的問題時回 `None`。
fn thinking_retry(
    sent: ThinkingConfig,
    reasoning: Option<Reasoning>,
    error: &LlmError,
) -> Option<ThinkingConfig> {
    match (sent, reasoning) {
        (ThinkingConfig::Adaptive { .. }, Some(reasoning)) if adaptive_rejected(error) => {
            ThinkingConfig::budget(reasoning)
        }
        (ThinkingConfig::Disabled, _) if disabled_rejected(error) => {
            Some(ThinkingConfig::ModelDefault)
        }
        _ => None,
    }
}

/// 請求內容錯誤（HTTP 400 等）的訊息。
fn unsupported_message(error: &LlmError) -> Option<&str> {
    match error {
        LlmError::Unsupported(message) => Some(message),
        _ => None,
    }
}

/// 模型不支援 adaptive thinking（或 `output_config.effort`）。
///
/// 比對規則（只看 [`LlmError::Unsupported`]，也就是 400／404／413／422）：
/// - `adaptive thinking is not supported`：官方文件（thinking-troubleshooting）列出的 Claude 4.5
///   以前模型的訊息「adaptive thinking is not supported on this model」。已對照文件，未實際打過 API。
/// - 同時包含 `'adaptive'` 與 `does not match any of the expected tags`：推測。舊版 API 或相容服務
///   不認得 `adaptive` 這個 tag 時的驗證錯誤格式，未驗證。
/// - 同時包含 `output_config` 與 `Extra inputs are not permitted`：推測。不支援 effort 的模型（例如
///   Claude Sonnet 4.5）對 `output_config` 的錯誤；文件只寫了逐則訊息的 `output_config` 會這樣回，
///   未驗證。
///
/// 後備要重送成功才會記住，所以推測的規則誤判也只是多送一次請求。
fn adaptive_rejected(error: &LlmError) -> bool {
    unsupported_message(error).is_some_and(|message| {
        message.contains("adaptive thinking is not supported")
            || (message.contains("'adaptive'")
                && message.contains("does not match any of the expected tags"))
            || (message.contains("output_config")
                && message.contains("Extra inputs are not permitted"))
    })
}

/// 模型不接受 `thinking: {type: "disabled"}`（thinking 一直開著）。
///
/// 比對規則（只看 [`LlmError::Unsupported`]）：
/// - `"thinking.type.disabled" is not supported`：官方文件（thinking-troubleshooting）列出的訊息，
///   Claude Opus 5.5、Fable／Mythos 系列與 Mythos Preview 都以這段開頭。已對照文件，未實際打過 API。
/// - `instead of {"type": "disabled"}`：官方文件列出的 Claude Sonnet 5.5 訊息（要改送
///   `between_tools`）。這裡不送 `between_tools`，只是不送 `thinking`。未實際打過 API。
///
/// Opus 5／Haiku 5.5 在 effort `xhigh`／`max` 時拒絕 `disabled` 的訊息不比對：這裡不送那兩種 effort。
fn disabled_rejected(error: &LlmError) -> bool {
    unsupported_message(error).is_some_and(|message| {
        message.contains("\"thinking.type.disabled\" is not supported")
            || message.contains("instead of {\"type\": \"disabled\"}")
    })
}

#[async_trait::async_trait]
impl ModelProvider for AnthropicProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, LlmError> {
        let url = endpoint(&self.base_url, "messages");
        let session = request.session_id.as_deref();
        // base64 只在這裡、從本機附件檔讀出來；歷史裡存的是附件參照。
        let images = crate::image::payloads_for(self.images.as_deref(), &request.input).await?;
        let first = self.initial_thinking(&request);
        let body = build_request_body_with_thinking(&request, self.max_tokens, first, &images)?;
        let response = match self.post(&url, &body, session, &cancel).await {
            Ok(response) => response,
            Err(error) => {
                let Some(retry) = thinking_retry(first, request.reasoning, &error) else {
                    return Err(crate::image::explain_unsupported(error, &request));
                };
                tracing::warn!(
                    model = %request.model,
                    rejected = ?first,
                    retry = ?retry,
                    "Anthropic 不接受這個 thinking 設定，改用後備重送"
                );
                let body =
                    build_request_body_with_thinking(&request, self.max_tokens, retry, &images)?;
                let response = self
                    .post(&url, &body, session, &cancel)
                    .await
                    .map_err(|error| crate::image::explain_unsupported(error, &request))?;
                // 重送成功才記住，之後這個模型直接用後備。
                self.remember(&request.model, |fallback| match first {
                    ThinkingConfig::Adaptive { .. } => fallback.budget_instead_of_adaptive = true,
                    _ => fallback.omit_instead_of_disabled = true,
                });
                response
            }
        };
        Ok(Box::pin(event_stream(
            response,
            MessagesEventMapper::new(),
            cancel,
            &self.api_key,
        )))
    }
}

/// `{base}/v1/{path}`；base 已經以 `/v1` 結尾時不重複。保留 base 既有的路徑。
pub(crate) fn endpoint(base: &Url, path: &str) -> Url {
    let mut url = base.clone();
    let base_path = base.path().trim_end_matches('/');
    let joined = if base_path.ends_with("/v1") {
        format!("{base_path}/{path}")
    } else {
        format!("{base_path}/v1/{path}")
    };
    url.set_path(&joined);
    url.set_query(None);
    url
}

fn parse_models(body: &Value) -> Result<Vec<ModelInfo>, LlmError> {
    let models = body
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| LlmError::Other("模型清單格式不符：缺少 `data` 陣列".into()))?;
    Ok(models
        .iter()
        .filter_map(|model| {
            let slug = model
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())?
                .to_owned();
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
    fn endpoint_adds_v1_once() {
        for base in [
            "https://api.anthropic.com",
            "https://api.anthropic.com/",
            "https://api.anthropic.com/v1",
            "https://api.anthropic.com/v1/",
        ] {
            let base = Url::parse(base).unwrap();
            assert_eq!(
                endpoint(&base, "messages").as_str(),
                "https://api.anthropic.com/v1/messages",
                "{base}"
            );
        }
        let proxy = Url::parse("https://proxy.example/anthropic").unwrap();
        assert_eq!(
            endpoint(&proxy, "models").as_str(),
            "https://proxy.example/anthropic/v1/models"
        );
    }

    #[test]
    fn models_with_display_names() {
        let body = json!({"data": [
            {"type": "model", "id": "claude-b", "display_name": "Claude B"},
            {"type": "model", "id": "claude-a"},
            {"type": "model", "display_name": "no id"},
        ], "has_more": false});
        assert_eq!(
            parse_models(&body).unwrap(),
            vec![
                ModelInfo {
                    slug: "claude-b".into(),
                    display_name: "Claude B".into()
                },
                ModelInfo {
                    slug: "claude-a".into(),
                    display_name: "claude-a".into()
                },
            ]
        );
        assert!(parse_models(&json!({})).is_err());
    }

    /// 經過 [`crate::api_common::http_error`] 的 400 錯誤。
    fn rejected(message: &str) -> LlmError {
        crate::api_common::http_error(
            400,
            &json!({"type": "error", "error": {"type": "invalid_request_error", "message": message}})
                .to_string(),
            "",
        )
    }

    #[test]
    fn thinking_rejections_are_matched_conservatively() {
        // 官方文件（thinking-troubleshooting）列出的訊息。
        assert!(adaptive_rejected(&rejected(
            "adaptive thinking is not supported on this model"
        )));
        for message in [
            "\"thinking.type.disabled\" is not supported for this model. Use \"thinking.type.adaptive\" and \"output_config.effort\" to control thinking behavior.",
            "\"thinking.type.disabled\" is not supported for this model. Thinking defaults to adaptive mode when not specified; use \"thinking.type.enabled\" with \"budget_tokens\" for extended thinking.",
            "To turn thinking off on this model, send \"thinking\": {\"type\": \"between_tools\"} instead of {\"type\": \"disabled\"}. The model does not think before responding.",
        ] {
            let error = rejected(message);
            assert!(disabled_rejected(&error), "{message}");
            assert!(!adaptive_rejected(&error), "{message}");
        }
        // 推測的格式。
        assert!(adaptive_rejected(&rejected(
            "thinking.type: Input tag 'adaptive' found using 'type' does not match any of the expected tags: 'disabled', 'enabled'"
        )));
        assert!(adaptive_rejected(&rejected(
            "output_config: Extra inputs are not permitted"
        )));
        // 不相關的錯誤不重送。
        for error in [
            rejected(
                "\"thinking.type.enabled\" is not supported for this model. Use \"thinking.type.adaptive\" and \"output_config.effort\" to control thinking behavior.",
            ),
            rejected("messages: roles must alternate"),
            rejected(
                "`thinking` or `redacted_thinking` blocks in the latest assistant message cannot be modified",
            ),
            rejected(
                "output_config.effort 'xhigh' is not supported when thinking is disabled on this model. Use effort 'high' or below, or enable thinking.",
            ),
            LlmError::Unavailable("adaptive thinking is not supported".into()),
        ] {
            assert!(!adaptive_rejected(&error), "{error:?}");
            assert!(!disabled_rejected(&error), "{error:?}");
        }
    }

    #[test]
    fn retry_depends_on_what_was_sent() {
        let adaptive = ThinkingConfig::from_reasoning(Some(Reasoning::Medium));
        let not_adaptive = rejected("adaptive thinking is not supported on this model");
        assert_eq!(
            thinking_retry(adaptive, Some(Reasoning::Medium), &not_adaptive),
            Some(ThinkingConfig::Budget {
                budget_tokens: 8192
            })
        );
        assert_eq!(
            thinking_retry(
                ThinkingConfig::Disabled,
                Some(Reasoning::Off),
                &not_adaptive
            ),
            None
        );
        let always_on = rejected("\"thinking.type.disabled\" is not supported for this model.");
        assert_eq!(
            thinking_retry(ThinkingConfig::Disabled, Some(Reasoning::Off), &always_on),
            Some(ThinkingConfig::ModelDefault)
        );
        // 已經是後備就不再重送。
        let budget = ThinkingConfig::budget(Reasoning::Low).unwrap();
        assert_eq!(
            thinking_retry(budget, Some(Reasoning::Low), &not_adaptive),
            None
        );
        assert_eq!(
            thinking_retry(ThinkingConfig::ModelDefault, None, &always_on),
            None
        );
    }

    #[test]
    fn debug_hides_key() {
        let provider = AnthropicProvider::new(
            Url::parse(DEFAULT_BASE_URL).unwrap(),
            "sk-ant-secret".into(),
            reqwest::Client::new(),
        )
        .with_max_tokens(100);
        let debug = format!("{provider:?}");
        assert!(!debug.contains("sk-ant-secret"), "{debug}");
        assert!(debug.contains("100"));
    }
}
