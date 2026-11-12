//! A fake LLM server that speaks the wire formats Pots uses: Anthropic
//! Messages (`POST …/v1/messages`), OpenAI Responses (`POST …/responses`) and
//! OpenAI Chat Completions (`POST …/chat/completions`), streaming over SSE.
//!
//! Each request takes the next step of a [`Script`]. Once the script runs
//! out (or with an empty one) the server improvises, so the app is usable
//! against it with no API key:
//!
//! - a message is echoed back;
//! - a message containing `!tool NAME {json}` makes the "model" call tool
//!   `NAME` with that input, and the tool's result is echoed when it comes
//!   back;
//! - `!error STATUS message` answers with an HTTP error.
//!
//! [`serve_once`] and [`serve`] are lower-level helpers for tests: canned raw
//! responses, one per connection.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

/// What the "model" does in answer to one request.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    /// Reply with this text, streamed in small pieces.
    Text(String),
    /// Call a tool, and stop for its result.
    Tool { name: String, input: Value },
    /// Fail with this HTTP status and error message.
    Error { status: u16, message: String },
}

/// The steps the server answers with, one per request, in order.
#[derive(Clone, Debug, Default)]
pub struct Script {
    steps: VecDeque<Step>,
}

impl Script {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.steps.push_back(Step::Text(text.into()));
        self
    }

    pub fn tool(mut self, name: impl Into<String>, input: Value) -> Self {
        self.steps.push_back(Step::Tool { name: name.into(), input });
        self
    }

    pub fn error(mut self, status: u16, message: impl Into<String>) -> Self {
        self.steps.push_back(Step::Error { status, message: message.into() });
        self
    }
}

/// A request the server received.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub path: String,
    /// Header names are lowercase.
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// How many requests are kept for [`MockServer::requests`].
const KEEP_REQUESTS: usize = 200;

struct State {
    script: Mutex<Script>,
    requests: Mutex<Vec<Recorded>>,
    /// Pause between streamed pieces, so streaming is visible in the UI.
    delay: Duration,
    next_id: AtomicUsize,
    stopping: AtomicBool,
}

/// A running mock server. It stops when dropped.
pub struct MockServer {
    addr: SocketAddr,
    state: Arc<State>,
}

impl MockServer {
    /// Serve `script` on a free local port, with no streaming delay.
    pub fn start(script: Script) -> Self {
        Self::bind("127.0.0.1:0", script, Duration::ZERO).expect("bind a local port")
    }

