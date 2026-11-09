//! 把 [`ModelRequest`] 轉成 Chat Completions 的請求 JSON。純函式，方便測試。

use serde_json::{Map, Value, json};

use crate::provider::{Item, ModelRequest, ToolCall};

/// [`super::OpenAiCompatProvider`] 的名稱。
pub const PROVIDER_NAME: &str = "openai-compatible";

/// 組出 `POST {base}/chat/completions` 的 body。
///
/// - `instructions` 變成開頭的 `system` message。
/// - 同一輪的助理文字與緊接著的工具呼叫合成一則 `assistant` message（`content` + `tool_calls`）。
/// - [`Item::ToolResult`] 變成 `tool` message。
/// - [`Item::Opaque`]（其他 provider 的專屬項目）略過。
/// - 一律 `stream: true` 並要求最後一塊帶 usage；沒有工具時不送 `tools`。
/// - 不主動送 `reasoning_effort`：不是每個模型都接受，只有被 Chat Completions 以
///   「工具 + reasoning」拒絕時才用 [`build_request_body_with_effort`] 重送。
pub fn build_request_body(request: &ModelRequest) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));
    body.insert("messages".into(), Value::Array(messages(request)));
    body.insert("stream".into(), json!(true));
    body.insert("stream_options".into(), json!({"include_usage": true}));
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    },
                })
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
    }
    Value::Object(body)
}

/// 同 [`build_request_body`]，但明確送 `reasoning_effort`。
///
/// GPT-5.4 以後的模型預設會推理，而 Chat Completions 不接受「工具 + reasoning」；被拒絕時
/// 用 `"none"` 重送一次（見 [`super::OpenAiCompatProvider`]），用工具但關掉推理。
pub fn build_request_body_with_effort(request: &ModelRequest, effort: &str) -> Value {
    let mut body = build_request_body(request);
    if let Some(map) = body.as_object_mut() {
        map.insert("reasoning_effort".into(), json!(effort));
    }
    body
}

/// 還沒送出的 assistant message：文字與同一輪的工具呼叫。
#[derive(Default)]
struct AssistantTurn {
    content: Option<String>,
    tool_calls: Vec<Value>,
}

impl AssistantTurn {
    fn into_message(self) -> Value {
        let mut message = Map::new();
        message.insert("role".into(), json!("assistant"));
        // 只有工具呼叫時 content 是 null（Chat Completions 的慣例）。
        message.insert("content".into(), json!(self.content));
        if !self.tool_calls.is_empty() {
            message.insert("tool_calls".into(), Value::Array(self.tool_calls));
        }
        Value::Object(message)
    }
}

fn messages(request: &ModelRequest) -> Vec<Value> {
    let mut messages = Vec::new();
    if let Some(instructions) = request.instructions.as_deref().filter(|s| !s.is_empty()) {
        messages.push(json!({"role": "system", "content": instructions}));
    }
    let mut turn: Option<AssistantTurn> = None;
    let flush = |turn: &mut Option<AssistantTurn>, messages: &mut Vec<Value>| {
        if let Some(turn) = turn.take() {
            messages.push(turn.into_message());
        }
    };
    for item in &request.input {
        match item {
            Item::User { text } => {
                flush(&mut turn, &mut messages);
                messages.push(json!({"role": "user", "content": text}));
            }
            Item::Assistant { text } => {
                flush(&mut turn, &mut messages);
                turn = Some(AssistantTurn {
                    content: Some(text.clone()),
                    tool_calls: Vec::new(),
                });
            }
            Item::ToolCall(call) => {
                turn.get_or_insert_with(AssistantTurn::default)
                    .tool_calls
                    .push(tool_call(call));
            }
            Item::ToolResult { call_id, output } => {
                flush(&mut turn, &mut messages);
                messages.push(json!({"role": "tool", "tool_call_id": call_id, "content": output}));
            }
            Item::Opaque { .. } => {}
        }
    }
    flush(&mut turn, &mut messages);
    messages
}

fn tool_call(call: &ToolCall) -> Value {
    let arguments = if call.arguments.trim().is_empty() {
        "{}"
    } else {
        call.arguments.as_str()
    };
    json!({
        "id": call.call_id,
        "type": "function",
        "function": {"name": call.name, "arguments": arguments},
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ToolSpec;

    fn call(id: &str, name: &str, arguments: &str) -> Item {
        Item::ToolCall(ToolCall {
            call_id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
        })
    }

    #[test]
    fn maps_history_to_chat_messages() {
        let request = ModelRequest {
            model: "gpt-test".into(),
            instructions: Some("你是 Pots".into()),
            input: vec![
                Item::User {
                    text: "幾點、天氣".into(),
                },
                Item::Opaque {
                    provider: "openai-responses".into(),
                    value: json!({"type": "reasoning"}),
                },
                Item::Assistant {
                    text: "我查一下".into(),
                },
                call("c1", "get_time", "{\"tz\":\"UTC\"}"),
                call("c2", "weather", ""),
                Item::ToolResult {
                    call_id: "c1".into(),
                    output: "12:00".into(),
                },
                Item::ToolResult {
                    call_id: "c2".into(),
                    output: "晴".into(),
                },
                call("c3", "get_time", "{}"),
                Item::ToolResult {
                    call_id: "c3".into(),
                    output: "12:01".into(),
                },
                Item::Assistant {
                    text: "12 點，晴".into(),
                },
                Item::User {
                    text: "謝謝".into(),
                },
            ],
            tools: vec![ToolSpec {
                name: "get_time".into(),
                description: "現在時間".into(),
                parameters: json!({"type": "object", "properties": {}}),
            }],
            session_id: None,
        };
        assert_eq!(
            build_request_body(&request),
            json!({
                "model": "gpt-test",
                "messages": [
                    {"role": "system", "content": "你是 Pots"},
                    {"role": "user", "content": "幾點、天氣"},
                    {"role": "assistant", "content": "我查一下", "tool_calls": [
                        {"id": "c1", "type": "function", "function": {"name": "get_time", "arguments": "{\"tz\":\"UTC\"}"}},
                        {"id": "c2", "type": "function", "function": {"name": "weather", "arguments": "{}"}},
                    ]},
                    {"role": "tool", "tool_call_id": "c1", "content": "12:00"},
                    {"role": "tool", "tool_call_id": "c2", "content": "晴"},
                    {"role": "assistant", "content": null, "tool_calls": [
                        {"id": "c3", "type": "function", "function": {"name": "get_time", "arguments": "{}"}},
                    ]},
                    {"role": "tool", "tool_call_id": "c3", "content": "12:01"},
                    {"role": "assistant", "content": "12 點，晴"},
                    {"role": "user", "content": "謝謝"},
                ],
                "stream": true,
                "stream_options": {"include_usage": true},
                "tools": [{"type": "function", "function": {
                    "name": "get_time",
                    "description": "現在時間",
                    "parameters": {"type": "object", "properties": {}},
                }}],
            })
        );
    }

    #[test]
    fn minimal_body_omits_tools_and_system() {
        let request = ModelRequest {
            model: "m".into(),
            instructions: Some(String::new()),
            input: vec![Item::User { text: "hi".into() }],
            tools: vec![],
            session_id: None,
        };
        let body = build_request_body(&request);
        assert!(body.get("tools").is_none());
        assert_eq!(body["messages"], json!([{"role": "user", "content": "hi"}]));
    }
}
