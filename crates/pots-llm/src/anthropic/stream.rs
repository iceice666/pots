//! Anthropic Messages 串流事件對應到 [`StreamEvent`]。純狀態機，不碰網路。

use std::collections::HashMap;

use serde_json::Value;

use crate::api_common::{
    EventMapper, TRUNCATED_OUTPUT, arguments_complete, parse_event, stream_error,
};
use crate::provider::{Item, LlmError, StreamEvent, ToolCall, Usage};
use crate::responses::SseMessage;

/// 進行中的 content block。
#[derive(Debug)]
enum Block {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        /// `content_block_start` 就帶了完整 `input`（非空物件）的相容伺服器。
        initial: Option<Value>,
        /// 累積的 `input_json_delta.partial_json`。
        json: String,
    },
    /// thinking 等不處理的 block。
    Ignored,
}

/// 把 Messages API 的串流事件轉成 [`StreamEvent`]。
///
/// `text_delta` 即時送出；每個 block 在 `content_block_stop` 時送出 [`StreamEvent::ItemDone`]
/// （文字或工具呼叫，工具參數是累積的 `partial_json`，空的時候是 `"{}"`）。只有 `message_stop`
/// 是成功；`error` 事件是失敗；在那之前結束的串流是 [`LlmError::Interrupted`]。
///
/// `stop_reason: "max_tokens"` 而且有工具呼叫的參數不是合法 JSON 時，`message_stop` 改送
/// [`LlmError::Interrupted`]：agent loop 會丟掉這一輪已送出的項目，被截斷的參數不會交給工具。
#[derive(Debug, Default)]
pub struct MessagesEventMapper {
    blocks: HashMap<u64, Block>,
    /// 已送出的工具呼叫中有參數不是合法 JSON 的。
    incomplete_call: bool,
    usage: Usage,
    stop_reason: Option<String>,
    terminated: bool,
}

impl MessagesEventMapper {
    pub fn new() -> Self {
        Self::default()
    }

    /// 已經收到終止事件（成功或失敗）。
    pub fn is_terminated(&self) -> bool {
        self.terminated
    }

