//! 把 [`ModelRequest`] 轉成 Chat Completions 的請求 JSON。純函式，方便測試。

use serde_json::{Map, Value, json};

use crate::image::{ImagePayloads, data_url};
use crate::provider::{Item, LlmError, ModelRequest, ToolCall};

/// [`super::OpenAiCompatProvider`] 的名稱。
pub const PROVIDER_NAME: &str = "openai-compatible";

/// 組出 `POST {base}/chat/completions` 的 body。
///
/// - `instructions` 變成開頭的 `system` message。
/// - 同一輪的助理文字與緊接著的工具呼叫合成一則 `assistant` message（`content` + `tool_calls`）。
/// - [`Item::ToolResult`] 變成 `tool` message。
/// - [`Item::Image`] 併進同一則 user message 的 `content`，變成文字片段加 `image_url`（data URL）。
///   `content` 因此從字串變成陣列；沒有圖片時仍是一模一樣的字串。
/// - [`Item::Opaque`]（其他 provider 的專屬項目）略過。
/// - 一律 `stream: true` 並要求最後一塊帶 usage；沒有工具時不送 `tools`。
/// - [`ModelRequest::reasoning`] 有設定時送 `reasoning_effort`（[`Reasoning::openai_effort`]，
///   `Off` 是 `"none"`）；沒設定時不送，用模型預設值（不是每個模型都接受這個參數）。
///   被 Chat Completions 以「工具 + reasoning」拒絕時，provider 會用
///   [`build_request_body_with_effort`] 改成 `"none"` 重送。
///
/// `images` 是這次請求的附件內容（attachment id → base64），由呼叫端在送出前讀檔產生；
/// 歷史裡有圖片但 `images` 少了它的內容時回 [`LlmError::Unsupported`]，不會默默送少一張圖的請求。
///
/// [`Reasoning::openai_effort`]: crate::provider::Reasoning::openai_effort
pub fn build_request_body(
    request: &ModelRequest,
    images: &ImagePayloads,
) -> Result<Value, LlmError> {
    match request.reasoning {
        Some(reasoning) => build_request_body_with_effort(
            request,
            reasoning.openai_effort(),
            images,
        ),
        None => base_body(request, images),
    }
}

/// 同 [`build_request_body`]，但不管 [`ModelRequest::reasoning`]，一律送這個 `reasoning_effort`。
///
/// GPT-5.4 以後的模型預設會推理，而 Chat Completions 不接受「工具 + reasoning」；被拒絕時
/// 用 `"none"` 重送一次（見 [`super::OpenAiCompatProvider`]），用工具但關掉推理。
pub fn build_request_body_with_effort(
    request: &ModelRequest,
    effort: &str,
    images: &ImagePayloads,
) -> Result<Value, LlmError> {
    let mut body = base_body(request, images)?;
    if let Some(map) = body.as_object_mut() {
        map.insert("reasoning_effort".into(), json!(effort));
    }
    Ok(body)
}

/// 不含 `reasoning_effort` 的 body。
fn base_body(request: &ModelRequest, images: &ImagePayloads) -> Result<Value, LlmError> {
    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));
    body.insert("messages".into(), Value::Array(messages(request, images)?));
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
    Ok(Value::Object(body))
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

fn messages(request: &ModelRequest, images: &ImagePayloads) -> Result<Vec<Value>, LlmError> {
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
    // 還沒送出的 user message。只有圖片出現時 `content` 才變成陣列；沒有圖片時送出的 JSON
    // 與以前完全一樣（字串）。
    let mut user: Option<UserTurn> = None;
    let flush_user = |user: &mut Option<UserTurn>, messages: &mut Vec<Value>| {
        if let Some(user) = user.take() {
            messages.push(user.into_message());
        }
    };
    for item in &request.input {
        match item {
            Item::User { text } => {
                flush(&mut turn, &mut messages);
                flush_user(&mut user, &mut messages);
                user = Some(UserTurn {
                    text: Some(text.clone()),
                    images: Vec::new(),
                });
            }
            Item::Image {
                attachment_id,
                mime,
            } => {
                flush(&mut turn, &mut messages);
                user.get_or_insert_with(UserTurn::default)
                    .images
                    .push(json!({
                        "type": "image_url",
                        "image_url": {
                            "url": data_url(mime, crate::image::payload(images, attachment_id)?),
                        },
                    }));
            }
            Item::Assistant { text } => {
                flush(&mut turn, &mut messages);
                flush_user(&mut user, &mut messages);
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
                flush_user(&mut user, &mut messages);
                messages.push(json!({"role": "tool", "tool_call_id": call_id, "content": output}));
            }
            Item::Opaque { .. } => {}
        }
    }
    flush(&mut turn, &mut messages);
    flush_user(&mut user, &mut messages);
    Ok(messages)
}