    /// Serve `script` on `addr`, pausing `delay` between streamed pieces.
    pub fn bind(addr: impl ToSocketAddrs, script: Script, delay: Duration) -> io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        let addr = listener.local_addr()?;
        let state = Arc::new(State {
            script: Mutex::new(script),
            requests: Mutex::new(Vec::new()),
            delay,
            next_id: AtomicUsize::new(1),
            stopping: AtomicBool::new(false),
        });
        let shared = state.clone();
        thread::Builder::new().name("pots-mock".into()).spawn(move || {
            for conn in listener.incoming() {
                if shared.stopping.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(conn) = conn else { continue };
                let state = shared.clone();
                let _ = thread::Builder::new().name("pots-mock-conn".into()).spawn(move || {
                    if let Err(e) = handle(conn, &state) {
                        eprintln!("pots-mock: {e}");
                    }
                });
            }
        })?;
        Ok(Self { addr, state })
    }

    /// `http://127.0.0.1:PORT`: the base URL for `api = "anthropic"`. For
    /// `api = "openai"` append `/v1`.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The requests received so far (the most recent ones), oldest first.
    pub fn requests(&self) -> Vec<Recorded> {
        self.state.requests.lock().unwrap().clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.state.stopping.store(true, Ordering::SeqCst);
        // Wake the accept loop so it sees the flag.
        let _ = TcpStream::connect(self.addr);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Api {
    Anthropic,
    Openai,
    Responses,
}

struct HttpRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn read_request(conn: &mut BufReader<TcpStream>) -> io::Result<Option<HttpRequest>> {
    let mut line = String::new();
    if conn.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or_default().to_string(), parts.next().unwrap_or_default().to_string());
    let mut headers = Vec::new();
    loop {
        line.clear();
        if conn.read_line(&mut line)? == 0 {
            break;
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let len = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0; len];
    conn.read_exact(&mut body)?;
    Ok(Some(HttpRequest { method, path, headers, body }))
}

fn handle(conn: TcpStream, state: &State) -> io::Result<()> {
    let mut reader = BufReader::new(conn.try_clone()?);
    let Some(req) = read_request(&mut reader)? else { return Ok(()) };
    let mut out = conn;
    let path = req.path.split('?').next().unwrap_or_default();
    let api = if path.ends_with("/messages") {
        Api::Anthropic
    } else if path.ends_with("/chat/completions") {
        Api::Openai
    } else if path.ends_with("/responses") {
        Api::Responses
    } else {
        let msg = format!(
            "pots-mock serves POST …/v1/messages, …/responses and …/chat/completions, not {} {}",
            req.method, path
        );
        return write_json(&mut out, 404, &error_body(&msg));
    };
    if req.method != "POST" {
        return write_json(&mut out, 405, &error_body("use POST"));
    }
    let body: Value = match serde_json::from_slice(&req.body) {
        Ok(v) => v,
        Err(e) => return write_json(&mut out, 400, &error_body(&format!("the body isn't JSON: {e}"))),
    };
    {
        let mut requests = state.requests.lock().unwrap();
        if requests.len() >= KEEP_REQUESTS {
            requests.remove(0);
        }
        requests.push(Recorded { path: path.to_string(), headers: req.headers, body: body.clone() });
    }
    let step = state.script.lock().unwrap().steps.pop_front().unwrap_or_else(|| improvise(api, &body));
    let id = state.next_id.fetch_add(1, Ordering::SeqCst);
    match step {
        Step::Error { status, message } => write_json(&mut out, status, &error_body(&message)),
        step => {
            let events = match api {
                Api::Anthropic => anthropic_events(&step, &body, id),
                Api::Openai => openai_events(&step, &body, id),
                Api::Responses => responses_events(&step, &body, id),
            };
            stream(&mut out, events, state.delay)
        }
    }
}

fn error_body(message: &str) -> Value {
    json!({"type": "error", "error": {"type": "mock_error", "message": message}})
}

fn write_json(out: &mut TcpStream, status: u16, body: &Value) -> io::Result<()> {
    let body = body.to_string();
    write!(
        out,
        "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        reason(status),
        body.len()
    )?;
    out.flush()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        529 => "Overloaded",
        _ => "Mock",
    }
}

/// Send SSE events as HTTP chunks, one at a time.
fn stream(out: &mut TcpStream, events: Vec<String>, delay: Duration) -> io::Result<()> {
    write!(
        out,
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n"
    )?;
    for (i, event) in events.iter().enumerate() {
        if i > 0 && !delay.is_zero() {
            thread::sleep(delay);
        }
        write!(out, "{:x}\r\n{event}\r\n", event.len())?;
        out.flush()?;
    }
    write!(out, "0\r\n\r\n")?;
    out.flush()
}

/// Small pieces of `text`, as a model streams it: a word and what follows.
fn pieces(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        if c.is_whitespace() && !cur.trim().is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// A rough token count: a token is about four bytes.
fn tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4).max(1)
}