    /// `message_delta` 帶來的停止原因（例如 `end_turn`、`tool_use`、`max_tokens`）。
    pub fn stop_reason(&self) -> Option<&str> {
        self.stop_reason.as_deref()
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

    fn read_usage(&mut self, usage: &Value) {
        let number = |key: &str| usage.get(key).and_then(Value::as_u64);
        // 輸入 token 包含 prompt cache 讀寫的部分。
        let input = [
            "input_tokens",
            "cache_creation_input_tokens",
            "cache_read_input_tokens",
        ]
        .iter()
        .filter_map(|key| number(key))
        .reduce(|a, b| a + b);
        if let Some(input) = input.filter(|&n| n > 0) {
            self.usage.input_tokens = input;
        }
        // `output_tokens` 是累計值。
        if let Some(output) = number("output_tokens") {
            self.usage.output_tokens = output;
        }
    }

    fn event(&mut self, kind: &str, value: &Value, out: &mut Vec<Result<StreamEvent, LlmError>>) {
        let index = value
            .get("index")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        match kind {
            "message_start" => {
                if let Some(usage) = value.pointer("/message/usage") {
                    self.read_usage(usage);
                }
            }
            "content_block_start" => {
                let block = value.get("content_block").unwrap_or(&Value::Null);
                let text = |key: &str| {
                    block
                        .get(key)
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned()
                };
                let block = match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let initial = text("text");
                        if !initial.is_empty() {
                            out.push(Ok(StreamEvent::TextDelta(initial.clone())));
                        }
                        Block::Text(initial)
                    }
                    Some("tool_use") => Block::ToolUse {
                        id: text("id"),
                        name: text("name"),
                        initial: block
                            .get("input")
                            .filter(|input| input.as_object().is_some_and(|o| !o.is_empty()))
                            .cloned(),
                        json: String::new(),
                    },
                    _ => Block::Ignored,
                };
                self.blocks.insert(index, block);
            }
            "content_block_delta" => {
                let delta = value.get("delta").unwrap_or(&Value::Null);
                let text = |key: &str| delta.get(key).and_then(Value::as_str).unwrap_or_default();
                match (
                    delta.get("type").and_then(Value::as_str),
                    self.blocks.get_mut(&index),
                ) {
                    (Some("text_delta"), Some(Block::Text(buffer))) => {
                        let delta = text("text");
                        if !delta.is_empty() {
                            buffer.push_str(delta);
                            out.push(Ok(StreamEvent::TextDelta(delta.to_owned())));
                        }
                    }
                    (Some("input_json_delta"), Some(Block::ToolUse { json, .. })) => {
                        json.push_str(text("partial_json"));
                    }
                    _ => {}
                }
            }
            "content_block_stop" => match self.blocks.remove(&index) {
                Some(Block::Text(text)) if !text.is_empty() => {
                    out.push(Ok(StreamEvent::ItemDone(Item::Assistant { text })));
                }
                Some(Block::ToolUse {
                    id,
                    name,
                    initial,
                    json,
                }) => {
                    let arguments = if !json.trim().is_empty() {
                        json
                    } else if let Some(input) = initial {
                        input.to_string()
                    } else {
                        "{}".to_owned()
                    };
                    self.incomplete_call |= !arguments_complete(&arguments);
                    out.push(Ok(StreamEvent::ItemDone(Item::ToolCall(ToolCall {
                        call_id: id,
                        name,
                        arguments,
                    }))));
                }
                _ => {}
            },
            "message_delta" => {
                if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(reason.to_owned());
                }
                if let Some(usage) = value.get("usage") {
                    self.read_usage(usage);
                }
            }
            "message_stop" => {
                self.terminated = true;
                if self.stop_reason.as_deref() == Some("max_tokens") {
                    if self.incomplete_call {
                        out.push(Err(LlmError::Interrupted(TRUNCATED_OUTPUT.into())));
                        return;
                    }
                    tracing::warn!(
                        "Anthropic 輸出達到 max tokens 被截斷（stop_reason: max_tokens）"
                    );
                }
                out.push(Ok(StreamEvent::Completed { usage: self.usage }));
            }
            "error" => {
                self.terminated = true;
                out.push(Err(stream_error(value)));
            }
            // ping 與之後新增的事件類型略過。
            _ => {}
        }
    }
}

impl EventMapper for MessagesEventMapper {
    fn handle(&mut self, message: &SseMessage, out: &mut Vec<Result<StreamEvent, LlmError>>) {
        if self.terminated || message.data.trim().is_empty() {
            return;
        }
        let value = match parse_event(&message.data) {
            Ok(value) => value,
            Err(error) => {
                self.terminated = true;
                out.push(Err(error));
                return;
            }
        };
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .or(message.event.as_deref())
            .unwrap_or_default()
            .to_owned();
        self.event(&kind, &value, out);
    }

    fn finish(&mut self, out: &mut Vec<Result<StreamEvent, LlmError>>) {
        if !self.terminated {
            self.terminated = true;
            out.push(Err(LlmError::Interrupted(
                "串流在 message_stop 之前結束".into(),
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::responses::SseDecoder;

    fn sse(events: &[Value]) -> String {
        events
            .iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().unwrap()
                )
            })
            .collect()
    }

    fn map_all(stream: &str) -> Vec<Result<StreamEvent, LlmError>> {
        let mut decoder = SseDecoder::new();
        let mut mapper = MessagesEventMapper::new();
        let mut events: Vec<_> = decoder
            .push(stream.as_bytes())
            .iter()
            .flat_map(|message| mapper.handle(message))
            .collect();
        events.extend(mapper.finish());
        events
    }

    #[test]
    fn text_tool_use_and_usage() {
        let stream = sse(&[
            json!({"type": "message_start", "message": {"id": "msg_1", "usage": {"input_tokens": 10, "cache_read_input_tokens": 2, "output_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "hmm"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "ping"}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "我查"}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "一下"}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "get_time", "input": {}}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"tz\":"}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "\"UTC\"}"}}),
            json!({"type": "content_block_stop", "index": 2}),
            json!({"type": "content_block_start", "index": 3, "content_block": {"type": "tool_use", "id": "toolu_2", "name": "noop", "input": {}}}),
            json!({"type": "content_block_stop", "index": 3}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 7}}),
            json!({"type": "message_stop"}),
        ]);
        assert_eq!(
            map_all(&stream),
            vec![
                Ok(StreamEvent::TextDelta("我查".into())),
                Ok(StreamEvent::TextDelta("一下".into())),
                Ok(StreamEvent::ItemDone(Item::Assistant {
                    text: "我查一下".into()
                })),
                Ok(StreamEvent::ItemDone(Item::ToolCall(ToolCall {
                    call_id: "toolu_1".into(),
                    name: "get_time".into(),
                    arguments: "{\"tz\":\"UTC\"}".into()
                }))),
                Ok(StreamEvent::ItemDone(Item::ToolCall(ToolCall {
                    call_id: "toolu_2".into(),
                    name: "noop".into(),
                    arguments: "{}".into()
                }))),
                Ok(StreamEvent::Completed {
                    usage: Usage {
                        input_tokens: 12,
                        output_tokens: 7
                    }
                }),
            ]
        );
    }