/// 還沒送出的 user message。
#[derive(Default)]
struct UserTurn {
    text: Option<String>,
    /// `image_url` 片段；空的就送字串 `content`（與加圖片之前完全一樣）。
    images: Vec<Value>,
}

impl UserTurn {
    fn into_message(self) -> Value {
        let Some(text) = self.text else {
            // 只有圖片（歷史裡沒有對應的文字）：純圖片 content 陣列。
            return json!({"role": "user", "content": self.images});
        };
        if self.images.is_empty() {
            return json!({"role": "user", "content": text});
        }
        let mut content = Vec::with_capacity(self.images.len() + 1);
        if !text.trim().is_empty() {
            content.push(json!({"type": "text", "text": text}));
        }
        content.extend(self.images);
        json!({"role": "user", "content": content})
    }
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
    use crate::provider::{Reasoning, ToolSpec};

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
            reasoning: None,
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
            build_request_body(&request, &ImagePayloads::new()).unwrap(),
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
            reasoning: None,
            model: "m".into(),
            instructions: Some(String::new()),
            input: vec![Item::User { text: "hi".into() }],
            tools: vec![],
            session_id: None,
        };
        let body = build_request_body(&request, &ImagePayloads::new()).unwrap();
        assert!(body.get("tools").is_none());
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["messages"], json!([{"role": "user", "content": "hi"}]));
    }

    fn image_request() -> ModelRequest {
        ModelRequest {
            model: "gpt-test".into(),
            input: vec![
                Item::User {
                    text: "這張圖是什麼？".into(),
                },
                Item::Image {
                    attachment_id: "att-1".into(),
                    mime: "image/png".into(),
                },
                Item::Assistant {
                    text: "一隻貓".into(),
                },
                Item::User {
                    text: "謝謝".into(),
                },
            ],
            ..ModelRequest::default()
        }
    }

    fn images() -> ImagePayloads {
        ImagePayloads::from([("att-1".to_string(), "QUJD".to_string())])
    }

    #[test]
    fn image_becomes_image_url_in_the_same_user_message() {
        let body = build_request_body(&image_request(), &images()).unwrap();
        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": [
                    {"type": "text", "text": "這張圖是什麼？"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}},
                ]},
                {"role": "assistant", "content": "一隻貓"},
                {"role": "user", "content": "謝謝"},
            ])
        );
        // 沒有圖片的訊息仍然是字串 content（與加圖片之前完全一樣）。
        assert_eq!(body["messages"][2]["content"], json!("謝謝"));
    }

    #[test]
    fn image_without_payload_is_an_error() {
        let error = build_request_body(&image_request(), &ImagePayloads::new()).unwrap_err();
        assert!(
            matches!(&error, LlmError::Unsupported(m) if m.contains("att-1")),
            "{error:?}"
        );
    }

    #[test]
    fn reasoning_maps_to_effort() {
        let mut request = ModelRequest {
            model: "m".into(),
            input: vec![Item::User { text: "hi".into() }],
            ..ModelRequest::default()
        };
        for (reasoning, effort) in [
            (Reasoning::Off, "none"),
            (Reasoning::Low, "low"),
            (Reasoning::Medium, "medium"),
            (Reasoning::High, "high"),
        ] {
            request.reasoning = Some(reasoning);
            assert_eq!(
                build_request_body(&request, &ImagePayloads::new()).unwrap()["reasoning_effort"],
                effort
            );
            // 明確指定的 effort（降級重送）優先。
            assert_eq!(
                build_request_body_with_effort(&request, "none", &ImagePayloads::new()).unwrap()
                    ["reasoning_effort"],
                "none"
            );
        }
    }
}
