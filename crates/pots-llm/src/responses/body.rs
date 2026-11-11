//! 把 [`ModelRequest`] 轉成 Responses API 的請求 JSON。純函式，方便測試。

use serde_json::{Map, Value, json};

use crate::provider::{Item, ModelRequest, ToolSpec};

/// [`Item::Opaque`] 的 provider 名稱，也是 [`super::ResponsesProvider`] 的名稱。
pub const PROVIDER_NAME: &str = "openai-responses";

/// Responses 請求的可調整項目。預設值就是 ChatGPT Plan（SIWC）要用的設定。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResponsesOptions {
    /// 所有 Pots 工具放進這個 namespace。SIWC 要求 function tool 放在 namespace
    /// （或 `additional_tools`）裡，不能直接放在頂層 `tools`。
    ///
    /// `None` 是給一般 OpenAI 平台 API 用：工具平整地放在頂層 `tools`，回送的
    /// `function_call` 也不帶 `namespace`。
    pub namespace: Option<String>,
    /// namespace 的說明，給模型看。
    pub namespace_description: String,
    /// 在 `include` 要求 `reasoning.encrypted_content`，並在下一輪回送 reasoning item。
    ///
    /// `store: false` 時伺服器不保存 reasoning，一般 Responses API 要靠加密內容做無狀態續接；
    /// SIWC 文件沒有明列 `include` 是否被接受（未驗證），所以做成選項，預設開啟。
    /// 新版 openai-python 的型別說明提到 `POST /v1/responses` 預設就會帶 `encrypted_content`，
    /// 且要用 `response.output_item.done` 的完整 item 回送；若 SIWC 拒絕 `include`，關掉這個
    /// 選項應該不影響正確性。關閉時只是不送 `include`；回送與否只看 item 有沒有加密內容。
    /// [`super::ResponsesProvider`] 被拒絕 `include` 時會自動拿掉它重送一次，之後不再送。
    ///
    /// 回送時拿掉 reasoning item 的 `id`：`store: false` 時伺服器沒有保存該項目，帶 `id` 可能被
    /// 當成引用而失敗（Codex 也不序列化這個欄位）。
    pub include_encrypted_reasoning: bool,
}

impl Default for ResponsesOptions {
    fn default() -> Self {
        Self {
            namespace: Some("pots".into()),
            namespace_description: "Pots 平台工具".into(),
            include_encrypted_reasoning: true,
        }
    }
}

/// 組出 `POST /responses` 的 body。
///
/// 只送必要的欄位（也是 SIWC 允許的）：`model`、`instructions`（有才送）、`input`、`tools`（有才送）、
/// `reasoning`（有設定才送）、`include`（見 [`ResponsesOptions::include_encrypted_reasoning`]）、
/// `store: false`、`stream: true`。不送 `previous_response_id`、`temperature`、
/// `max_output_tokens` 等，也不產生 system message item。
///
/// [`ModelRequest::reasoning`] 有設定時送 `reasoning: {"effort": ...}`（`Off` 是 `"none"`）；
/// 沒設定時不送 `reasoning`，讓伺服器用模型預設值（推理模型預設會推理，Responses API 允許
/// 工具與推理並用）。不接受某個 effort 的模型會回錯誤，不自動重送。
pub fn build_request_body(request: &ModelRequest, options: &ResponsesOptions) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));
    if let Some(instructions) = request.instructions.as_deref().filter(|s| !s.is_empty()) {
        body.insert("instructions".into(), json!(instructions));
    }
    let input: Vec<Value> = request
        .input
        .iter()
        .filter_map(|item| input_item(item, options))
        .collect();
    body.insert("input".into(), Value::Array(input));
    if !request.tools.is_empty() {
        body.insert("tools".into(), tools_field(&request.tools, options));
    }
    if let Some(reasoning) = request.reasoning {
        body.insert(
            "reasoning".into(),
            json!({"effort": reasoning.openai_effort()}),
        );
    }
    if options.include_encrypted_reasoning {
        body.insert("include".into(), json!(["reasoning.encrypted_content"]));
    }
    body.insert("store".into(), json!(false));
    body.insert("stream".into(), json!(true));
    Value::Object(body)
}

/// 工具放進 namespace（SIWC）或平整地放在頂層（一般 OpenAI 平台）。
fn tools_field(tools: &[ToolSpec], options: &ResponsesOptions) -> Value {
    match &options.namespace {
        Some(namespace) => json!([namespace_tool(
            tools,
            namespace,
            &options.namespace_description
        )]),
        None => Value::Array(tools.iter().map(function_tool).collect()),
    }
}

/// 一個 function tool；平台工具的 schema 不一定符合 strict mode 的限制，明確關掉。
fn function_tool(tool: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.parameters,
        "strict": false,
    })
}

fn namespace_tool(tools: &[ToolSpec], namespace: &str, description: &str) -> Value {
    json!({
        "type": "namespace",
        "name": namespace,
        "description": description,
        "tools": tools.iter().map(function_tool).collect::<Vec<_>>(),
    })
}