    fn tool_use(index: u64, input: Value, partial: &[&str]) -> Vec<Value> {
        let mut events = vec![
            json!({"type": "content_block_start", "index": index, "content_block": {"type": "tool_use", "id": format!("toolu_{index}"), "name": "get_time", "input": input}}),
        ];
        events.extend(partial.iter().map(|json| {
            json!({"type": "content_block_delta", "index": index, "delta": {"type": "input_json_delta", "partial_json": json}})
        }));
        events.push(json!({"type": "content_block_stop", "index": index}));
        events
    }

    fn end(stop_reason: &str) -> Vec<Value> {
        vec![
            json!({"type": "message_delta", "delta": {"stop_reason": stop_reason}, "usage": {"output_tokens": 5}}),
            json!({"type": "message_stop"}),
        ]
    }

    #[test]
    fn truncated_tool_call_fails_turn() {
        let truncated = sse(&[tool_use(0, json!({}), &["{\"tz\":"]), end("max_tokens")].concat());
        let events = map_all(&truncated);
        assert_eq!(
            events.last(),
            Some(&Err(LlmError::Interrupted(
                "輸出被截斷（達到 max tokens）".into()
            )))
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Ok(StreamEvent::Completed { .. })))
        );

        // 參數完整的工具呼叫、只有文字：照常完成。
        let complete_call = sse(&[tool_use(0, json!({}), &["{}"]), end("max_tokens")].concat());
        let text_only = sse(
            &[
                vec![
                    json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": "一半"}}),
                    json!({"type": "content_block_stop", "index": 0}),
                ],
                end("max_tokens"),
            ]
            .concat(),
        );
        for stream in [complete_call, text_only] {
            let events = map_all(&stream);
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
    fn input_from_content_block_start_without_deltas() {
        let stream = sse(&[
            tool_use(0, json!({"tz": "UTC"}), &[]),
            tool_use(1, json!({"ignored": true}), &["{\"tz\":\"JST\"}"]),
            end("tool_use"),
        ]
        .concat());
        let arguments: Vec<String> = map_all(&stream)
            .into_iter()
            .filter_map(|event| match event {
                Ok(StreamEvent::ItemDone(Item::ToolCall(call))) => Some(call.arguments),
                _ => None,
            })
            .collect();
        assert_eq!(arguments, vec!["{\"tz\":\"UTC\"}", "{\"tz\":\"JST\"}"]);
    }

    #[test]
    fn eof_before_message_stop_is_interrupted() {
        let stream = sse(&[
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "一半"}}),
        ]);
        let events = map_all(&stream);
        assert_eq!(events.len(), 2);
        assert!(matches!(events[1], Err(LlmError::Interrupted(_))));
    }

    #[test]
    fn error_event_is_mapped() {
        let stream = sse(&[
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 1}}}),
            json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
            json!({"type": "message_stop"}),
        ]);
        assert!(matches!(
            &map_all(&stream)[..],
            [Err(LlmError::Unavailable(m))] if m.contains("Overloaded")
        ));
    }
}
