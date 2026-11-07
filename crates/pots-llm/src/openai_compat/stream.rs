//! Chat Completions 串流（`data:` chunk）對應到 [`StreamEvent`]。純狀態機，不碰網路。

use std::collections::BTreeMap;

use serde_json::Value;

use crate::api_common::{
    EventMapper, TRUNCATED_OUTPUT, arguments_complete, parse_event, stream_error,
};
use crate::provider::{Item, LlmError, StreamEvent, ToolCall, Usage};
use crate::responses::SseMessage;

/// 串流中還沒完成的工具呼叫。
#[derive(Debug, Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

/// 把 Chat Completions 的串流 chunk 轉成 [`StreamEvent`]。
///
/// 文字增量即時送出；工具呼叫依 `index` 累積。收到 `data: [DONE]` 時依序送出助理文字、
/// 各工具呼叫（依 `index`）的 [`StreamEvent::ItemDone`]，再送 [`StreamEvent::Completed`]。
/// 有些伺服器送完 `finish_reason` 就直接關閉連線、不送 `[DONE]`，這也算完成；
/// 兩者都沒有就結束的串流是 [`LlmError::Interrupted`]。
#[derive(Debug, Default)]
pub struct ChatCompletionsMapper {
    text: String,
    calls: BTreeMap<u64, PartialCall>,
    finish_reason: Option<String>,
    usage: Usage,
    terminated: bool,
}

impl ChatCompletionsMapper {
    pub fn new() -> Self {
        Self::default()
    }

    /// 已經收到終止事件（成功或失敗）。
    pub fn is_terminated(&self) -> bool {
        self.terminated
    }

    /// 處理一則 SSE 訊息，回傳要送出的事件。
    pub fn handle(&mut self, message: &SseMessage) -> Vec<Result<StreamEvent, LlmError>> {
        let mut out = Vec::new();
        EventMapper::handle(self, message, &mut out);
        out
    }

    /// 連線結束時呼叫，回傳最後的事件。
    pub fn finish(&mut self) -> Vec<Result<StreamEvent, LlmError>> {
        let mut out = Vec::new();
        EventMapper::finish(self, &mut out);
        out
    }

    fn chunk(&mut self, value: &Value, out: &mut Vec<Result<StreamEvent, LlmError>>) {
        if let Some(usage) = value.get("usage").filter(|u| u.is_object()) {
            let number = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or_default();
            self.usage = Usage {
                input_tokens: number("prompt_tokens"),
                output_tokens: number("completion_tokens"),
            };
        }
        let Some(choice) = value.pointer("/choices/0") else {
            return;
        };
        if let Some(delta) = choice.get("delta") {
            if let Some(text) = delta
                .get("content")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                self.text.push_str(text);
                out.push(Ok(StreamEvent::TextDelta(text.to_owned())));
            }
            for call in delta
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                self.tool_call_delta(call);
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_owned());
        }
    }

    fn tool_call_delta(&mut self, call: &Value) {
        let text = |pointer: &str| {
            call.pointer(pointer)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        };
        let id = text("/id");
        let index = match call.get("index").and_then(Value::as_u64) {
            Some(index) => index,
            // 沒有 index 的伺服器：新的 id 開新的呼叫，否則接在最後一個後面。
            None => {
                let last = self.calls.keys().next_back().copied();
                let known = id.is_some_and(|id| self.calls.values().any(|c| c.id == id));
                match last {
                    Some(last) if known || id.is_none() => last,
                    Some(last) => last + 1,
                    None => 0,
                }
            }
        };
        let partial = self.calls.entry(index).or_default();
        if let Some(id) = id
            && partial.id.is_empty()
        {
            partial.id = id.to_owned();
        }
        if let Some(name) = text("/function/name")
            && partial.name.is_empty()
        {
            partial.name = name.to_owned();
        }
        if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str) {
            partial.arguments.push_str(arguments);
        }
    }

    /// 送出這一輪的項目與完成事件。`finish_reason: "length"`（達到 max tokens）而且有工具呼叫的
    /// 參數不是合法 JSON 時，整輪以 [`LlmError::Interrupted`] 失敗，不把被截斷的參數交給工具。
    fn complete(&mut self, out: &mut Vec<Result<StreamEvent, LlmError>>) {
        self.terminated = true;
        if self.finish_reason.as_deref() == Some("length") {
            if self
                .calls
                .values()
                .any(|call| !arguments_complete(&call.arguments))
            {
                out.push(Err(LlmError::Interrupted(TRUNCATED_OUTPUT.into())));
                return;
            }
            tracing::warn!("Chat Completions 輸出達到 max tokens 被截斷（finish_reason: length）");
        }
        let text = std::mem::take(&mut self.text);
        if !text.is_empty() {
            out.push(Ok(StreamEvent::ItemDone(Item::Assistant { text })));
        }
        for (index, call) in std::mem::take(&mut self.calls) {
            let call_id = if call.id.is_empty() {
                format!("call_{index}_{}", uuid::Uuid::new_v4().simple())
            } else {
                call.id
            };
            let arguments = if call.arguments.trim().is_empty() {
                "{}".to_owned()
            } else {
                call.arguments
            };
            out.push(Ok(StreamEvent::ItemDone(Item::ToolCall(ToolCall {
                call_id,
                name: call.name,
                arguments,
            }))));
        }
        out.push(Ok(StreamEvent::Completed { usage: self.usage }));
    }
}

