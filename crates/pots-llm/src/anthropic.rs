//! Anthropic Messages API (`POST /v1/messages`, streaming).

use serde_json::{Value, json};

use super::{Event, HttpRequest, Part, Provider, Request, Role, Stop, Turn, sse};

const DEFAULT_MAX_TOKENS: u32 = 16_000;

pub(super) fn request(provider: &Provider, req: &Request) -> HttpRequest {
    let mut headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("anthropic-version".to_string(), "2023-06-01".to_string()),
    ];
    if let Some(key) = &provider.key {
        headers.push(("x-api-key".to_string(), key.clone()));
    }
    HttpRequest {
        url: format!("{}/v1/messages", provider.base_url),
        headers,
        body: body(req, provider.prompt_cache),
    }
}

fn body(req: &Request, cache: bool) -> Value {
    let messages: Vec<Value> = req
        .turns
        .iter()
        .filter(|t| !t.parts.is_empty())
        .map(|t| {
            json!({
                "role": match t.role { Role::User => "user", Role::Assistant => "assistant" },
                "content": t.parts.iter().map(block).collect::<Vec<_>>(),
            })
        })
        .collect();
    let mut body = json!({
        "model": req.model,
        "max_tokens": req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        "stream": true,
        "messages": messages,
    });
    if !req.system.is_empty() {
        body["system"] = json!(req.system);
    }
    if cache {
        // Automatic caching: each call of an agent loop re-sends everything before it.
        body["cache_control"] = json!({"type": "ephemeral"});
    }
    if !req.tools.is_empty() {
        body["tools"] = req
            .tools
            .iter()
            .map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.input_schema}))
            .collect();
    }
    body
}

fn block(part: &Part) -> Value {
    match part {
        Part::Text(text) => json!({"type": "text", "text": text}),
        Part::ToolCall { id, name, input } => json!({"type": "tool_use", "id": id, "name": name, "input": input}),
        Part::ToolResult { id, content, is_error } => {
            json!({"type": "tool_result", "tool_use_id": id, "content": content, "is_error": is_error})
        }
        Part::Raw(v) => v.clone(),
    }
}

/// Rebuilds each content block from `content_block_start` + deltas, so
/// blocks we don't interpret (thinking, with its signature) round-trip intact.
#[derive(Default)]
pub(super) struct Decoder {
    blocks: Vec<Value>,
    /// Partial JSON of each `tool_use` block's input.
    input_json: Vec<String>,
    stop: Option<String>,
    done: bool,
    usage: super::Usage,
}

impl Decoder {
    fn read_usage(&mut self, u: &Value) {
        let n = |k: &str| u[k].as_u64();
        if let Some(v) = n("input_tokens") {
            self.usage.input = v;
        }
        if let Some(v) = n("cache_creation_input_tokens") {
            self.usage.cache_write = v;
        }
        if let Some(v) = n("cache_read_input_tokens") {
            self.usage.cache_read = v;
        }
        // Cumulative: message_delta's count supersedes message_start's.
        if let Some(v) = n("output_tokens") {
            self.usage.output = v;
        }
    }

    fn slot(&mut self, index: usize) -> &mut Value {
        if self.blocks.len() <= index {
            self.blocks.resize(index + 1, Value::Null);
            self.input_json.resize(index + 1, String::new());
        }
        let block = &mut self.blocks[index];
        if block.is_null() {
            // A delta without a start: assume text, as that's all it can be.
            *block = json!({"type": "text", "text": ""});
        }
        block
    }
}

fn append(block: &mut Value, field: &str, text: &str) {
    if let Some(obj) = block.as_object_mut() {
        let mut s = obj.get(field).and_then(Value::as_str).unwrap_or_default().to_string();
        s.push_str(text);
        obj.insert(field.to_string(), Value::String(s));
    }
}

