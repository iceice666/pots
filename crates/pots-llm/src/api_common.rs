//! API key provider（[`crate::openai_compat`]、[`crate::anthropic`]）共用的部分：
//! HTTP 錯誤對應、送出請求，以及把 SSE 位元組串流轉成 [`StreamEvent`] 的驅動。
//!
//! 錯誤訊息保留伺服器的錯誤類型與訊息（截斷）方便診斷，並把 API key 遮掉；
//! 連線錯誤拿掉 URL。

use std::collections::VecDeque;

use futures::StreamExt;
use futures::stream::BoxStream;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::provider::{LlmError, StreamEvent};
use crate::responses::{SseDecoder, SseMessage};

/// 錯誤訊息的最大字元數。
const MAX_MESSAGE_CHARS: usize = 300;

/// 把 SSE 訊息轉成 [`StreamEvent`] 的純狀態機。
pub(crate) trait EventMapper: Send + 'static {
    /// 處理一則訊息，把要送出的事件推進 `out`。送出 `Err` 或 [`StreamEvent::Completed`]
    /// 之後串流就結束。
    fn handle(&mut self, message: &SseMessage, out: &mut Vec<Result<StreamEvent, LlmError>>);

    /// 連線在終止事件之前結束：送出完成事件（伺服器沒送終止標記但其實已完成），
    /// 或 [`LlmError::Interrupted`]。
    fn finish(&mut self, out: &mut Vec<Result<StreamEvent, LlmError>>);
}

struct StreamState<M> {
    bytes: BoxStream<'static, reqwest::Result<bytes::Bytes>>,
    decoder: SseDecoder,
    mapper: M,
    pending: VecDeque<Result<StreamEvent, LlmError>>,
    cancel: CancellationToken,
    secret: String,
    done: bool,
}

impl<M: EventMapper> StreamState<M> {
    fn feed(&mut self, messages: impl IntoIterator<Item = SseMessage>) {
        let mut out = Vec::new();
        for message in messages {
            self.mapper.handle(&message, &mut out);
        }
        self.pending.extend(out);
    }
}

/// 位元組 → SSE 訊息 → [`StreamEvent`]。`cancel` 觸發時以 [`LlmError::Cancelled`] 結束；
/// 錯誤訊息裡出現 `secret` 時遮掉。
pub(crate) fn event_stream<M: EventMapper>(
    response: reqwest::Response,
    mapper: M,
    cancel: CancellationToken,
    secret: &str,
) -> impl futures::Stream<Item = Result<StreamEvent, LlmError>> + Send + use<M> {
    let state = StreamState {
        bytes: response.bytes_stream().boxed(),
        decoder: SseDecoder::new(),
        mapper,
        pending: VecDeque::new(),
        cancel,
        secret: secret.to_owned(),
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
                let event = event.map_err(|error| redact_error(error, &state.secret));
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
                        Err(LlmError::Interrupted(format!(
                            "讀取串流失敗：{}",
                            error.without_url()
                        ))),
                        state,
                    ));
                }
                None => {
                    let last = state.decoder.finish();
                    state.feed(last);
                    let mut out = Vec::new();
                    state.mapper.finish(&mut out);
                    state.pending.extend(out);
                    // 剩下的事件送完就結束。
                    state.done = true;
                }
            }
        }
    })
}

/// 加上使用者設定的額外 header，以及（有設定時）這次對話的 session header。
///
/// `headers` 已經排在最前面，所以使用者可以在裡面覆寫 provider 的預設值；session header
/// 只在 `headers` 沒有同名項目時才補上。
pub(crate) fn with_extra_headers(
    request: reqwest::RequestBuilder,
    headers: &HeaderMap,
    session_header: Option<&HeaderName>,
    session: Option<&str>,
) -> Result<reqwest::RequestBuilder, LlmError> {
    let mut request = request.headers(headers.clone());
    if let (Some(name), Some(session)) = (session_header, session)
        && !session.is_empty()
        && !headers.contains_key(name)
    {
        let value = HeaderValue::from_str(session)
            .map_err(|_| LlmError::Unsupported("session ID 含有無效字元".into()))?;
        request = request.header(name, value);
    }
    Ok(request)
}

/// 送出請求；連線錯誤轉成 [`LlmError::Unavailable`]（不含 URL）。非 2xx 轉成對應的錯誤。
pub(crate) async fn send(
    request: reqwest::RequestBuilder,
    secret: &str,
) -> Result<reqwest::Response, LlmError> {
    let response = request
        .send()
        .await
        .map_err(|error| LlmError::Unavailable(format!("連線失敗：{}", error.without_url())))?;
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    Err(http_error(status, &body, secret))
}

/// 讀取 2xx 回應的 JSON body。
pub(crate) async fn read_json(response: reqwest::Response, what: &str) -> Result<Value, LlmError> {
    response
        .json()
        .await
        .map_err(|error| LlmError::Unavailable(format!("無法讀取{what}：{}", error.without_url())))
}

