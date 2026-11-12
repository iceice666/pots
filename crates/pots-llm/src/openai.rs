//! OpenAI Chat Completions API (`POST /chat/completions`, streaming).

use serde_json::{Value, json};

use super::{Event, HttpRequest, Part, Provider, Request, Role, Stop, Turn, sse};

pub(super) fn request(provider: &Provider, req: &Request) -> HttpRequest {
    let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
    if let Some(key) = &provider.key {
        headers.push(("authorization".to_string(), format!("Bearer {key}")));
    }
    HttpRequest {
        url: format!("{}/chat/completions", provider.base_url),
        headers,
        body: body(req, provider.stream_usage),
    }
}

fn body(req: &Request, stream_usage: bool) -> Value {
    let mut messages = Vec::new();
    if !req.system.is_empty() {
        messages.push(json!({"role": "system", "content": req.system}));
    }
    for turn in &req.turns {
        let text = turn
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        match turn.role {
            Role::User => {
                // Tool results are messages of their own, right after the call.
                for part in &turn.parts {
                    if let Part::ToolResult { id, content, is_error } = part {
                        let content = if *is_error { format!("Error: {content}") } else { content.clone() };
                        messages.push(json!({"role": "tool", "tool_call_id": id, "content": content}));
                    }
                }
                if !text.is_empty() {
                    messages.push(json!({"role": "user", "content": text}));
                }
            }
            Role::Assistant => {
                let calls: Vec<Value> = turn
                    .tool_calls()
                    .map(|(id, name, input)| {
                        // Arguments that weren't valid JSON are kept as the model sent them.
                        let args = match input {
                            Value::String(raw) => raw.clone(),
                            v => v.to_string(),
                        };
                        json!({"id": id, "type": "function", "function": {"name": name, "arguments": args}})
                    })
                    .collect();
                if text.is_empty() && calls.is_empty() {
                    continue;
                }
                let mut msg = json!({"role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text) }});
                if !calls.is_empty() {
                    msg["tool_calls"] = json!(calls);
                }
                messages.push(msg);
            }
        }
    }
    let mut body = json!({"model": req.model, "stream": true, "messages": messages});
    if stream_usage {
        body["stream_options"] = json!({"include_usage": true});
    }
    if let Some(max) = req.max_tokens {
        body["max_tokens"] = json!(max);
    }
    if !req.tools.is_empty() {
        body["tools"] = req
            .tools
            .iter()
            .map(|t| {
                json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.input_schema}})
            })
            .collect();
    }
    body
}

#[derive(Default)]
struct Call {
    id: String,
    name: String,
    args: String,
}

#[derive(Default)]
pub(super) struct Decoder {
    text: String,
    calls: Vec<Call>,
    finish: Option<String>,
    done: bool,
    usage: super::Usage,
}

