//! OpenAI Responses API (`POST /responses`, streaming).
//!
//! Requests are stateless: the whole conversation goes in `input` each time.
//! Reasoning items a reply produced are kept as [`Part::Raw`] and sent back
//! within the same exchange, so reasoning models keep their train of thought
//! across tool calls.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::{Event, HttpRequest, Part, Provider, Request, Role, Stop, Turn, Usage, sse};

pub(super) fn request(provider: &Provider, req: &Request) -> HttpRequest {
    let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
    if let Some(key) = &provider.key {
        headers.push(("authorization".to_string(), format!("Bearer {key}")));
    }
    HttpRequest { url: format!("{}/responses", provider.base_url), headers, body: body(req) }
}

fn body(req: &Request) -> Value {
    let mut input = Vec::new();
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
                for part in &turn.parts {
                    if let Part::ToolResult { id, content, is_error } = part {
                        let output = if *is_error { format!("Error: {content}") } else { content.clone() };
                        input.push(json!({"type": "function_call_output", "call_id": id, "output": output}));
                    }
                }
                if !text.is_empty() {
                    input.push(json!({"role": "user", "content": text}));
                }
            }
            Role::Assistant => {
                // Items in the order the model produced them: reasoning first,
                // then what it said, then its calls.
                for part in &turn.parts {
                    if let Part::Raw(item) = part
                        && item["type"] == "reasoning"
                    {
                        input.push(item.clone());
                    }
                }
                if !text.is_empty() {
                    input.push(json!({
                        "type": "message", "role": "assistant",
                        "content": [{"type": "output_text", "text": text, "annotations": []}],
                    }));
                }
                for (id, name, args) in turn.tool_calls() {
                    // Arguments that weren't valid JSON are kept as the model sent them.
                    let args = match args {
                        Value::String(raw) => raw.clone(),
                        v => v.to_string(),
                    };
                    input.push(json!({"type": "function_call", "call_id": id, "name": name, "arguments": args}));
                }
            }
        }
    }
    let mut body = json!({"model": req.model, "stream": true, "input": input});
    if !req.system.is_empty() {
        body["instructions"] = json!(req.system);
    }
    if let Some(max) = req.max_tokens {
        body["max_output_tokens"] = json!(max);
    }
    if !req.tools.is_empty() {
        body["tools"] = req
            .tools
            .iter()
            .map(|t| json!({"type": "function", "name": t.name, "description": t.description, "parameters": t.input_schema}))
            .collect();
    }
    body
}

/// Builds the reply from finished output items (`response.output_item.done`,
/// or the final response's `output` for servers that only send that); deltas
/// only drive the live [`Event`]s.
#[derive(Default)]
pub(super) struct Decoder {
    /// Finished output items by `output_index`.
    items: BTreeMap<u64, Value>,
    /// Text streamed so far, used if no finished message item ever arrives.
    text: String,
    /// Output indexes whose tool call was already announced.
    announced: Vec<u64>,
    /// `status` and `incomplete_details.reason` of the final response.
    status: Option<(String, Option<String>)>,
    usage: Usage,
}

impl Decoder {
    fn announce(&mut self, index: u64, item: &Value, out: &mut Vec<Event>) {
        if item["type"] == "function_call"
            && !self.announced.contains(&index)
            && let Some(name) = item["name"].as_str().filter(|n| !n.is_empty())
        {
            self.announced.push(index);
            out.push(Event::ToolCall(name.to_string()));
        }
    }

    fn finished(&mut self, response: &Value) {
        let status = response["status"].as_str().unwrap_or("completed").to_string();
        let reason = response["incomplete_details"]["reason"].as_str().map(str::to_string);
        self.status = Some((status, reason));
        for (i, item) in response["output"].as_array().into_iter().flatten().enumerate() {
            let index = i as u64;
            self.items.entry(index).or_insert_with(|| item.clone());
        }
        if let Some(u) = response.get("usage").filter(|u| u.is_object()) {
            let cached = u["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
            self.usage = Usage {
                input: u["input_tokens"].as_u64().unwrap_or(0).saturating_sub(cached),
                output: u["output_tokens"].as_u64().unwrap_or(0),
                cache_write: 0,
                cache_read: cached,
            };
        }
    }
}

fn error_text(err: &Value) -> String {
    err["message"].as_str().or(err.as_str()).unwrap_or("unknown error").to_string()
}

impl super::Decoder for Decoder {
    fn feed(&mut self, event: &sse::Event, out: &mut Vec<Event>) -> Result<bool, String> {
        if event.data.trim() == "[DONE]" {
            return Ok(true);
        }
        let Ok(data) = serde_json::from_str::<Value>(&event.data) else { return Ok(false) };
        let kind = data["type"].as_str().unwrap_or(event.event.as_str());
        let index = data["output_index"].as_u64().unwrap_or(0);
        match kind {
            "response.output_text.delta" | "response.refusal.delta" | "response.content_part.delta" => {
                if let Some(text) = data["delta"].as_str().filter(|t| !t.is_empty()) {
                    self.text.push_str(text);
                    out.push(Event::Text(text.to_string()));
                }
            }
            "response.output_item.added" => self.announce(index, &data["item"], out),
            "response.output_item.done" => {
                self.announce(index, &data["item"], out);
                self.items.insert(index, data["item"].clone());
            }
            "response.completed" | "response.incomplete" | "response.done" => {
                self.finished(&data["response"]);
                return Ok(true);
            }
            "response.failed" => return Err(error_text(&data["response"]["error"])),
            "error" => return Err(error_text(data.get("error").unwrap_or(&data))),
            _ => {}
        }
        Ok(false)
    }