impl super::Decoder for Decoder {
    fn feed(&mut self, event: &sse::Event, out: &mut Vec<Event>) -> Result<bool, String> {
        let data: Value = match serde_json::from_str(&event.data) {
            Ok(v) => v,
            Err(_) => return Ok(false), // not JSON: nothing we understand
        };
        let kind = data["type"].as_str().unwrap_or(&event.event).to_string();
        let index = data["index"].as_u64().unwrap_or(0) as usize;
        match kind.as_str() {
            "content_block_start" => {
                let mut block = data["content_block"].clone();
                if block["type"] == "tool_use" {
                    out.push(Event::ToolCall(block["name"].as_str().unwrap_or_default().to_string()));
                    block["input"] = json!({});
                }
                *self.slot(index) = block;
            }
            "content_block_delta" => {
                let delta = &data["delta"];
                match delta["type"].as_str().unwrap_or_default() {
                    "text_delta" => {
                        let text = delta["text"].as_str().unwrap_or_default();
                        append(self.slot(index), "text", text);
                        if !text.is_empty() {
                            out.push(Event::Text(text.to_string()));
                        }
                    }
                    "thinking_delta" => append(self.slot(index), "thinking", delta["thinking"].as_str().unwrap_or_default()),
                    "signature_delta" => {
                        self.slot(index)["signature"] = delta["signature"].clone();
                    }
                    "input_json_delta" => {
                        self.slot(index);
                        self.input_json[index].push_str(delta["partial_json"].as_str().unwrap_or_default());
                    }
                    _ => {} // citations etc.
                }
            }
            "content_block_stop" => {
                let json = self.input_json.get_mut(index).map(std::mem::take).unwrap_or_default();
                let block = self.slot(index);
                if block["type"] == "tool_use" && !json.trim().is_empty() {
                    block["input"] = serde_json::from_str(&json).unwrap_or(Value::String(json));
                }
            }
            "message_start" => self.read_usage(&data["message"]["usage"].clone()),
            "message_delta" => {
                if let Some(stop) = data["delta"]["stop_reason"].as_str() {
                    self.stop = Some(stop.to_string());
                }
                self.read_usage(&data["usage"].clone());
            }
            "message_stop" => {
                self.done = true;
                return Ok(true);
            }
            "error" => {
                let msg = data["error"]["message"].as_str().unwrap_or("unknown error");
                return Err(format!("{} ({})", msg, data["error"]["type"].as_str().unwrap_or("error")));
            }
            _ => {} // message_start, ping
        }
        Ok(false)
    }

    fn usage(&self) -> super::Usage {
        self.usage
    }

    fn finish(self: Box<Self>) -> Result<(Turn, Stop), String> {
        if !self.done {
            return Err("the stream ended before the reply finished".into());
        }
        let parts = self
            .blocks
            .into_iter()
            .filter(|b| !b.is_null())
            .map(|b| match b["type"].as_str() {
                Some("text") => Part::Text(b["text"].as_str().unwrap_or_default().to_string()),
                Some("tool_use") => Part::ToolCall {
                    id: b["id"].as_str().unwrap_or_default().to_string(),
                    name: b["name"].as_str().unwrap_or_default().to_string(),
                    input: b["input"].clone(),
                },
                _ => Part::Raw(b),
            })
            .collect();
        let stop = match self.stop.as_deref() {
            Some("end_turn" | "stop_sequence") | None => Stop::EndTurn,
            Some("tool_use") => Stop::ToolUse,
            Some("max_tokens") => Stop::MaxTokens,
            Some("refusal") => Stop::Refusal,
            Some(other) => Stop::Other(other.to_string()),
        };
        Ok((Turn { role: Role::Assistant, parts }, stop))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Decoder as _, ToolSpec};
    use super::*;

    fn decode(events: &[(&str, Value)]) -> (Vec<Event>, Result<(Turn, Stop), String>) {
        let mut d = Box::new(Decoder::default());
        let mut out = Vec::new();
        for (event, data) in events {
            let e = sse::Event { event: event.to_string(), data: data.to_string() };
            if d.feed(&e, &mut out).unwrap() {
                break;
            }
        }
        (out, d.finish())
    }