impl super::Decoder for Decoder {
    fn feed(&mut self, event: &sse::Event, out: &mut Vec<Event>) -> Result<bool, String> {
        if event.data.trim() == "[DONE]" {
            self.done = true;
            return Ok(true);
        }
        let data: Value = match serde_json::from_str(&event.data) {
            Ok(v) => v,
            Err(_) => return Ok(false),
        };
        if let Some(err) = data.get("error") {
            let msg = err["message"].as_str().or(err.as_str()).unwrap_or("unknown error");
            return Err(msg.to_string());
        }
        if let Some(u) = data.get("usage").filter(|u| u.is_object()) {
            let cached = u["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
            let prompt = u["prompt_tokens"].as_u64().unwrap_or(0);
            self.usage = super::Usage {
                input: prompt.saturating_sub(cached),
                output: u["completion_tokens"].as_u64().unwrap_or(0),
                cache_write: 0,
                cache_read: cached,
            };
        }
        let Some(choice) = data["choices"].get(0) else {
            return Ok(false); // e.g. the trailing usage chunk
        };
        let delta = &choice["delta"];
        if let Some(text) = delta["content"].as_str().filter(|t| !t.is_empty()) {
            self.text.push_str(text);
            out.push(Event::Text(text.to_string()));
        }
        for (pos, call) in delta["tool_calls"].as_array().into_iter().flatten().enumerate() {
            let index = call["index"].as_u64().map_or(pos, |i| i as usize);
            if self.calls.len() <= index {
                self.calls.resize_with(index + 1, Call::default);
            }
            let slot = &mut self.calls[index];
            if let Some(id) = call["id"].as_str().filter(|s| !s.is_empty()) {
                slot.id = id.to_string();
            }
            if let Some(name) = call["function"]["name"].as_str().filter(|s| !s.is_empty()) {
                if slot.name.is_empty() {
                    out.push(Event::ToolCall(name.to_string()));
                }
                slot.name.push_str(name);
            }
            if let Some(args) = call["function"]["arguments"].as_str() {
                slot.args.push_str(args);
            }
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            self.finish = Some(reason.to_string());
        }
        Ok(false)
    }

    fn usage(&self) -> super::Usage {
        self.usage
    }

    fn finish(self: Box<Self>) -> Result<(Turn, Stop), String> {
        // Some servers end the stream without `[DONE]`; a finish reason is enough.
        if !self.done && self.finish.is_none() {
            return Err("the stream ended before the reply finished".into());
        }
        let mut parts = Vec::new();
        if !self.text.is_empty() {
            parts.push(Part::Text(self.text));
        }
        let has_calls = !self.calls.is_empty();
        for (i, call) in self.calls.into_iter().enumerate() {
            let input = if call.args.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&call.args).unwrap_or(Value::String(call.args))
            };
            let id = if call.id.is_empty() { format!("call_{i}") } else { call.id };
            parts.push(Part::ToolCall { id, name: call.name, input });
        }
        let stop = match self.finish.as_deref() {
            _ if has_calls => Stop::ToolUse,
            Some("stop") | None => Stop::EndTurn,
            Some("length") => Stop::MaxTokens,
            Some("content_filter") => Stop::Refusal,
            Some(other) => Stop::Other(other.to_string()),
        };
        Ok((Turn { role: Role::Assistant, parts }, stop))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Decoder as _, ToolSpec};
    use super::*;

    fn decode(chunks: &[&str]) -> (Vec<Event>, Result<(Turn, Stop), String>) {
        decode_with_usage(chunks).0
    }

    fn decode_with_usage(chunks: &[&str]) -> ((Vec<Event>, Result<(Turn, Stop), String>), super::super::Usage) {
        let mut d = Box::new(Decoder::default());
        let mut out = Vec::new();
        for c in chunks {
            if d.feed(&sse::Event { event: String::new(), data: c.to_string() }, &mut out).unwrap() {
                break;
            }
        }
        let usage = d.usage();
        ((out, d.finish()), usage)
    }

    #[test]
    fn decodes_text_and_split_tool_calls() {
        let (events, done) = decode(&[
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"Checking"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"read_channel","arguments":""}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"channel\":"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"design\"}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":9,"prompt_tokens_details":{"cached_tokens":60}}}"#,
            "[DONE]",
        ]);
        assert!(matches!(events.as_slice(), [Event::Text(t), Event::ToolCall(n)] if t == "Checking" && n == "read_channel"), "{events:?}");
        let (turn, stop) = done.unwrap();
        assert_eq!(stop, Stop::ToolUse);
        assert_eq!(
            turn.parts,
            vec![
                Part::Text("Checking".into()),
                Part::ToolCall { id: "call_a".into(), name: "read_channel".into(), input: json!({"channel": "design"}) },
            ]
        );
    }

    #[test]
    fn reads_the_trailing_usage_chunk() {
        let (_, usage) = decode_with_usage(&[
            r#"{"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":9,"prompt_tokens_details":{"cached_tokens":60}}}"#,
            "[DONE]",
        ]);
        assert_eq!(usage, super::super::Usage { input: 40, output: 9, cache_write: 0, cache_read: 60 });
    }

    #[test]
    fn finish_reason_without_done_is_enough() {
        let (_, done) = decode(&[r#"{"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}"#]);
        assert_eq!(done.unwrap(), (Turn::assistant("ok"), Stop::EndTurn));
    }

    #[test]
    fn builds_messages_with_tool_round_trip() {
        let req = Request {
            model: "gpt".into(),
            system: "sys".into(),
            turns: vec![
                Turn::user("hi"),
                Turn {
                    role: Role::Assistant,
                    parts: vec![
                        Part::Raw(json!({"type": "thinking"})),
                        Part::ToolCall { id: "c1".into(), name: "list_channels".into(), input: json!({}) },
                    ],
                },
                Turn { role: Role::User, parts: vec![Part::ToolResult { id: "c1".into(), content: "nope".into(), is_error: true }] },
            ],
            tools: vec![ToolSpec { name: "list_channels", description: "d", input_schema: json!({"type": "object"}) }],
            max_tokens: Some(100),
            extra: None,
        };
        let b = body(&req, true);
        assert_eq!(b["stream_options"], json!({"include_usage": true}));
        let m = &b["messages"];
        assert_eq!(m[0], json!({"role": "system", "content": "sys"}));
        assert_eq!(m[1], json!({"role": "user", "content": "hi"}));
        assert_eq!(m[2]["content"], Value::Null);
        assert_eq!(m[2]["tool_calls"][0]["function"], json!({"name": "list_channels", "arguments": "{}"}));
        assert_eq!(m[3], json!({"role": "tool", "tool_call_id": "c1", "content": "Error: nope"}));
        assert_eq!(m.as_array().unwrap().len(), 4);
        assert_eq!(b["max_tokens"], 100);
        assert_eq!(b["tools"][0]["function"]["name"], "list_channels");
    }
}