    fn usage(&self) -> Usage {
        self.usage
    }

    fn finish(self: Box<Self>) -> Result<(Turn, Stop), String> {
        let Some((status, reason)) = self.status else {
            return Err("the stream ended before the reply finished".into());
        };
        let mut texts = Vec::new();
        let mut reasoning = Vec::new();
        let mut calls = Vec::new();
        let mut refused = false;
        for (i, item) in self.items.into_values().enumerate() {
            match item["type"].as_str() {
                Some("message") => {
                    for c in item["content"].as_array().into_iter().flatten() {
                        match c["type"].as_str() {
                            Some("output_text") => texts.extend(c["text"].as_str().map(str::to_string)),
                            Some("refusal") => {
                                refused = true;
                                texts.extend(c["refusal"].as_str().map(str::to_string));
                            }
                            _ => {}
                        }
                    }
                }
                Some("reasoning") => reasoning.push(Part::Raw(item)),
                Some("function_call") => {
                    let args = item["arguments"].as_str().unwrap_or_default();
                    let input = if args.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(args).unwrap_or_else(|_| Value::String(args.to_string()))
                    };
                    let id = item["call_id"].as_str().or(item["id"].as_str()).map_or_else(|| format!("call_{i}"), str::to_string);
                    let name = item["name"].as_str().unwrap_or_default().to_string();
                    calls.push(Part::ToolCall { id, name, input });
                }
                _ => {} // built-in tools and the like: not ours to replay
            }
        }
        let text = if texts.is_empty() { self.text } else { texts.concat() };
        let mut parts = reasoning;
        if !text.is_empty() {
            parts.push(Part::Text(text));
        }
        let has_calls = !calls.is_empty();
        parts.extend(calls);
        let stop = match (status.as_str(), reason.as_deref()) {
            _ if has_calls => Stop::ToolUse,
            ("incomplete", Some("max_output_tokens")) => Stop::MaxTokens,
            ("incomplete", Some("content_filter")) => Stop::Refusal,
            _ if refused => Stop::Refusal,
            ("completed", _) => Stop::EndTurn,
            (other, reason) => Stop::Other(reason.unwrap_or(other).to_string()),
        };
        Ok((Turn { role: Role::Assistant, parts }, stop))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Decoder as _, ToolSpec};
    use super::*;

    fn decode(events: &[Value]) -> (Vec<Event>, Result<(Turn, Stop), String>, Usage) {
        let mut d = Box::new(Decoder::default());
        let mut out = Vec::new();
        for e in events {
            let ev = sse::Event { event: e["type"].as_str().unwrap_or_default().to_string(), data: e.to_string() };
            match d.feed(&ev, &mut out) {
                Ok(true) => break,
                Ok(false) => {}
                Err(e) => return (out, Err(e), Usage::default()),
            }
        }
        let usage = d.usage();
        (out, d.finish(), usage)
    }

