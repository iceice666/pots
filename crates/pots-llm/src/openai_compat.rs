//! OpenAI 相容的 Chat Completions provider（API key）。
//!
//! 適用 OpenAI（`https://api.openai.com/v1`）、OpenRouter（`https://openrouter.ai/api/v1`）、
//! Ollama（`http://localhost:11434/v1`）、DeepSeek 等提供 `POST {base}/chat/completions`
//! 串流的服務。base URL 要包含版本路徑（`/v1`），結尾有沒有 `/` 都可以。
//!
//! - [`build_request_body`]：純函式，把 [`ModelRequest`] 轉成請求 JSON。
//! - [`ChatCompletionsMapper`]：純狀態機，把 SSE chunk 轉成 [`StreamEvent`](crate::StreamEvent)。
//!
//! API key 只放在 `Authorization: Bearer` header，不會出現在 `Debug`、log 或錯誤訊息；
//! key 是空字串時不送 `Authorization`（例如本機的 Ollama）。

mod body;
mod stream;

use serde_json::Value;
use tokio_util::sync::CancellationToken;
use url::Url;

pub use self::body::{PROVIDER_NAME, build_request_body};
pub use self::stream::ChatCompletionsMapper;
use crate::api_common::{event_stream, read_json, send};
use crate::provider::{EventStream, LlmError, ModelProvider, ModelRequest};
pub use crate::responses::ModelInfo;
use crate::responses::endpoint;

/// OpenAI 官方的 base URL。
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

pub struct OpenAiCompatProvider {
    base_url: Url,
    api_key: String,
    http: reqwest::Client,
}

impl std::fmt::Debug for OpenAiCompatProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompatProvider")
            .field("base_url", &self.base_url.as_str())
            .field("api_key", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl OpenAiCompatProvider {
    /// `base_url` 例如 [`DEFAULT_BASE_URL`]；`http` 由呼叫端建立。
    pub fn new(base_url: Url, api_key: String, http: reqwest::Client) -> Self {
        Self {
            base_url,
            api_key,
            http,
        }
    }

    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    /// `GET {base}/models` 的 `data[].id`，依伺服器順序；`display_name` 等於 ID。
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>, LlmError> {
        let url = endpoint(&self.base_url, "models");
        let response = send(self.authorized(self.http.get(url)), &self.api_key).await?;
        let body = read_json(response, "模型清單").await?;
        parse_models(&body)
    }

    fn authorized(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.api_key.is_empty() {
            request
        } else {
            request.bearer_auth(&self.api_key)
        }
    }
}

#[async_trait::async_trait]
impl ModelProvider for OpenAiCompatProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, LlmError> {
        let url = endpoint(&self.base_url, "chat/completions");
        let body = build_request_body(&request);
        let builder = self
            .authorized(self.http.post(url))
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&body);
        let response = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(LlmError::Cancelled),
            response = send(builder, &self.api_key) => response?,
        };
        Ok(Box::pin(event_stream(
            response,
            ChatCompletionsMapper::new(),
            cancel,
            &self.api_key,
        )))
    }
}

fn parse_models(body: &Value) -> Result<Vec<ModelInfo>, LlmError> {
    let models = body
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| LlmError::Other("模型清單格式不符：缺少 `data` 陣列".into()))?;
    Ok(models
        .iter()
        .filter_map(|model| model.get("id").and_then(Value::as_str))
        .filter(|id| !id.is_empty())
        .map(|id| ModelInfo {
            slug: id.to_owned(),
            display_name: id.to_owned(),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn endpoints_keep_version_path() {
        for base in ["https://api.openai.com/v1", "https://api.openai.com/v1/"] {
            let base = Url::parse(base).unwrap();
            assert_eq!(
                endpoint(&base, "chat/completions").as_str(),
                "https://api.openai.com/v1/chat/completions"
            );
        }
        let openrouter = Url::parse("https://openrouter.ai/api/v1").unwrap();
        assert_eq!(
            endpoint(&openrouter, "models").as_str(),
            "https://openrouter.ai/api/v1/models"
        );
    }

    #[test]
    fn models_in_server_order() {
        let body = json!({"object": "list", "data": [
            {"id": "gpt-b", "object": "model"},
            {"object": "model"},
            {"id": "gpt-a", "object": "model"},
        ]});
        let ids: Vec<_> = parse_models(&body)
            .unwrap()
            .into_iter()
            .map(|m| (m.slug, m.display_name))
            .collect();
        assert_eq!(
            ids,
            vec![
                ("gpt-b".to_owned(), "gpt-b".to_owned()),
                ("gpt-a".to_owned(), "gpt-a".to_owned())
            ]
        );
        assert!(parse_models(&json!({"models": []})).is_err());
    }

    #[test]
    fn debug_hides_key() {
        let provider = OpenAiCompatProvider::new(
            Url::parse(DEFAULT_BASE_URL).unwrap(),
            "sk-secret".into(),
            reqwest::Client::new(),
        );
        let debug = format!("{provider:?}");
        assert!(!debug.contains("sk-secret"), "{debug}");
        assert!(debug.contains("api.openai.com"));
    }
}