/// HTTP 錯誤回應（狀態碼 + body）對應到 [`LlmError`]。
pub(crate) fn http_error(status: u16, body: &str, secret: &str) -> LlmError {
    let parsed = match serde_json::from_str::<Value>(body) {
        Ok(value) => ErrorBody::from_json(&value),
        Err(_) => ErrorBody {
            code: None,
            message: Some(body.trim().to_owned()).filter(|s| !s.is_empty()),
        },
    };
    let error = classify(Some(status), &parsed, mentions_quota(body));
    redact_error(error, secret)
}

/// 串流中的錯誤物件（`{"error": {...}}` 或 Anthropic 的 `error` 事件）對應到 [`LlmError`]。
pub(crate) fn stream_error(value: &Value) -> LlmError {
    let parsed = ErrorBody::from_json(value);
    // OpenRouter 等會在串流錯誤裡放數字的 HTTP 狀態碼。
    let status = value
        .pointer("/error/code")
        .and_then(Value::as_u64)
        .and_then(|code| u16::try_from(code).ok())
        .filter(|code| (400..600).contains(code));
    classify(status, &parsed, mentions_quota(&value.to_string()))
}

/// 錯誤 body 裡的類型與訊息。
#[derive(Debug, Default)]
struct ErrorBody {
    /// OpenAI 的 `error.code`／`error.type`、Anthropic 的 `error.type`。
    code: Option<String>,
    message: Option<String>,
}

impl ErrorBody {
    /// 支援 `{"error": {"message", "code"|"type"}}`、`{"error": "..."}`（Ollama）、
    /// `{"message": "..."}` 與 `{"detail": "..."}`。
    fn from_json(value: &Value) -> Self {
        let text = |value: &Value, key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        match value.get("error") {
            Some(Value::String(message)) => Self {
                code: None,
                message: Some(message.clone()),
            },
            Some(error @ Value::Object(_)) => Self {
                code: text(error, "code")
                    .or_else(|| text(error, "type"))
                    .filter(|code| code != "error"),
                message: text(error, "message"),
            },
            _ => Self {
                code: None,
                message: text(value, "message").or_else(|| text(value, "detail")),
            },
        }
    }
}

/// body 提到額度或帳單問題（OpenAI 的 `insufficient_quota`、Anthropic 的 credit balance）。
fn mentions_quota(body: &str) -> bool {
    let body = body.to_lowercase();
    ["insufficient_quota", "billing", "credit balance"]
        .iter()
        .any(|needle| body.contains(needle))
}

fn classify(status: Option<u16>, body: &ErrorBody, quota: bool) -> LlmError {
    let detail = detail(status, body);
    if quota && matches!(status, None | Some(402 | 429)) {
        return LlmError::Other(format!("API 額度不足：{detail}"));
    }
    match status {
        Some(401 | 403) => LlmError::InvalidCredentials(detail),
        Some(402) => LlmError::Other(format!("API 額度不足：{detail}")),
        Some(429) => LlmError::Unavailable(format!("速率限制或額度不足：{detail}")),
        Some(400 | 404 | 413 | 422) => LlmError::Unsupported(detail),
        Some(408 | 500..=599) => LlmError::Unavailable(detail),
        Some(_) => LlmError::Other(detail),
        None => match body.code.as_deref() {
            Some("authentication_error" | "permission_error" | "invalid_api_key") => {
                LlmError::InvalidCredentials(detail)
            }
            Some("rate_limit_error" | "rate_limit_exceeded") => {
                LlmError::Unavailable(format!("速率限制或額度不足：{detail}"))
            }
            Some("overloaded_error" | "api_error" | "server_error" | "timeout_error") => {
                LlmError::Unavailable(detail)
            }
            Some(
                "invalid_request_error"
                | "not_found_error"
                | "model_not_found"
                | "request_too_large"
                | "context_length_exceeded",
            ) => LlmError::Unsupported(detail),
            _ => LlmError::Other(detail),
        },
    }
}

fn detail(status: Option<u16>, body: &ErrorBody) -> String {
    let mut detail = String::new();
    if let Some(status) = status {
        detail.push_str(&format!("HTTP {status}"));
    }
    if let Some(code) = &body.code {
        if !detail.is_empty() {
            detail.push(' ');
        }
        detail.push_str(code);
    }
    if let Some(message) = &body.message {
        if !detail.is_empty() {
            detail.push('：');
        }
        detail.push_str(&truncate(message.trim(), MAX_MESSAGE_CHARS));
    }
    if detail.is_empty() {
        "未知錯誤".into()
    } else {
        detail
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((index, _)) => format!("{}…", &text[..index]),
        None => text.to_owned(),
    }
}

