//! SSE 解碼與 Responses 串流事件對應。兩者都是純狀態機，不碰網路，方便測試。

use serde_json::Value;

use super::body::PROVIDER_NAME;
use super::error::ApiError;
use crate::provider::{Item, LlmError, StreamEvent, ToolCall, Usage};

/// 一則 SSE 訊息。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SseMessage {
    pub event: Option<String>,
    pub data: String,
}

/// 逐塊餵入位元組、吐出完整 SSE 訊息的解碼器（`text/event-stream`）。
#[derive(Debug, Default)]
pub struct SseDecoder {
    line: Vec<u8>,
    event: Option<String>,
    data: Option<String>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 餵入一塊資料，回傳這塊資料裡完成的訊息。訊息可以跨塊。
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseMessage> {
        let mut messages = Vec::new();
        for &byte in chunk {
            if byte == b'\n' {
                let mut line = std::mem::take(&mut self.line);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if let Some(message) = self.line_done(&String::from_utf8_lossy(&line)) {
                    messages.push(message);
                }
            } else {
                self.line.push(byte);
            }
        }
        messages
    }

    /// 串流結束：送出最後一則沒有以空行結尾的訊息。
    pub fn finish(&mut self) -> Option<SseMessage> {
        if !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            // 最後一行沒有換行：照樣處理欄位，但不會因此觸發送出。
            let _ = self.line_done(&String::from_utf8_lossy(&line));
        }
        self.dispatch()
    }

    fn line_done(&mut self, line: &str) -> Option<SseMessage> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => {
                let data = self.data.get_or_insert_with(String::new);
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(value);
            }
            _ => {}
        }
        None
    }

    fn dispatch(&mut self) -> Option<SseMessage> {
        let event = self.event.take();
        let data = self.data.take()?;
        Some(SseMessage { event, data })
    }
}

/// 把 Responses 的串流事件轉成 [`StreamEvent`]。
///
/// 只有 `response.completed` 是成功；`response.failed`、`response.incomplete`、`error`
/// 都是失敗。終止事件之後的訊息一律忽略。
#[derive(Debug, Default)]
pub struct ResponsesEventMapper {
    terminated: bool,
}

impl ResponsesEventMapper {
    pub fn new() -> Self {
        Self::default()
    }

    /// 已經收到終止事件（成功或失敗）。
    pub fn is_terminated(&self) -> bool {
        self.terminated
    }

    /// 處理一則 SSE 訊息，回傳要送出的事件（通常 0 或 1 個）。
    pub fn handle(&mut self, message: &SseMessage) -> Option<Result<StreamEvent, LlmError>> {
        if self.terminated || message.data.trim() == "[DONE]" {
            return None;
        }
        let value: Value = match serde_json::from_str(&message.data) {
            Ok(value) => value,
            Err(error) => {
                self.terminated = true;
                return Some(Err(LlmError::Interrupted(format!(
                    "無法解析串流事件：{error}"
                ))));
            }
        };
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .or(message.event.as_deref())
            .unwrap_or_default();
        match kind {
            "response.output_text.delta" => value
                .get("delta")
                .and_then(Value::as_str)
                .filter(|delta| !delta.is_empty())
                .map(|delta| Ok(StreamEvent::TextDelta(delta.to_owned()))),
            "response.output_item.done" => value
                .get("item")
                .and_then(output_item)
                .map(|item| Ok(StreamEvent::ItemDone(item))),
            "response.completed" => {
                self.terminated = true;
                let usage = value
                    .pointer("/response/usage")
                    .map(parse_usage)
                    .unwrap_or_default();
                Some(Ok(StreamEvent::Completed { usage }))
            }
            "response.failed" => {
                self.terminated = true;
                let error = value
                    .pointer("/response/error")
                    .filter(|e| !e.is_null())
                    .map(ApiError::from_json)
                    .unwrap_or_else(|| ApiError {
                        message: Some("response.failed（沒有錯誤內容）".into()),
                        ..ApiError::default()
                    });
                Some(Err(error.into_llm_error()))
            }
            "response.incomplete" => {
                self.terminated = true;
                let reason = value
                    .pointer("/response/incomplete_details/reason")
                    .and_then(Value::as_str)
                    .unwrap_or("未知原因");
                Some(Err(LlmError::Other(format!("回應未完成：{reason}"))))
            }
            "error" => {
                self.terminated = true;
                Some(Err(ApiError::from_json(&value).into_llm_error()))
            }
            _ => None,
        }
    }
}

/// 一個完成的輸出項目對應的 [`Item`]。沒有文字的 message 略過。
fn output_item(item: &Value) -> Option<Item> {
    match item.get("type").and_then(Value::as_str) {
        Some("message") => {
            let text: String = item
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                    Some("output_text") => part.get("text").and_then(Value::as_str),
                    Some("refusal") => part.get("refusal").and_then(Value::as_str),
                    _ => None,
                })
                .collect();
            (!text.is_empty()).then_some(Item::Assistant { text })
        }
        Some("function_call") => {
            let text = |key: &str| item.get(key).and_then(Value::as_str).map(str::to_owned);
            Some(Item::ToolCall(ToolCall {
                call_id: text("call_id")?,
                name: text("name")?,
                arguments: text("arguments").unwrap_or_else(|| "{}".into()),
            }))
        }
        // reasoning 與其他 provider 專屬項目原樣保存，下一輪回送。
        _ => Some(Item::Opaque {
            provider: PROVIDER_NAME.into(),
            value: item.clone(),
        }),
    }
}

