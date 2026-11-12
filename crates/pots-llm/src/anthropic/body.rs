//! 把 [`ModelRequest`] 轉成 Anthropic Messages API 的請求 JSON。純函式，方便測試。

use serde_json::{Map, Value, json};

use crate::image::{ImagePayloads, normalize_mime, payload};
use crate::provider::{Item, LlmError, ModelRequest, Reasoning, ToolCall};

/// [`super::AnthropicProvider`] 的名稱，也是 thinking block 的 [`Item::Opaque`] provider。
pub const PROVIDER_NAME: &str = "anthropic-compatible";

/// 預設的 `max_tokens`。
pub const DEFAULT_MAX_TOKENS: u32 = 8192;

/// 請求裡的 `thinking`／`output_config` 設定。
///
/// [`ThinkingConfig::from_reasoning`] 是預設的對應；[`super::AnthropicProvider`] 被拒絕時改用
/// [`ThinkingConfig::Budget`] 或 [`ThinkingConfig::ModelDefault`]（見模組說明）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThinkingConfig {
    /// 不送 `thinking` 與 `output_config`，用模型預設值（有些模型預設就會推理）。
    ModelDefault,
    /// `thinking: {type: "adaptive"}` 加上 `output_config: {effort}`（Claude 4.6 以後）。
    Adaptive { effort: &'static str },
    /// `thinking: {type: "enabled", budget_tokens}`（Claude 4.5 以前只支援這種）。
    /// `max_tokens` 至少提高到 `budget_tokens + DEFAULT_MAX_TOKENS`，留空間給回覆。
    Budget { budget_tokens: u32 },
    /// `thinking: {type: "disabled"}`。
    Disabled,
}

impl ThinkingConfig {
    /// 預設的對應：沒設定 → 不送；`Off` → `disabled`；其他 → adaptive + effort。
    pub fn from_reasoning(reasoning: Option<Reasoning>) -> Self {
        match reasoning {
            None => Self::ModelDefault,
            Some(Reasoning::Off) => Self::Disabled,
            Some(reasoning) => Self::Adaptive {
                effort: reasoning.openai_effort(),
            },
        }
    }

    /// adaptive 被拒時的後備：固定的 thinking budget。`Off` 沒有對應的 budget。
    ///
    /// 數字是自己訂的（文件只規定至少 1024、小於 `max_tokens`）。
    pub fn budget(reasoning: Reasoning) -> Option<Self> {
        let budget_tokens = match reasoning {
            Reasoning::Off => return None,
            Reasoning::Low => 2048,
            Reasoning::Medium => 8192,
            Reasoning::High => 16384,
        };
        Some(Self::Budget { budget_tokens })
    }
}

/// 組出 `POST /v1/messages` 的 body，`thinking` 用 [`ThinkingConfig::from_reasoning`]。
/// 見 [`build_request_body_with_thinking`]。
pub fn build_request_body(
    request: &ModelRequest,
    max_tokens: u32,
    images: &ImagePayloads,
) -> Result<Value, LlmError> {
    build_request_body_with_thinking(
        request,
        max_tokens,
        ThinkingConfig::from_reasoning(request.reasoning),
        images,
    )
}

/// 組出 `POST /v1/messages` 的 body。
///
/// - `instructions` 放頂層 `system`（有才送）。
/// - 每個 [`Item`] 變成一個 content block：使用者文字 → user 的 `text`；助理文字 → assistant 的
///   `text`；工具呼叫 → assistant 的 `tool_use`（`input` 是解析過的 JSON 物件，解析失敗時 `{}`）；
///   工具結果 → user 的 `tool_result`。
/// - Anthropic 要求 user／assistant 交替，所以相鄰同角色的 block 合併成一則 message
///   （例如工具結果與接著的新使用者訊息）。
/// - 這個 provider 的 [`Item::Opaque`]（`thinking`／`redacted_thinking` block）原樣放回 assistant
///   message 的對應位置：搭配工具時 API 要求它們完整、不經修改地回送。
/// - [`Item::Image`] 變成 user 的 `image` block（`source: {type: "base64", ...}`），併進相鄰的
///   user message；`images` 少了附件內容時回 [`LlmError::Unsupported`]。
/// - 其他 provider 的 [`Item::Opaque`] 與空白文字略過；沒有工具時不送 `tools`。
/// - `thinking` 依 [`ThinkingConfig`]。
pub fn build_request_body_with_thinking(
    request: &ModelRequest,
    max_tokens: u32,
    thinking: ThinkingConfig,
    images: &ImagePayloads,
) -> Result<Value, LlmError> {
    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));
    let max_tokens = match thinking {
        ThinkingConfig::Budget { budget_tokens } => {
            max_tokens.max(budget_tokens.saturating_add(DEFAULT_MAX_TOKENS))
        }
        _ => max_tokens,
    };
    body.insert("max_tokens".into(), json!(max_tokens));
    match thinking {
        ThinkingConfig::ModelDefault => {}
        ThinkingConfig::Adaptive { effort } => {
            body.insert("thinking".into(), json!({"type": "adaptive"}));
            body.insert("output_config".into(), json!({"effort": effort}));
        }
        ThinkingConfig::Budget { budget_tokens } => {
            body.insert(
                "thinking".into(),
                json!({"type": "enabled", "budget_tokens": budget_tokens}),
            );
        }
        ThinkingConfig::Disabled => {
            body.insert("thinking".into(), json!({"type": "disabled"}));
        }
    }
    if let Some(instructions) = request.instructions.as_deref().filter(|s| !s.is_empty()) {
        body.insert("system".into(), json!(instructions));
    }
    body.insert(
        "messages".into(),
        Value::Array(messages(&request.input, images)?),
    );
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
    Ok(Value::Object(body))
}