/// 把錯誤訊息裡的 `secret` 換成 `***`（伺服器有時會回顯收到的 key）。
pub(crate) fn redact_error(error: LlmError, secret: &str) -> LlmError {
    if secret.is_empty() {
        return error;
    }
    let redact = |message: String| message.replace(secret, "***");
    match error {
        LlmError::NotEntitled(m) => LlmError::NotEntitled(redact(m)),
        LlmError::Unavailable(m) => LlmError::Unavailable(redact(m)),
        LlmError::Unsupported(m) => LlmError::Unsupported(redact(m)),
        LlmError::Interrupted(m) => LlmError::Interrupted(redact(m)),
        LlmError::InvalidCredentials(m) => LlmError::InvalidCredentials(redact(m)),
        LlmError::Other(m) => LlmError::Other(redact(m)),
        other => other,
    }
}

/// 輸出達到 max tokens、工具呼叫的參數被截斷時的錯誤訊息。
pub(crate) const TRUNCATED_OUTPUT: &str = "輸出被截斷（達到 max tokens）";

/// 工具參數是完整的 JSON（空字串視為 `{}`）。
pub(crate) fn arguments_complete(arguments: &str) -> bool {
    arguments.trim().is_empty() || serde_json::from_str::<Value>(arguments).is_ok()
}

/// 解析 JSON 字串值；錯誤時以 [`LlmError::Interrupted`] 結束串流。
pub(crate) fn parse_event(data: &str) -> Result<Value, LlmError> {
    serde_json::from_str(data)
        .map_err(|error| LlmError::Interrupted(format!("無法解析串流事件：{error}")))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn http(status: u16, body: Value) -> LlmError {
        http_error(status, &body.to_string(), "sk-secret")
    }

    #[test]
    fn status_mapping() {
        let openai_key = json!({"error": {
            "message": "Incorrect API key provided: sk-secret", "type": "invalid_request_error",
            "code": "invalid_api_key"
        }});
        let error = http(401, openai_key);
        assert!(
            matches!(&error, LlmError::InvalidCredentials(m) if m.contains("invalid_api_key") && m.contains("***") && !m.contains("sk-secret")),
            "{error:?}"
        );
        assert!(matches!(
            http(403, json!({"type": "error", "error": {"type": "permission_error", "message": "no"}})),
            LlmError::InvalidCredentials(m) if m == "HTTP 403 permission_error：no"
        ));
        assert!(matches!(
            http(404, json!({"error": {"message": "The model `x` does not exist", "code": "model_not_found"}})),
            LlmError::Unsupported(m) if m.contains("does not exist")
        ));
        assert!(matches!(
            http(400, json!({"error": "model \"x\" not found, try pulling it first"})),
            LlmError::Unsupported(m) if m.contains("pulling")
        ));
        assert!(matches!(
            http(422, json!({"detail": "bad"})),
            LlmError::Unsupported(m) if m.contains("bad")
        ));
        assert!(matches!(
            http(429, json!({"error": {"message": "slow down", "code": "rate_limit_exceeded"}})),
            LlmError::Unavailable(m) if m.starts_with("速率限制或額度不足：")
        ));
        assert!(matches!(
            http(429, json!({"error": {"message": "You exceeded your current quota", "code": "insufficient_quota"}})),
            LlmError::Other(m) if m.starts_with("API 額度不足：")
        ));
        assert!(matches!(
            http(
                400,
                json!({"type": "error", "error": {"type": "invalid_request_error", "message": "Your credit balance is too low"}})
            ),
            LlmError::Unsupported(_)
        ));
        assert!(matches!(
            http(529, json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}})),
            LlmError::Unavailable(m) if m.contains("Overloaded")
        ));
        assert!(matches!(
            http_error(502, "<html>bad gateway</html>", ""),
            LlmError::Unavailable(m) if m.contains("bad gateway")
        ));
    }

    #[test]
    fn long_messages_are_truncated() {
        let long = "x".repeat(1000);
        let LlmError::Unavailable(message) = http_error(500, &long, "") else {
            panic!()
        };
        assert!(message.chars().count() < 320, "{message}");
        assert!(message.ends_with('…'));
    }

    #[test]
    fn stream_errors_map_by_type() {
        let error = |value: Value| stream_error(&json!({"type": "error", "error": value}));
        assert!(matches!(
            error(json!({"type": "overloaded_error", "message": "Overloaded"})),
            LlmError::Unavailable(_)
        ));
        assert!(matches!(
            error(json!({"type": "authentication_error", "message": "bad key"})),
            LlmError::InvalidCredentials(_)
        ));
        assert!(matches!(
            error(json!({"type": "invalid_request_error", "message": "bad"})),
            LlmError::Unsupported(_)
        ));
        assert!(matches!(
            error(json!({"type": "billing_error", "message": "billing"})),
            LlmError::Other(m) if m.starts_with("API 額度不足")
        ));
        assert!(matches!(
            stream_error(&json!({"error": {"code": 502, "message": "upstream"}})),
            LlmError::Unavailable(m) if m.contains("upstream")
        ));
        assert!(matches!(
            stream_error(&json!({"error": {"message": "weird"}})),
            LlmError::Other(m) if m == "weird"
        ));
    }
}