    #[test]
    fn decodes_reasoning_text_and_tool_calls() {
        let reasoning = json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "enc"});
        let call = json!({"type": "function_call", "id": "fc_1", "call_id": "call_a", "name": "read_channel", "arguments": ""});
        let call_done = json!({"type": "function_call", "id": "fc_1", "call_id": "call_a", "name": "read_channel", "arguments": "{\"channel\":\"design\"}"});
        let (events, done, usage) = decode(&[
            json!({"type": "response.created", "response": {"status": "in_progress"}}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning}),
            json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "message", "role": "assistant", "content": []}}),
            json!({"type": "response.output_text.delta", "output_index": 1, "delta": "Check"}),
            json!({"type": "response.output_text.delta", "output_index": 1, "delta": "ing"}),
            json!({"type": "response.output_item.done", "output_index": 1, "item": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Checking"}]}}),
            json!({"type": "response.output_item.added", "output_index": 2, "item": call}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 2, "delta": "{\"channel\":"}),
            json!({"type": "response.output_item.done", "output_index": 2, "item": call_done}),
            json!({"type": "response.completed", "response": {"status": "completed", "output": [], "usage": {"input_tokens": 100, "output_tokens": 9, "input_tokens_details": {"cached_tokens": 60}}}}),
        ]);
        assert!(
            matches!(events.as_slice(), [Event::Text(a), Event::Text(b), Event::ToolCall(n)] if a == "Check" && b == "ing" && n == "read_channel"),
            "{events:?}"
        );
        let (turn, stop) = done.unwrap();
        assert_eq!(stop, Stop::ToolUse);
        assert_eq!(
            turn.parts,
            vec![
                Part::Raw(reasoning),
                Part::Text("Checking".into()),
                Part::ToolCall { id: "call_a".into(), name: "read_channel".into(), input: json!({"channel": "design"}) },
            ]
        );
        assert_eq!(usage, Usage { input: 40, output: 9, cache_write: 0, cache_read: 60 });
    }

    #[test]
    fn takes_output_from_the_final_response_and_tolerates_other_event_names() {
        // Some compatible servers send `response.content_part.delta` and
        // `response.done`, and no `output_item.done`.
        let (events, done, _) = decode(&[
            json!({"type": "response.content_part.delta", "output_index": 0, "delta": "Hi"}),
            json!({"type": "response.done", "response": {"status": "completed", "output": [
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Hi"}]},
            ]}}),
        ]);
        assert!(matches!(events.as_slice(), [Event::Text(t)] if t == "Hi"));
        assert_eq!(done.unwrap(), (Turn::assistant("Hi"), Stop::EndTurn));
    }

    #[test]
    fn maps_incomplete_and_failed_responses() {
        let (_, done, _) = decode(&[
            json!({"type": "response.output_text.delta", "delta": "cut"}),
            json!({"type": "response.incomplete", "response": {"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}}}),
        ]);
        assert_eq!(done.unwrap(), (Turn::assistant("cut"), Stop::MaxTokens));

        let (_, done, _) = decode(&[json!({"type": "response.failed", "response": {"status": "failed", "error": {"code": "server_error", "message": "boom"}}})]);
        assert_eq!(done.unwrap_err(), "boom");

        let (_, done, _) = decode(&[json!({"type": "response.output_text.delta", "delta": "partial"})]);
        assert!(done.unwrap_err().contains("ended"));
    }

    #[test]
    fn builds_input_with_tool_round_trip() {
        let reasoning = json!({"type": "reasoning", "id": "rs_1", "summary": []});
        let req = Request {
            model: "gpt".into(),
            system: "sys".into(),
            turns: vec![
                Turn::user("hi"),
                Turn {
                    role: Role::Assistant,
                    parts: vec![
                        Part::Raw(reasoning.clone()),
                        Part::Raw(json!({"type": "thinking"})),
                        Part::Text("Looking".into()),
                        Part::ToolCall { id: "c1".into(), name: "list_channels".into(), input: json!({}) },
                    ],
                },
                Turn { role: Role::User, parts: vec![Part::ToolResult { id: "c1".into(), content: "nope".into(), is_error: true }] },
            ],
            tools: vec![ToolSpec { name: "list_channels", description: "d", input_schema: json!({"type": "object"}) }],
            max_tokens: Some(100),
            extra: None,
        };
        let b = body(&req);
        assert_eq!(b["instructions"], "sys");
        assert_eq!(b["max_output_tokens"], 100);
        assert_eq!(b["tools"][0], json!({"type": "function", "name": "list_channels", "description": "d", "parameters": {"type": "object"}}));
        let input = b["input"].as_array().unwrap();
        assert_eq!(input[0], json!({"role": "user", "content": "hi"}));
        assert_eq!(input[1], reasoning, "reasoning goes back; other providers' raw blocks don't");
        assert_eq!(input[2]["content"][0]["text"], "Looking");
        assert_eq!(input[3], json!({"type": "function_call", "call_id": "c1", "name": "list_channels", "arguments": "{}"}));
        assert_eq!(input[4], json!({"type": "function_call_output", "call_id": "c1", "output": "Error: nope"}));
        assert_eq!(input.len(), 5);
    }
}