fn anthropic_events(step: &Step, body: &Value, id: usize) -> Vec<String> {
    let ev = |kind: &str, data: Value| format!("event: {kind}\ndata: {data}\n\n");
    let model = body["model"].clone();
    let mut events = vec![ev(
        "message_start",
        json!({"type": "message_start", "message": {
            "id": format!("msg_mock_{id}"), "type": "message", "role": "assistant", "model": model, "content": [],
            "usage": {"input_tokens": tokens(&body["messages"].to_string()), "output_tokens": 1},
        }}),
    )];
    let (stop, output) = match step {
        Step::Text(text) => {
            events.push(ev("content_block_start", json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})));
            for p in pieces(text) {
                events.push(ev("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": p}})));
            }
            events.push(ev("content_block_stop", json!({"type": "content_block_stop", "index": 0})));
            ("end_turn", tokens(text))
        }
        Step::Tool { name, input } => {
            events.push(ev(
                "content_block_start",
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": format!("toolu_mock_{id}"), "name": name, "input": {}}}),
            ));
            let args = input.to_string();
            for p in split_in_two(&args) {
                events.push(ev("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": p}})));
            }
            events.push(ev("content_block_stop", json!({"type": "content_block_stop", "index": 0})));
            ("tool_use", tokens(&args))
        }
        Step::Error { .. } => unreachable!("errors aren't streamed"),
    };
    events.push(ev("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": stop}, "usage": {"output_tokens": output}})));
    events.push(ev("message_stop", json!({"type": "message_stop"})));
    events
}

fn openai_events(step: &Step, body: &Value, id: usize) -> Vec<String> {
    let chunk = |delta: Value, finish: Value| {
        let data = json!({
            "id": format!("chatcmpl-mock-{id}"), "object": "chat.completion.chunk", "model": body["model"],
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        });
        format!("data: {data}\n\n")
    };
    let mut events = Vec::new();
    let (finish, output) = match step {
        Step::Text(text) => {
            for (i, p) in pieces(text).into_iter().enumerate() {
                let delta = if i == 0 { json!({"role": "assistant", "content": p}) } else { json!({"content": p}) };
                events.push(chunk(delta, Value::Null));
            }
            ("stop", tokens(text))
        }
        Step::Tool { name, input } => {
            events.push(chunk(
                json!({"role": "assistant", "tool_calls": [{"index": 0, "id": format!("call_mock_{id}"), "type": "function", "function": {"name": name, "arguments": ""}}]}),
                Value::Null,
            ));
            let args = input.to_string();
            for p in split_in_two(&args) {
                events.push(chunk(json!({"tool_calls": [{"index": 0, "function": {"arguments": p}}]}), Value::Null));
            }
            ("tool_calls", tokens(&args))
        }
        Step::Error { .. } => unreachable!("errors aren't streamed"),
    };
    events.push(chunk(json!({}), json!(finish)));
    if body["stream_options"]["include_usage"] == json!(true) {
        let usage = json!({"prompt_tokens": tokens(&body["messages"].to_string()), "completion_tokens": output});
        events.push(format!("data: {}\n\n", json!({"id": format!("chatcmpl-mock-{id}"), "object": "chat.completion.chunk", "choices": [], "usage": usage})));
    }
    events.push("data: [DONE]\n\n".to_string());
    events
}

fn responses_events(step: &Step, body: &Value, id: usize) -> Vec<String> {
    let ev = |data: Value| format!("event: {}\ndata: {data}\n\n", data["type"].as_str().unwrap_or_default());
    let mut events = vec![ev(json!({"type": "response.created", "response": {"id": format!("resp_mock_{id}"), "status": "in_progress"}}))];
    let (item, output) = match step {
        Step::Text(text) => {
            let item_id = format!("msg_mock_{id}");
            events.push(ev(json!({
                "type": "response.output_item.added", "output_index": 0,
                "item": {"type": "message", "id": item_id, "role": "assistant", "status": "in_progress", "content": []},
            })));
            for p in pieces(text) {
                events.push(ev(json!({"type": "response.output_text.delta", "item_id": item_id, "output_index": 0, "content_index": 0, "delta": p})));
            }
            let item = json!({
                "type": "message", "id": item_id, "role": "assistant", "status": "completed",
                "content": [{"type": "output_text", "text": text, "annotations": []}],
            });
            (item, tokens(text))
        }
        Step::Tool { name, input } => {
            let (item_id, call_id) = (format!("fc_mock_{id}"), format!("call_mock_{id}"));
            events.push(ev(json!({
                "type": "response.output_item.added", "output_index": 0,
                "item": {"type": "function_call", "id": item_id, "call_id": call_id, "name": name, "arguments": "", "status": "in_progress"},
            })));
            let args = input.to_string();
            for p in split_in_two(&args) {
                events.push(ev(json!({"type": "response.function_call_arguments.delta", "item_id": item_id, "output_index": 0, "delta": p})));
            }
            let item = json!({"type": "function_call", "id": item_id, "call_id": call_id, "name": name, "arguments": args, "status": "completed"});
            (item, tokens(&args))
        }
        Step::Error { .. } => unreachable!("errors aren't streamed"),
    };
    events.push(ev(json!({"type": "response.output_item.done", "output_index": 0, "item": item})));
    let usage = json!({"input_tokens": tokens(&body["input"].to_string()), "output_tokens": output});
    events.push(ev(json!({
        "type": "response.completed",
        "response": {"id": format!("resp_mock_{id}"), "status": "completed", "model": body["model"], "output": [item], "usage": usage},
    })));
    events
}

