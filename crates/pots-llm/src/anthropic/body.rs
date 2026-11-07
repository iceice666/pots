//! 把 [`ModelRequest`] 轉成 Anthropic Messages API 的請求 JSON。純函式，方便測試。

use serde_json::{Map, Value, json};

use crate::provider::{Item, ModelRequest, ToolCall};

/// [`super::AnthropicProvider`] 的名稱。
pub const PROVIDER_NAME: &str = "anthropic-compatible";

/// 預設的 `max_tokens`。
pub const DEFAULT_MAX_TOKENS: u32 = 8192;

/// 組出 `POST /v1/messages` 的 body。
///
/// - `instructions` 放頂層 `system`（有才送）。
/// - 每個 [`Item`] 變成一個 content block：使用者文字 → user 的 `text`；助理文字 → assistant 的
///   `text`；工具呼叫 → assistant 的 `tool_use`（`input` 是解析過的 JSON 物件，解析失敗時 `{}`）；
///   工具結果 → user 的 `tool_result`。
/// - Anthropic 要求 user／assistant 交替，所以相鄰同角色的 block 合併成一則 message
///   （例如工具結果與接著的新使用者訊息）。
/// - [`Item::Opaque`]（其他 provider 的專屬項目）與空白文字略過；沒有工具時不送 `tools`。
pub fn build_request_body(request: &ModelRequest, max_tokens: u32) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));
    body.insert("max_tokens".into(), json!(max_tokens));
    if let Some(instructions) = request.instructions.as_deref().filter(|s| !s.is_empty()) {
        body.insert("system".into(), json!(instructions));
    }
    body.insert("messages".into(), Value::Array(messages(&request.input)));
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.parameters,
                })
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
    }
    body.insert("stream".into(), json!(true));
    Value::Object(body)
}

fn messages(items: &[Item]) -> Vec<Value> {
    let mut messages: Vec<(&'static str, Vec<Value>)> = Vec::new();
    for item in items {
        let (role, block) = match item {
            Item::User { text } if !text.trim().is_empty() => {
                ("user", json!({"type": "text", "text": text}))
            }
            Item::Assistant { text } if !text.trim().is_empty() => {
                ("assistant", json!({"type": "text", "text": text}))
            }
            Item::ToolCall(call) => ("assistant", tool_use(call)),
            Item::ToolResult { call_id, output } => (
                "user",
                json!({"type": "tool_result", "tool_use_id": call_id, "content": output}),
            ),
            Item::User { .. } | Item::Assistant { .. } | Item::Opaque { .. } => continue,
        };
        match messages.last_mut() {
            Some((last, blocks)) if *last == role => blocks.push(block),
            _ => messages.push((role, vec![block])),
        }
    }
    messages
        .into_iter()
        .map(|(role, content)| json!({"role": role, "content": content}))
        .collect()
}

fn tool_use(call: &ToolCall) -> Value {
    let input = serde_json::from_str::<Value>(&call.arguments)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    json!({"type": "tool_use", "id": call.call_id, "name": call.name, "input": input})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ToolSpec;

    fn call(id: &str, arguments: &str) -> Item {
        Item::ToolCall(ToolCall {
            call_id: id.into(),
            name: "get_time".into(),
            arguments: arguments.into(),
        })
    }

    #[test]
    fn merges_same_role_blocks_and_parses_tool_input() {
        let request = ModelRequest {
            model: "claude-test".into(),
            instructions: Some("你是 Pots".into()),
            input: vec![
                Item::User {
                    text: "幾點".into(),
                },
                Item::Opaque {
                    provider: "openai-responses".into(),
                    value: json!({"type": "reasoning"}),
                },
                Item::Assistant {
                    text: "我查一下".into(),
                },
                call("c1", "{\"tz\":\"UTC\"}"),
                call("c2", "not json"),
                Item::ToolResult {
                    call_id: "c1".into(),
                    output: "12:00".into(),
                },
                Item::ToolResult {
                    call_id: "c2".into(),
                    output: "錯誤".into(),
                },
                Item::User {
                    text: "還有台北？".into(),
                },
                call("c3", "[1]"),
            ],
            tools: vec![ToolSpec {
                name: "get_time".into(),
                description: "現在時間".into(),
                parameters: json!({"type": "object", "properties": {}}),
            }],
        };
        assert_eq!(
            build_request_body(&request, 1024),
            json!({
                "model": "claude-test",
                "max_tokens": 1024,
                "system": "你是 Pots",
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "幾點"}]},
                    {"role": "assistant", "content": [
                        {"type": "text", "text": "我查一下"},
                        {"type": "tool_use", "id": "c1", "name": "get_time", "input": {"tz": "UTC"}},
                        {"type": "tool_use", "id": "c2", "name": "get_time", "input": {}},
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "c1", "content": "12:00"},
                        {"type": "tool_result", "tool_use_id": "c2", "content": "錯誤"},
                        {"type": "text", "text": "還有台北？"},
                    ]},
                    {"role": "assistant", "content": [
                        {"type": "tool_use", "id": "c3", "name": "get_time", "input": {}},
                    ]},
                ],
                "tools": [{
                    "name": "get_time",
                    "description": "現在時間",
                    "input_schema": {"type": "object", "properties": {}},
                }],
                "stream": true,
            })
        );
    }

    #[test]
    fn minimal_body_omits_optional_fields() {
        let request = ModelRequest {
            model: "m".into(),
            instructions: None,
            input: vec![Item::User { text: "hi".into() }],
            tools: vec![],
        };
        let body = build_request_body(&request, DEFAULT_MAX_TOKENS);
        let object = body.as_object().unwrap();
        assert!(!object.contains_key("tools"));
        assert!(!object.contains_key("system"));
        assert_eq!(body["max_tokens"], 8192);
    }
}