    #[test]
    fn decodes_thinking_text_and_tool_use() {
        let (events, done) = decode(&[
            ("message_start", json!({"type": "message_start", "message": {"id": "m"}})),
            ("content_block_start", json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}})),
            ("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig"}})),
            ("content_block_stop", json!({"type": "content_block_stop", "index": 0})),
            ("content_block_start", json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}})),
            ("content_block_delta", json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Let me look."}})),
            ("content_block_stop", json!({"type": "content_block_stop", "index": 1})),
            ("content_block_start", json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "tu_1", "name": "read_channel", "input": {}}})),
            ("content_block_delta", json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"chan"}})),
            ("content_block_delta", json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "nel\": \"general\"}"}})),
            ("content_block_stop", json!({"type": "content_block_stop", "index": 2})),
            ("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}})),
            ("message_stop", json!({"type": "message_stop"})),
        ]);
        assert!(matches!(events.as_slice(), [Event::Text(t), Event::ToolCall(n)] if t == "Let me look." && n == "read_channel"), "{events:?}");
        let (turn, stop) = done.unwrap();
        assert_eq!(stop, Stop::ToolUse);
        assert_eq!(
            turn.parts,
            vec![
                Part::Raw(json!({"type": "thinking", "thinking": "", "signature": "sig"})),
                Part::Text("Let me look.".into()),
                Part::ToolCall { id: "tu_1".into(), name: "read_channel".into(), input: json!({"channel": "general"}) },
            ]
        );
    }

    #[test]
    fn reads_usage_from_start_and_delta() {
        let mut d = Box::new(Decoder::default());
        let mut out = Vec::new();
        for data in [
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 12, "cache_creation_input_tokens": 3000, "cache_read_input_tokens": 9000, "output_tokens": 1}}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 345}}),
        ] {
            d.feed(&sse::Event { event: String::new(), data: data.to_string() }, &mut out).unwrap();
        }
        assert_eq!(d.usage(), super::super::Usage { input: 12, output: 345, cache_write: 3000, cache_read: 9000 });
    }

    #[test]
    fn stream_errors_and_early_ends_fail() {
        let mut d = Box::new(Decoder::default());
        let e = sse::Event { event: "error".into(), data: json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}).to_string() };
        assert_eq!(d.feed(&e, &mut Vec::new()).unwrap_err(), "Overloaded (overloaded_error)");
        assert!(decode(&[("message_start", json!({"type": "message_start"}))]).1.is_err());
    }

    #[test]
    fn builds_body_with_system_tools_and_replayed_blocks() {
        let thinking = json!({"type": "thinking", "thinking": "", "signature": "sig"});
        let req = Request {
            model: "claude-opus-5-5".into(),
            system: "sys".into(),
            turns: vec![
                Turn::user("hi"),
                Turn {
                    role: Role::Assistant,
                    parts: vec![
                        Part::Raw(thinking.clone()),
                        Part::ToolCall { id: "t1".into(), name: "list_channels".into(), input: json!({}) },
                    ],
                },
                Turn { role: Role::User, parts: vec![Part::ToolResult { id: "t1".into(), content: "#general".into(), is_error: false }] },
            ],
            tools: vec![ToolSpec { name: "list_channels", description: "d", input_schema: json!({"type": "object"}) }],
            max_tokens: None,
            extra: None,
        };
        let b = body(&req, true);
        assert_eq!(b["cache_control"], json!({"type": "ephemeral"}));
        assert!(body(&req, false).get("cache_control").is_none());
        assert_eq!(b["system"], "sys");
        assert_eq!(b["max_tokens"], DEFAULT_MAX_TOKENS);
        assert_eq!(b["tools"][0]["name"], "list_channels");
        assert_eq!(b["messages"][1]["content"][0], thinking);
        assert_eq!(b["messages"][1]["content"][1]["type"], "tool_use");
        assert_eq!(b["messages"][2]["content"][0], json!({"type": "tool_result", "tool_use_id": "t1", "content": "#general", "is_error": false}));
    }
}