fn messages(items: &[Item], images: &ImagePayloads) -> Result<Vec<Value>, LlmError> {
    let mut messages: Vec<(&'static str, Vec<Value>)> = Vec::new();
    for item in items {
        let (role, block) = match item {
            Item::Image {
                attachment_id,
                mime,
            } => ("user", image(mime, payload(images, attachment_id)?)?),
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
            // thinking／redacted_thinking block：原樣回送，不可修改。
            Item::Opaque { provider, value } if provider == PROVIDER_NAME && value.is_object() => {
                ("assistant", value.clone())
            }
            Item::User { .. } | Item::Assistant { .. } | Item::Opaque { .. } => continue,
        };
        match messages.last_mut() {
            Some((last, blocks)) if *last == role => blocks.push(block),
            _ => messages.push((role, vec![block])),
        }
    }
    Ok(messages
        .into_iter()
        .map(|(role, content)| json!({"role": role, "content": content}))
        .collect())
}

/// `image` content block。Anthropic 只收 `image/jpeg`、`image/png`、`image/gif`、`image/webp`
/// 四種 `media_type`，其他型別由 client 端先擋掉；這裡再確認一次，避免送出一定失敗的請求。
fn image(mime: &str, base64: &str) -> Result<Value, LlmError> {
    let mime = normalize_mime(mime);
    if !matches!(
        mime,
        "image/jpeg" | "image/png" | "image/gif" | "image/webp"
    ) {
        return Err(LlmError::Unsupported(format!(
            "Anthropic 不支援這個圖片格式：{mime}（只收 png、jpeg、gif、webp）"
        )));
    }
    Ok(json!({
        "type": "image",
        "source": {"type": "base64", "media_type": mime, "data": base64},
    }))
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

    fn thinking(text: &str, signature: &str) -> Item {
        Item::Opaque {
            provider: PROVIDER_NAME.into(),
            value: json!({"type": "thinking", "thinking": text, "signature": signature}),
        }
    }

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
            reasoning: None,
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
            session_id: None,
        };
        assert_eq!(
            build_request_body(&request, 1024, &ImagePayloads::new()).unwrap(),
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
            reasoning: None,
            model: "m".into(),
            instructions: None,
            input: vec![Item::User { text: "hi".into() }],
            tools: vec![],
            session_id: None,
        };
        let body = build_request_body(&request, DEFAULT_MAX_TOKENS, &ImagePayloads::new()).unwrap();
        let object = body.as_object().unwrap();
        assert!(!object.contains_key("tools"));
        assert!(!object.contains_key("system"));
        assert!(!object.contains_key("thinking"));
        assert!(!object.contains_key("output_config"));
        assert_eq!(body["max_tokens"], 8192);
    }

    #[test]
    fn reasoning_maps_to_adaptive_or_disabled() {
        let mut request = ModelRequest {
            model: "m".into(),
            input: vec![Item::User { text: "hi".into() }],
            ..ModelRequest::default()
        };
        for (reasoning, effort) in [
            (Reasoning::Low, "low"),
            (Reasoning::Medium, "medium"),
            (Reasoning::High, "high"),
        ] {
            request.reasoning = Some(reasoning);
            let body = build_request_body(&request, 1024, &ImagePayloads::new()).unwrap();
            assert_eq!(body["thinking"], json!({"type": "adaptive"}));
            assert_eq!(body["output_config"], json!({"effort": effort}));
            assert_eq!(body["max_tokens"], 1024);
        }
        request.reasoning = Some(Reasoning::Off);
        let body = build_request_body(&request, 1024, &ImagePayloads::new()).unwrap();
        assert_eq!(body["thinking"], json!({"type": "disabled"}));
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn budget_fallback_raises_max_tokens() {
        let request = ModelRequest {
            model: "m".into(),
            input: vec![Item::User { text: "hi".into() }],
            reasoning: Some(Reasoning::High),
            ..ModelRequest::default()
        };
        assert_eq!(ThinkingConfig::budget(Reasoning::Off), None);
        for (reasoning, budget) in [
            (Reasoning::Low, 2048),
            (Reasoning::Medium, 8192),
            (Reasoning::High, 16384),
        ] {
            let config = ThinkingConfig::budget(reasoning).unwrap();
            let body = build_request_body_with_thinking(
                &request,
                DEFAULT_MAX_TOKENS,
                config,
                &ImagePayloads::new(),
            )
            .unwrap();
            assert_eq!(
                body["thinking"],
                json!({"type": "enabled", "budget_tokens": budget})
            );
            assert!(body.get("output_config").is_none());
            assert_eq!(body["max_tokens"], budget + DEFAULT_MAX_TOKENS);
        }
        // 使用者設定的 max_tokens 已經夠大就不動。
        let config = ThinkingConfig::Budget {
            budget_tokens: 2048,
        };
        let body =
            build_request_body_with_thinking(&request, 64_000, config, &ImagePayloads::new())
                .unwrap();
        assert_eq!(body["max_tokens"], 64_000);
        // 明確不送。
        let body = build_request_body_with_thinking(
            &request,
            1024,
            ThinkingConfig::ModelDefault,
            &ImagePayloads::new(),
        )
        .unwrap();
        assert!(body.get("thinking").is_none());
        assert!(body.get("output_config").is_none());
    }

    fn image_request() -> ModelRequest {
        ModelRequest {
            model: "claude-test".into(),
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
            ],
            ..ModelRequest::default()
        }
    }

    fn images() -> ImagePayloads {
        ImagePayloads::from([("att-1".to_string(), "QUJD".to_string())])
    }

    #[test]
    fn image_becomes_base64_block_in_the_same_user_message() {
        let body = build_request_body(&image_request(), 1024, &images()).unwrap();
        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": [
                    {"type": "text", "text": "這張圖是什麼？"},
                    {
                        "type": "image",
                        "source": {"type": "base64", "media_type": "image/png", "data": "QUJD"},
                    },
                ]},
                {"role": "assistant", "content": [{"type": "text", "text": "一隻貓"}]},
            ])
        );
    }

    #[test]
    fn image_without_payload_is_an_error() {
        let error = build_request_body(&image_request(), 1024, &ImagePayloads::new()).unwrap_err();
        assert!(
            matches!(&error, LlmError::Unsupported(m) if m.contains("att-1")),
            "{error:?}"
        );
    }

    #[test]
    fn image_mime_is_normalized_and_validated() {
        let mut request = image_request();
        request.input[1] = Item::Image {
            attachment_id: "att-1".into(),
            mime: "image/jpg".into(),
        };
        let body = build_request_body(&request, 1024, &images()).unwrap();
        assert_eq!(
            body["messages"][0]["content"][1]["source"]["media_type"],
            "image/jpeg"
        );

        let mut request = image_request();
        request.input[1] = Item::Image {
            attachment_id: "att-1".into(),
            mime: "image/tiff".into(),
        };
        let error = build_request_body(&request, 1024, &images()).unwrap_err();
        assert!(
            matches!(&error, LlmError::Unsupported(m) if m.contains("image/tiff")),
            "{error:?}"
        );
    }

    #[test]
    fn thinking_blocks_are_replayed_in_place() {
        let redacted = json!({"type": "redacted_thinking", "data": "enc"});
        let request = ModelRequest {
            model: "m".into(),
            input: vec![
                Item::User {
                    text: "幾點".into(),
                },
                thinking("先查時間", "sig-1"),
                Item::Opaque {
                    provider: PROVIDER_NAME.into(),
                    value: redacted.clone(),
                },
                call("c1", "{}"),
                Item::ToolResult {
                    call_id: "c1".into(),
                    output: "12:00".into(),
                },
                // 其他 provider 的 Opaque、不是物件的值略過。
                Item::Opaque {
                    provider: "openai-responses".into(),
                    value: json!({"type": "reasoning"}),
                },
                Item::Opaque {
                    provider: PROVIDER_NAME.into(),
                    value: json!("not a block"),
                },
                thinking("", "sig-2"),
                Item::Assistant {
                    text: "12 點".into(),
                },
            ],
            ..ModelRequest::default()
        };
        assert_eq!(
            build_request_body(&request, 1024, &ImagePayloads::new()).unwrap()["messages"],
            json!([
                {"role": "user", "content": [{"type": "text", "text": "幾點"}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "先查時間", "signature": "sig-1"},
                    redacted,
                    {"type": "tool_use", "id": "c1", "name": "get_time", "input": {}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "c1", "content": "12:00"},
                ]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "", "signature": "sig-2"},
                    {"type": "text", "text": "12 點"},
                ]},
            ])
        );
    }
}