/// Halves of `s` at a char boundary, so JSON arrives in more than one delta.
fn split_in_two(s: &str) -> Vec<&str> {
    let mut mid = s.len() / 2;
    while !s.is_char_boundary(mid) {
        mid += 1;
    }
    [&s[..mid], &s[mid..]].into_iter().filter(|p| !p.is_empty()).collect()
}

/// What the last message of a request was.
enum Last {
    /// A user's text.
    Text(String),
    /// Results of tool calls.
    ToolResults(Vec<String>),
}

fn last_message(api: Api, body: &Value) -> Last {
    let messages = body["messages"].as_array().map(Vec::as_slice).unwrap_or_default();
    match api {
        Api::Responses => {
            let input = body["input"].as_array().map(Vec::as_slice).unwrap_or_default();
            let results: Vec<String> = input
                .iter()
                .rev()
                .take_while(|i| i["type"] == "function_call_output")
                .map(|i| text_of(&i["output"]))
                .collect();
            if !results.is_empty() {
                return Last::ToolResults(results.into_iter().rev().collect());
            }
            match &body["input"] {
                Value::String(s) => Last::Text(s.clone()),
                _ => Last::Text(input.last().map(|i| text_of(&i["content"])).unwrap_or_default()),
            }
        }
        Api::Anthropic => {
            let Some(last) = messages.last() else { return Last::Text(String::new()) };
            let blocks: Vec<Value> = match &last["content"] {
                Value::String(s) => vec![json!({"type": "text", "text": s})],
                Value::Array(a) => a.clone(),
                _ => vec![],
            };
            let results: Vec<String> =
                blocks.iter().filter(|b| b["type"] == "tool_result").map(|b| text_of(&b["content"])).collect();
            if !results.is_empty() {
                return Last::ToolResults(results);
            }
            Last::Text(blocks.iter().filter(|b| b["type"] == "text").map(|b| text_of(&b["text"])).collect::<Vec<_>>().join("\n\n"))
        }
        Api::Openai => {
            let tools: Vec<String> = messages
                .iter()
                .rev()
                .take_while(|m| m["role"] == "tool")
                .map(|m| text_of(&m["content"]))
                .collect();
            if !tools.is_empty() {
                return Last::ToolResults(tools.into_iter().rev().collect());
            }
            Last::Text(messages.last().map(|m| text_of(&m["content"])).unwrap_or_default())
        }
    }
}

/// Text from a string, or from an array of text blocks.
fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// The answer when the script has run out.
fn improvise(api: Api, body: &Value) -> Step {
    match last_message(api, body) {
        Last::ToolResults(results) => {
            let mut text = String::from("The tool returned:\n");
            for r in results {
                let r = match r.char_indices().nth(1500) {
                    Some((i, _)) => format!("{}\n…", &r[..i]),
                    None => r,
                };
                text.push_str(&format!("\n```\n{}\n```\n", r.trim_end()));
            }
            Step::Text(text)
        }
        Last::Text(text) => command(&text).unwrap_or_else(|| {
            let quoted = text.trim().lines().map(|l| format!("> {l}")).collect::<Vec<_>>().join("\n");
            Step::Text(format!(
                "This is **pots-mock**, not a real model. You said:\n\n{quoted}\n\n\
                 Try `!tool list_channels {{}}`, \
                 `!tool ask_owner {{\"question\": \"Ship it?\", \"options\": [\"Yes\", \"No\"]}}` or `!error 529 Overloaded`."
            ))
        }),
    }
}