/// 一個歷史項目對應的 input item；不屬於這個 provider 或不能回送的項目回 `None`。
fn input_item(item: &Item, options: &ResponsesOptions) -> Option<Value> {
    let value = match item {
        Item::User { text } => json!({
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": text }],
        }),
        Item::Assistant { text } => json!({
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": text }],
        }),
        Item::ToolCall(call) => {
            let mut value = json!({
                "type": "function_call",
                "name": call.name,
                "call_id": call.call_id,
                "arguments": call.arguments,
            });
            if let Some(namespace) = &options.namespace {
                value["namespace"] = json!(namespace);
            }
            value
        }
        Item::ToolResult { call_id, output } => json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": output,
        }),
        Item::Opaque { provider, value } if provider == PROVIDER_NAME => {
            return replay_opaque(value);
        }
        Item::Opaque { .. } => return None,
    };
    Some(value)
}

/// 回送 provider 專屬項目。reasoning item 要有加密內容才能在 `store: false` 下續接（沒有就略過）；
/// 拿掉 `id`，避免伺服器去找沒有保存的項目。其他項目原樣回送。
fn replay_opaque(value: &Value) -> Option<Value> {
    if value.get("type").and_then(Value::as_str) != Some("reasoning") {
        return Some(value.clone());
    }
    let has_encrypted = value
        .get("encrypted_content")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    if !has_encrypted {
        return None;
    }
    let mut value = value.clone();
    if let Some(object) = value.as_object_mut() {
        object.remove("id");
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Reasoning, ToolCall};

    fn request() -> ModelRequest {
        ModelRequest {
            reasoning: None,
            model: "gpt-test".into(),
            instructions: Some("你是 Pots".into()),
            input: vec![
                Item::User {
                    text: "幾點".into(),
                },
                Item::Opaque {
                    provider: PROVIDER_NAME.into(),
                    value: json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "enc"}),
                },
                Item::Opaque {
                    provider: PROVIDER_NAME.into(),
                    value: json!({"type": "reasoning", "id": "rs_2", "summary": []}),
                },
                Item::Opaque {
                    provider: "other".into(),
                    value: json!({"type": "x"}),
                },
                Item::ToolCall(ToolCall {
                    call_id: "c1".into(),
                    name: "get_time".into(),
                    arguments: "{}".into(),
                }),
                Item::ToolResult {
                    call_id: "c1".into(),
                    output: "12:00".into(),
                },
                Item::Assistant {
                    text: "12 點".into(),
                },
            ],
            tools: vec![ToolSpec {
                name: "get_time".into(),
                description: "現在時間".into(),
                parameters: json!({"type": "object", "properties": {}}),
            }],
            session_id: None,
        }
    }

    #[test]
    fn body_matches_siwc_contract() {
        let body = build_request_body(&request(), &ResponsesOptions::default());
        assert_eq!(
            body,
            json!({
                "model": "gpt-test",
                "instructions": "你是 Pots",
                "input": [
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "幾點"}]},
                    {"type": "reasoning", "summary": [], "encrypted_content": "enc"},
                    {"type": "function_call", "namespace": "pots", "name": "get_time", "call_id": "c1", "arguments": "{}"},
                    {"type": "function_call_output", "call_id": "c1", "output": "12:00"},
                    {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "12 點"}]},
                ],
                "tools": [{
                    "type": "namespace",
                    "name": "pots",
                    "description": "Pots 平台工具",
                    "tools": [{
                        "type": "function",
                        "name": "get_time",
                        "description": "現在時間",
                        "parameters": {"type": "object", "properties": {}},
                        "strict": false,
                    }],
                }],
                "include": ["reasoning.encrypted_content"],
                "store": false,
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
        let options = ResponsesOptions {
            include_encrypted_reasoning: false,
            ..ResponsesOptions::default()
        };
        let body = build_request_body(&request, &options);
        let keys: Vec<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys.len(), 4);
        for key in ["model", "input", "store", "stream"] {
            assert!(keys.contains(&key), "{key}");
        }
    }

    #[test]
    fn reasoning_replay_depends_on_encrypted_content_only() {
        let options = ResponsesOptions {
            include_encrypted_reasoning: false,
            ..ResponsesOptions::default()
        };
        let body = build_request_body(&request(), &options);
        assert!(body.get("include").is_none());
        let reasoning: Vec<&Value> = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["type"] == "reasoning")
            .collect();
        assert_eq!(
            reasoning,
            vec![&json!({"type": "reasoning", "summary": [], "encrypted_content": "enc"})]
        );
    }
    #[test]
    fn reasoning_sets_effort() {
        let mut request = request();
        for (reasoning, effort) in [
            (Reasoning::Off, "none"),
            (Reasoning::Low, "low"),
            (Reasoning::Medium, "medium"),
            (Reasoning::High, "high"),
        ] {
            request.reasoning = Some(reasoning);
            let body = build_request_body(&request, &ResponsesOptions::default());
            assert_eq!(body["reasoning"], json!({"effort": effort}));
        }
        request.reasoning = None;
        let body = build_request_body(&request, &ResponsesOptions::default());
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn api_key_tools_are_flat_without_namespace() {
        let options = ResponsesOptions {
            namespace: None,
            include_encrypted_reasoning: false,
            ..ResponsesOptions::default()
        };
        let body = build_request_body(&request(), &options);
        assert_eq!(
            body["tools"],
            json!([{
                "type": "function",
                "name": "get_time",
                "description": "現在時間",
                "parameters": {"type": "object", "properties": {}},
                "strict": false,
            }])
        );
        let call = &body["input"][2];
        assert_eq!(call["type"], "function_call");
        assert_eq!(call["name"], "get_time");
        assert!(call.get("namespace").is_none(), "{call}");
    }
}