fn parse_usage(usage: &Value) -> Usage {
    let number = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or_default();
    Usage {
        input_tokens: number("input_tokens"),
        output_tokens: number("output_tokens"),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn sse(event: &str, data: Value) -> String {
        format!("event: {event}\ndata: {data}\n\n")
    }

    #[test]
    fn decoder_handles_split_chunks_and_crlf() {
        let mut decoder = SseDecoder::new();
        let input = ": keep-alive\r\nevent: a\r\ndata: {\"x\":\r\ndata: 1}\r\n\r\ndata: tail";
        let mut messages = Vec::new();
        for chunk in input.as_bytes().chunks(3) {
            messages.extend(decoder.push(chunk));
        }
        assert_eq!(
            messages,
            vec![SseMessage {
                event: Some("a".into()),
                data: "{\"x\":\n1}".into()
            }]
        );
        assert_eq!(
            decoder.finish(),
            Some(SseMessage {
                event: None,
                data: "tail".into()
            })
        );
        assert_eq!(decoder.finish(), None);
    }

    #[test]
    fn decoder_keeps_multibyte_text_across_chunks() {
        let mut decoder = SseDecoder::new();
        let input = "data: 你好\n\n".as_bytes();
        let (a, b) = input.split_at(8);
        assert!(decoder.push(a).is_empty());
        assert_eq!(decoder.push(b)[0].data, "你好");
    }

    fn map_all(stream: &str) -> Vec<Result<StreamEvent, LlmError>> {
        let mut decoder = SseDecoder::new();
        let mut mapper = ResponsesEventMapper::new();
        decoder
            .push(stream.as_bytes())
            .iter()
            .filter_map(|message| mapper.handle(message))
            .collect()
    }

    #[test]
    fn maps_text_tool_call_reasoning_and_completion() {
        let reasoning =
            json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "enc"});
        let stream = [
            sse("response.created", json!({"type": "response.created", "response": {}})),
            sse("response.output_item.done", json!({"type": "response.output_item.done", "item": reasoning})),
            sse("response.output_text.delta", json!({"type": "response.output_text.delta", "delta": "嗨"})),
            sse(
                "response.output_item.done",
                json!({"type": "response.output_item.done", "item": {
                    "type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": "嗨"}]
                }}),
            ),
            sse(
                "response.output_item.done",
                json!({"type": "response.output_item.done", "item": {
                    "type": "function_call", "namespace": "pots", "name": "get_time",
                    "call_id": "c1", "arguments": "{}"
                }}),
            ),
            sse(
                "response.completed",
                json!({"type": "response.completed", "response": {"usage": {"input_tokens": 7, "output_tokens": 3}}}),
            ),
            sse("response.output_text.delta", json!({"type": "response.output_text.delta", "delta": "late"})),
        ]
        .concat();
        assert_eq!(
            map_all(&stream),
            vec![
                Ok(StreamEvent::ItemDone(Item::Opaque {
                    provider: PROVIDER_NAME.into(),
                    value: reasoning
                })),
                Ok(StreamEvent::TextDelta("嗨".into())),
                Ok(StreamEvent::ItemDone(Item::Assistant {
                    text: "嗨".into()
                })),
                Ok(StreamEvent::ItemDone(Item::ToolCall(ToolCall {
                    call_id: "c1".into(),
                    name: "get_time".into(),
                    arguments: "{}".into()
                }))),
                Ok(StreamEvent::Completed {
                    usage: Usage {
                        input_tokens: 7,
                        output_tokens: 3
                    }
                }),
            ]
        );
    }

    #[test]
    fn maps_failures() {
        let failed = sse(
            "response.failed",
            json!({"type": "response.failed", "response": {"error": {"code": "subscription_sharing_usage_limit_exceeded", "message": "limit"}}}),
        );
        assert_eq!(map_all(&failed), vec![Err(LlmError::UsageLimit)]);

        let incomplete = sse(
            "response.incomplete",
            json!({"type": "response.incomplete", "response": {"incomplete_details": {"reason": "content_filter"}}}),
        );
        assert!(
            matches!(&map_all(&incomplete)[..], [Err(LlmError::Other(m))] if m.contains("content_filter"))
        );

        let error = sse(
            "error",
            json!({"type": "error", "code": "subscription_sharing_usage_unavailable", "message": "later"}),
        );
        assert!(matches!(
            &map_all(&error)[..],
            [Err(LlmError::Unavailable(_))]
        ));

        assert!(matches!(
            &map_all("data: {oops\n\n")[..],
            [Err(LlmError::Interrupted(_))]
        ));
    }
}