/// A `!tool NAME {json}` or `!error STATUS message` command in `text`.
fn command(text: &str) -> Option<Step> {
    if let Some(i) = text.find("!tool ") {
        let rest = text[i + "!tool ".len()..].trim();
        let (name, input) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let input = input.trim();
        let input = if input.is_empty() { Ok(json!({})) } else { serde_json::from_str(input) };
        return Some(match input {
            Ok(input) => Step::Tool { name: name.to_string(), input },
            Err(e) => Step::Text(format!("pots-mock: `!tool {name}` needs a JSON object after the name ({e}).")),
        });
    }
    if let Some(i) = text.find("!error ") {
        let rest = text[i + "!error ".len()..].trim();
        let (status, message) = rest.split_once(char::is_whitespace).unwrap_or((rest, "mock error"));
        let status = status.parse().ok().filter(|s| (400..600).contains(s)).unwrap_or(500);
        return Some(Step::Error { status, message: message.trim().to_string() });
    }
    None
}

/// Serve one canned HTTP response with `status` (e.g. `"200 OK"`) and an
/// event-stream `body`. Returns the base URL and a handle that yields the
/// raw request the server received.
pub fn serve_once(status: &str, body: impl Into<String>) -> (String, thread::JoinHandle<String>) {
    let (url, handle) = serve(vec![(status.to_string(), body.into())]);
    (url, thread::spawn(move || handle.join().unwrap().remove(0)))
}

/// Serve one canned response per connection, in order: (status, body).
/// Returns the base URL and a handle that yields the raw requests received.
pub fn serve(responses: Vec<(String, String)>) -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = thread::spawn(move || responses.into_iter().map(|(status, body)| answer(&listener, &status, &body)).collect());
    (url, handle)
}

fn answer(listener: &TcpListener, status: &str, body: &str) -> String {
    {
        let (mut sock, _) = listener.accept().unwrap();
        let mut req = Vec::new();
        let mut buf = [0u8; 4096];
        // Read headers, then as much body as Content-Length says.
        loop {
            let n = sock.read(&mut buf).unwrap();
            req.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&req);
            if let Some(end) = text.find("\r\n\r\n") {
                let len = text[..end]
                    .lines()
                    .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
                    .unwrap_or(0);
                if req.len() >= end + 4 + len {
                    break;
                }
            }
        }
        write!(
            sock,
            "HTTP/1.1 {status}\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        String::from_utf8(req).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_text_into_word_pieces() {
        assert_eq!(pieces("Hello there, you"), ["Hello", " there,", " you"]);
        assert_eq!(pieces(""), Vec::<String>::new());
    }

    #[test]
    fn reads_commands_from_the_message() {
        assert_eq!(command("Owner: !tool list_channels"), Some(Step::Tool { name: "list_channels".into(), input: json!({}) }));
        assert_eq!(
            command("Owner: please !tool read_channel {\"channel\": \"general\"}"),
            Some(Step::Tool { name: "read_channel".into(), input: json!({"channel": "general"}) })
        );
        assert_eq!(command("!error 529 Overloaded now"), Some(Step::Error { status: 529, message: "Overloaded now".into() }));
        assert_eq!(command("just chatting"), None);
    }

    #[test]
    fn finds_tool_results_in_either_format() {
        let anthropic = json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": "hi"}]},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "t", "name": "x", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t", "content": "42"}]},
        ]});
        assert!(matches!(last_message(Api::Anthropic, &anthropic), Last::ToolResults(r) if r == ["42"]));
        let openai = json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": null, "tool_calls": []},
            {"role": "tool", "tool_call_id": "t", "content": "42"},
        ]});
        assert!(matches!(last_message(Api::Openai, &openai), Last::ToolResults(r) if r == ["42"]));
        assert!(matches!(last_message(Api::Openai, &json!({"messages": [{"role": "user", "content": "yo"}]})), Last::Text(t) if t == "yo"));
    }
}