impl EventMapper for ChatCompletionsMapper {
    fn handle(&mut self, message: &SseMessage, out: &mut Vec<Result<StreamEvent, LlmError>>) {
        if self.terminated {
            return;
        }
        let data = message.data.trim();
        if data.is_empty() {
            return;
        }
        if data == "[DONE]" {
            if self.finish_reason.is_none() && self.text.is_empty() && self.calls.is_empty() {
                tracing::debug!("Chat Completions 串流沒有任何輸出就送了 [DONE]");
            }
            self.complete(out);
            return;
        }
        let value = match parse_event(data) {
            Ok(value) => value,
            Err(error) => {
                self.terminated = true;
                out.push(Err(error));
                return;
            }
        };
        if value.get("error").is_some_and(|e| !e.is_null()) {
            self.terminated = true;
            out.push(Err(stream_error(&value)));
            return;
        }
        self.chunk(&value, out);
    }

    fn finish(&mut self, out: &mut Vec<Result<StreamEvent, LlmError>>) {
        if self.terminated {
            return;
        }
        if self.finish_reason.is_some() {
            self.complete(out);
        } else {
            self.terminated = true;
            out.push(Err(LlmError::Interrupted(
                "串流在 finish_reason／[DONE] 之前結束".into(),
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::responses::SseDecoder;

    fn chunk(value: Value) -> String {
        format!("data: {value}\n\n")
    }

    fn map_all(stream: &str, eof: bool) -> Vec<Result<StreamEvent, LlmError>> {
        let mut decoder = SseDecoder::new();
        let mut mapper = ChatCompletionsMapper::new();
        let mut events: Vec<_> = decoder
            .push(stream.as_bytes())
            .iter()
            .flat_map(|message| mapper.handle(message))
            .collect();
        if eof {
            events.extend(mapper.finish());
        }
        events
    }

    fn delta(delta: Value) -> String {
        chunk(json!({"choices": [{"index": 0, "delta": delta, "finish_reason": null}]}))
    }

    fn finish(reason: &str) -> String {
        chunk(json!({"choices": [{"index": 0, "delta": {}, "finish_reason": reason}]}))
    }

    fn usage(prompt: u64, completion: u64) -> String {
        chunk(
            json!({"choices": [], "usage": {"prompt_tokens": prompt, "completion_tokens": completion}}),
        )
    }

    #[test]
    fn text_tool_calls_and_usage() {
        let stream = [
            delta(json!({"role": "assistant", "content": ""})),
            delta(json!({"content": "我查"})),
            delta(json!({"content": "一下"})),
            delta(
                json!({"tool_calls": [{"index": 1, "id": "c2", "type": "function", "function": {"name": "weather", "arguments": ""}}]}),
            ),
            delta(
                json!({"tool_calls": [{"index": 0, "id": "c1", "type": "function", "function": {"name": "get_time", "arguments": "{\"tz\""}}]}),
            ),
            delta(json!({"tool_calls": [{"index": 0, "function": {"arguments": ":\"UTC\"}"}}]})),
            finish("tool_calls"),
            usage(9, 4),
            "data: [DONE]\n\n".into(),
            delta(json!({"content": "late"})),
        ]
        .concat();
        assert_eq!(
            map_all(&stream, true),
            vec![
                Ok(StreamEvent::TextDelta("我查".into())),
                Ok(StreamEvent::TextDelta("一下".into())),
                Ok(StreamEvent::ItemDone(Item::Assistant {
                    text: "我查一下".into()
                })),
                Ok(StreamEvent::ItemDone(Item::ToolCall(ToolCall {
                    call_id: "c1".into(),
                    name: "get_time".into(),
                    arguments: "{\"tz\":\"UTC\"}".into()
                }))),
                Ok(StreamEvent::ItemDone(Item::ToolCall(ToolCall {
                    call_id: "c2".into(),
                    name: "weather".into(),
                    arguments: "{}".into()
                }))),
                Ok(StreamEvent::Completed {
                    usage: Usage {
                        input_tokens: 9,
                        output_tokens: 4
                    }
                }),
            ]
        );
    }

    #[test]
    fn calls_without_index_split_by_id() {
        let stream = [
            delta(
                json!({"tool_calls": [{"id": "a", "function": {"name": "x", "arguments": "{}"}}]}),
            ),
            delta(
                json!({"tool_calls": [{"id": "b", "function": {"name": "y", "arguments": "{\"k\":"}}]}),
            ),
            delta(json!({"tool_calls": [{"function": {"arguments": "1}"}}]})),
            finish("tool_calls"),
        ]
        .concat();
        let calls: Vec<_> = map_all(&stream, true)
            .into_iter()
            .filter_map(|event| match event {
                Ok(StreamEvent::ItemDone(Item::ToolCall(call))) => Some(call),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            (calls[0].call_id.as_str(), calls[0].arguments.as_str()),
            ("a", "{}")
        );
        assert_eq!(
            (calls[1].call_id.as_str(), calls[1].arguments.as_str()),
            ("b", "{\"k\":1}")
        );
    }

    #[test]
    fn truncated_tool_call_fails_turn() {
        let truncated = [
            delta(json!({"content": "我查"})),
            delta(
                json!({"tool_calls": [{"index": 0, "id": "c1", "function": {"name": "get_time", "arguments": "{\"tz\":"}}]}),
            ),
            finish("length"),
            "data: [DONE]\n\n".into(),
        ]
        .concat();
        assert_eq!(
            map_all(&truncated, true),
            vec![
                Ok(StreamEvent::TextDelta("我查".into())),
                Err(LlmError::Interrupted(
                    "輸出被截斷（達到 max tokens）".into()
                )),
            ]
        );

        // 參數完整的工具呼叫、只有文字：照常完成。
        let complete_call = [
            delta(
                json!({"tool_calls": [{"index": 0, "id": "c1", "function": {"name": "get_time", "arguments": "{}"}}]}),
            ),
            finish("length"),
        ]
        .concat();
        let text_only = [delta(json!({"content": "一半"})), finish("length")].concat();
        for stream in [complete_call, text_only] {
            let events = map_all(&stream, true);
            assert!(matches!(
                events.last(),
                Some(Ok(StreamEvent::Completed { .. }))
            ));
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e, Ok(StreamEvent::ItemDone(_))))
            );
        }
    }

    #[test]
    fn finish_reason_without_done_is_complete() {
        let stream = [delta(json!({"content": "hi"})), finish("stop")].concat();
        let events = map_all(&stream, true);
        assert_eq!(
            events.last(),
            Some(&Ok(StreamEvent::Completed {
                usage: Usage::default()
            }))
        );
        assert!(events.contains(&Ok(StreamEvent::ItemDone(Item::Assistant {
            text: "hi".into()
        }))));
    }

    #[test]
    fn eof_without_finish_is_interrupted() {
        let events = map_all(&delta(json!({"content": "一半"})), true);
        assert_eq!(events.len(), 2);
        assert!(matches!(events[1], Err(LlmError::Interrupted(_))));
    }

    #[test]
    fn error_object_and_garbage_end_stream() {
        let stream = [
            delta(json!({"content": "x"})),
            chunk(json!({"error": {"message": "Rate limit", "code": "rate_limit_exceeded"}})),
            finish("stop"),
        ]
        .concat();
        let events = map_all(&stream, true);
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[1], Err(LlmError::Unavailable(m)) if m.contains("Rate limit")));

        assert!(matches!(
            &map_all("data: {oops\n\n", true)[..],
            [Err(LlmError::Interrupted(_))]
        ));
    }
}
