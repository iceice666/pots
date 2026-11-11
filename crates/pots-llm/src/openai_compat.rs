//! OpenAI 相容的 Chat Completions provider（API key）。
//!
//! 適用 OpenAI（`https://api.openai.com/v1`）、OpenRouter（`https://openrouter.ai/api/v1`）、
//! Ollama（`http://localhost:11434/v1`）、DeepSeek 等提供 `POST {base}/chat/completions`
//! 串流的服務。base URL 要包含版本路徑（`/v1`），結尾有沒有 `/` 都可以。
//!
//! - [`build_request_body`]：純函式，把 [`ModelRequest`] 轉成請求 JSON。
//! - [`ChatCompletionsMapper`]：純狀態機，把 SSE chunk 轉成 [`StreamEvent`](crate::StreamEvent)。
//!
//! 推理強度：[`ModelRequest::reasoning`] 有設定時送 `reasoning_effort`（`none`／`low`／`medium`／
//! `high`），沒設定時不送。GPT-5.4 以後的模型預設會推理，而 Chat Completions 不接受「工具 +
//! reasoning」。請求帶工具時若被以那個理由拒絕，會改成 `reasoning_effort: "none"` 重送一次，並記住
//! 這個 provider 之後帶工具的請求都直接這樣送（不管要求的強度）；連 `none` 都不接受的模型則回報
//! 要改用 Responses API。使用者自己選 `Off` 而被拒絕 `none` 時，提示改用其他強度。
//!
//! API key 只放在 `Authorization: Bearer` header，不會出現在 `Debug`、log 或錯誤訊息；
//! key 是空字串時不送 `Authorization`（例如本機的 Ollama）。

mod body;
mod stream;

use std::sync::atomic::{AtomicBool, Ordering};

use reqwest::header::{HeaderMap, HeaderName};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use url::Url;

pub use self::body::{PROVIDER_NAME, build_request_body, build_request_body_with_effort};
pub use self::stream::ChatCompletionsMapper;
use crate::api_common::{event_stream, read_json, send, with_extra_headers};
use crate::provider::{EventStream, LlmError, ModelProvider, ModelRequest, Reasoning};
pub use crate::responses::ModelInfo;
use crate::responses::endpoint;

/// OpenAI 官方的 base URL。
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

pub struct OpenAiCompatProvider {
    base_url: Url,
    api_key: String,
    http: reqwest::Client,
    /// 伺服器拒絕過「工具 + reasoning」（見模組說明）；之後帶工具的請求都直接關掉推理。
    tools_without_reasoning: AtomicBool,
    /// 使用者設定的額外 header，排在認證 header 後面。
    headers: HeaderMap,
    /// 有設定時，把 [`ModelRequest::session_id`] 放進這個 header。
    session_header: Option<HeaderName>,
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
            tools_without_reasoning: AtomicBool::new(false),
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

    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    /// `GET {base}/models` 的 `data[].id`，依伺服器順序；`display_name` 等於 ID。
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>, LlmError> {
        let url = endpoint(&self.base_url, "models");
        let request = with_extra_headers(
            self.authorized(self.http.get(url)),
            &self.headers,
            None,
            None,
        )?;
        let response = send(request, &self.api_key).await?;
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
        let session = request.session_id.clone();
        // 已經送 `none`（使用者選 `Off`）就不必再降級重送。
        let requested_off = request.reasoning == Some(Reasoning::Off);
        let mut downgraded =
            !request.tools.is_empty() && self.tools_without_reasoning.load(Ordering::Relaxed);
        let mut body = if downgraded {
            build_request_body_with_effort(&request, "none")
        } else {
            build_request_body(&request)
        };
        loop {
            let builder = self
                .authorized(self.http.post(url.clone()))
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .json(&body);
            let builder = with_extra_headers(
                builder,
                &self.headers,
                self.session_header.as_ref(),
                session.as_deref(),
            )?;
            let response = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(LlmError::Cancelled),
                response = send(builder, &self.api_key) => response,
            };
            match response {
                Ok(response) => {
                    return Ok(Box::pin(event_stream(
                        response,
                        ChatCompletionsMapper::new(),
                        cancel,
                        &self.api_key,
                    )));
                }
                Err(error) if !downgraded && !requested_off && tools_need_no_reasoning(&error) => {
                    tracing::warn!(
                        model = %request.model,
                        "Chat Completions 不接受工具與推理並用，改成 reasoning_effort=none 重送"
                    );
                    self.tools_without_reasoning.store(true, Ordering::Relaxed);
                    downgraded = true;
                    body = build_request_body_with_effort(&request, "none");
                }
                Err(error) => {
                    return Err(if downgraded {
                        explain_none_rejected(error, NONE_REJECTED_WITH_TOOLS)
                    } else if requested_off {
                        explain_none_rejected(error, NONE_REJECTED_OFF)
                    } else {
                        error
                    });
                }
            }
        }
    }
}

/// Chat Completions 以「工具 + reasoning」為由拒絕（GPT-5.4 以後的模型預設會推理）。
fn tools_need_no_reasoning(error: &LlmError) -> bool {
    matches!(error, LlmError::Unsupported(message)
        if message.contains("Function tools with reasoning_effort are not supported"))
}

/// 為了工具降級成 `none` 後仍被拒的提示。
const NONE_REJECTED_WITH_TOOLS: &str =
    "這個模型不能用 Chat Completions 搭配工具：請改用 ChatGPT 方案（Responses API）或換一個模型";

/// 使用者選 `Off` 而 `none` 被拒的提示。
const NONE_REJECTED_OFF: &str = "這個模型不能關掉推理：請把推理強度改成 low 以上或不設定";

/// 送 `reasoning_effort: "none"` 被拒：模型不接受 `none`。在訊息後面加上 `hint`。
fn explain_none_rejected(error: LlmError, hint: &str) -> LlmError {
    match &error {
        LlmError::Unsupported(message)
            if message.contains("reasoning_effort") && message.contains("none") =>
        {
            LlmError::Unsupported(format!("{message}（{hint}）"))
        }
        _ => error,
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
