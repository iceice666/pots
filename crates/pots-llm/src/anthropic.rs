//! Anthropic Messages API provider（API key），也適用提供相同介面的相容服務。
//!
//! base URL 例如 `https://api.anthropic.com`；請求送到 `{base}/v1/messages`。使用者已經在
//! base URL 結尾寫了 `/v1` 時不會重複。
//!
//! - [`build_request_body`]：純函式，把 [`ModelRequest`] 轉成請求 JSON。
//! - [`MessagesEventMapper`]：純狀態機，把 SSE 事件轉成 [`StreamEvent`](crate::StreamEvent)。
//!
//! API key 只放在 `x-api-key` header，不會出現在 `Debug`、log 或錯誤訊息。

mod body;
mod stream;

use serde_json::Value;
use tokio_util::sync::CancellationToken;
use url::Url;

pub use self::body::{DEFAULT_MAX_TOKENS, PROVIDER_NAME, build_request_body};
pub use self::stream::MessagesEventMapper;
use crate::api_common::{event_stream, read_json, send};
use crate::provider::{EventStream, LlmError, ModelProvider, ModelRequest};
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
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicProvider")
            .field("base_url", &self.base_url.as_str())
            .field("api_key", &"<redacted>")
            .field("max_tokens", &self.max_tokens)
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
        }
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
            let response = send(self.headers(self.http.get(url))?, &self.api_key).await?;
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
    fn headers(
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
        let body = build_request_body(&request, self.max_tokens);
        // `.json()` 會設定 `content-type: application/json`。
        let builder = self
            .headers(self.http.post(url))?
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&body);
        let response = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(LlmError::Cancelled),
            response = send(builder, &self.api_key) => response?,
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
