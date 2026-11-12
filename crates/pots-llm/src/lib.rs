//! Provider-neutral chat model client.
//!
//! A [`Request`] is a list of [`Turn`]s; [`stream`] sends it to an
//! Anthropic- or OpenAI-compatible endpoint on a background thread and
//! reports progress as [`Event`]s, ending with the full assistant [`Turn`].
//! [`cost`] prices the [`Usage`] a reply reports.

mod anthropic;
pub mod cost;
mod openai;
mod responses;
mod sse;

use std::collections::{BTreeMap, HashMap};
use std::io::BufReader;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use futures::channel::mpsc;
use serde::{Deserialize, Serialize};
use serde_json::Value;


/// Which wire format a provider speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Api {
    Anthropic,
    Openai,
    /// The owner's own Claude Code CLI (`claude -p`), on their subscription.
    /// It runs the whole agent loop itself, so [`stream`] doesn't speak it:
    /// pots-agent drives the CLI instead.
    #[serde(rename = "claude-code")]
    ClaudeCode,
}

impl Api {
    /// The name config.toml uses for it.
    pub fn as_str(self) -> &'static str {
        match self {
            Api::Anthropic => "anthropic",
            Api::Openai => "openai",
            Api::ClaudeCode => "claude-code",
        }
    }

    /// Where the official API is. Claude Code has no URL: it's a program.
    pub fn default_base_url(self) -> &'static str {
        match self {
            Api::Anthropic => "https://api.anthropic.com",
            Api::Openai => "https://api.openai.com/v1",
            Api::ClaudeCode => "",
        }
    }
}

/// Which OpenAI endpoint an `api = "openai"` provider is called at.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Endpoint {
    /// Try the Responses API first, falling back to Chat Completions if the
    /// server doesn't have it. What worked is remembered per base URL.
    #[default]
    Auto,
    /// `POST {base_url}/responses`.
    Responses,
    /// `POST {base_url}/chat/completions`.
    Chat,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub api: Api,
    /// Unused (and optional) for `api = "claude-code"`.
    #[serde(default)]
    pub base_url: String,
    /// `api = "claude-code"`: the `claude` CLI on this machine, if it isn't
    /// found on its own.
    pub command: Option<String>,
    /// A key written into config.toml itself (plain text, kept working for
    /// hand-written configs).
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    /// The key from keys.toml, filled in after parsing.
    #[serde(skip)]
    pub stored_key: Option<String>,
    /// Extra HTTP headers sent with every request.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Anthropic: automatic prompt caching (default on).
    pub prompt_cache: Option<bool>,
    /// OpenAI: `stream_options.include_usage` for token counts (default on).
    pub stream_usage: Option<bool>,
    /// OpenAI: which endpoint to call (default: detect it).
    #[serde(default)]
    pub endpoint: Endpoint,
}

impl ProviderConfig {
    /// The API key, if one is configured and present.
    pub fn key(&self) -> Option<String> {
        let present = |k: &Option<String>| k.clone().filter(|k| !k.is_empty());
        present(&self.api_key)
            .or_else(|| present(&self.stored_key))
            .or_else(|| present(&self.api_key_env.as_ref().and_then(|var| std::env::var(var).ok())))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Part {
    Text(String),
    ToolCall { id: String, name: String, input: Value },
    ToolResult { id: String, content: String, is_error: bool },
    /// A provider-specific block (e.g. Anthropic `thinking`) that must be sent
    /// back unchanged within the same exchange. Other providers drop it.
    Raw(Value),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Turn {
    pub role: Role,
    pub parts: Vec<Part>,
}

impl Turn {
    pub fn user(text: impl Into<String>) -> Self {
        Self { role: Role::User, parts: vec![Part::Text(text.into())] }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self { role: Role::Assistant, parts: vec![Part::Text(text.into())] }
    }

    pub fn tool_calls(&self) -> impl Iterator<Item = (&str, &str, &Value)> {
        self.parts.iter().filter_map(|p| match p {
            Part::ToolCall { id, name, input } => Some((id.as_str(), name.as_str(), input)),
            _ => None,
        })
    }
}

#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

#[derive(Clone, Debug)]
pub struct Request {
    pub model: String,
    pub system: String,
    pub turns: Vec<Turn>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: Option<u32>,
    /// Merged into the top level of the request body.
    pub extra: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    EndTurn,
    ToolUse,
    MaxTokens,
    Refusal,
    Other(String),
}

/// Tokens one or more model calls used.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Uncached input.
    pub input: u64,
    pub output: u64,
    #[serde(default)]
    pub cache_write: u64,
    #[serde(default)]
    pub cache_read: u64,
}

impl Usage {
    pub fn add(&mut self, other: Usage) {
        self.input += other.input;
        self.output += other.output;
        self.cache_write += other.cache_write;
        self.cache_read += other.cache_read;
    }

    /// All input tokens, cached or not.
    pub fn total_input(&self) -> u64 {
        self.input + self.cache_write + self.cache_read
    }
}

#[derive(Debug)]
pub enum Event {
    /// More reply text.
    Text(String),
    /// The model started calling a tool.
    ToolCall(String),
    /// The reply is complete.
    Done(Turn, Stop, Usage),
    Error(String),
}

#[derive(Clone, Debug)]
pub struct Provider {
    pub api: Api,
    pub base_url: String,
    pub key: Option<String>,
    pub headers: BTreeMap<String, String>,
    /// Anthropic: ask for automatic prompt caching.
    pub prompt_cache: bool,
    /// OpenAI: ask for token usage at the end of the stream.
    pub stream_usage: bool,
    /// OpenAI: Responses, Chat Completions, or whichever the server has.
    pub endpoint: Endpoint,
    /// Claude Code: the configured `claude` CLI.
    pub command: Option<String>,
}

impl Provider {
    pub fn new(config: &ProviderConfig) -> Self {
        Self {
            api: config.api,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            key: config.key(),
            headers: config.headers.clone(),
            prompt_cache: config.prompt_cache.unwrap_or(true),
            stream_usage: config.stream_usage.unwrap_or(true),
            endpoint: config.endpoint,
            command: config.command.clone(),
        }
    }

    /// The endpoints to try, in order.
    fn wires(&self) -> Vec<Wire> {
        match (self.api, self.endpoint) {
            (Api::Anthropic, _) => vec![Wire::Anthropic],
            (Api::ClaudeCode, _) => vec![],
            (Api::Openai, Endpoint::Responses) => vec![Wire::Responses],
            (Api::Openai, Endpoint::Chat) => vec![Wire::Chat],
            (Api::Openai, Endpoint::Auto) => match detected().lock().unwrap().get(&self.base_url) {
                Some(&wire) => vec![wire],
                None => vec![Wire::Responses, Wire::Chat],
            },
        }
    }
}

/// One concrete wire format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wire {
    Anthropic,
    Responses,
    Chat,
}

/// The OpenAI endpoint each auto-detecting base URL turned out to have.
fn detected() -> &'static Mutex<HashMap<String, Wire>> {
    static DETECTED: OnceLock<Mutex<HashMap<String, Wire>>> = OnceLock::new();
    DETECTED.get_or_init(Default::default)
}

/// An HTTP request ready to send.
struct HttpRequest {
    url: String,
    headers: Vec<(String, String)>,
    body: Value,
}

/// Turns SSE events of one API into [`Event`]s.
trait Decoder: Send {
    /// Handle one event, pushing what it produced to `out`. Returns `true`
    /// once the stream is complete.
    fn feed(&mut self, event: &sse::Event, out: &mut Vec<Event>) -> Result<bool, String>;
    /// The finished assistant turn.
    fn finish(self: Box<Self>) -> Result<(Turn, Stop), String>;
    /// Tokens used, as far as the stream reported them.
    fn usage(&self) -> Usage;
}

fn build(provider: &Provider, req: &Request, wire: Wire) -> (HttpRequest, Box<dyn Decoder>) {
    let mut http = match wire {
        Wire::Anthropic => anthropic::request(provider, req),
        Wire::Responses => responses::request(provider, req),
        Wire::Chat => openai::request(provider, req),
    };
    if let (Some(Value::Object(extra)), Value::Object(body)) = (&req.extra, &mut http.body) {
        body.extend(extra.clone());
    }
    for (k, v) in &provider.headers {
        http.headers.push((k.clone(), v.clone()));
    }
    let decoder: Box<dyn Decoder> = match wire {
        Wire::Anthropic => Box::new(anthropic::Decoder::default()),
        Wire::Responses => Box::new(responses::Decoder::default()),
        Wire::Chat => Box::new(openai::Decoder::default()),
    };
    (http, decoder)
}

/// One way to send the request: the HTTP request, how to read the reply,
/// and which wire format it is.
struct Attempt {
    http: HttpRequest,
    decoder: Box<dyn Decoder>,
    wire: Wire,
}

/// Start streaming a reply. The receiver yields events until `Done` or
/// `Error`; dropping it cancels the request.
pub fn stream(provider: &Provider, req: &Request) -> mpsc::UnboundedReceiver<Event> {
    let (tx, rx) = mpsc::unbounded();
    if provider.api == Api::ClaudeCode {
        let _ = tx.unbounded_send(Event::Error("Claude Code runs the reply itself; it isn't an HTTP API.".into()));
        return rx;
    }
    let attempts: Vec<Attempt> = provider
        .wires()
        .into_iter()
        .map(|wire| {
            let (http, decoder) = build(provider, req, wire);
            Attempt { http, decoder, wire }
        })
        .collect();
    let base_url = provider.base_url.clone();
    let spawned = thread::Builder::new().name("llm-stream".into()).spawn(move || {
        if let Err(e) = run(attempts, &base_url, &tx) {
            let _ = tx.unbounded_send(Event::Error(e));
        }
    });
    if let Err(e) = spawned {
        let (tx, rx) = mpsc::unbounded();
        let _ = tx.unbounded_send(Event::Error(format!("couldn't start request thread: {e}")));
        return rx;
    }
    rx
}

/// Whether an HTTP status says the server has no such endpoint (as opposed
/// to an error with the request itself), so another endpoint is worth a try.
fn no_such_endpoint(status: u16) -> bool {
    matches!(status, 404 | 405 | 501)
}

/// Send the first attempt the server has an endpoint for, and stream its reply.
fn run(attempts: Vec<Attempt>, base_url: &str, tx: &mpsc::UnboundedSender<Event>) -> Result<(), String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(30)))
        .build()
        .into();
    let detecting = attempts.len() > 1;
    let count = attempts.len();
    for (i, Attempt { http, decoder, wire }) in attempts.into_iter().enumerate() {
        let mut req = agent.post(&http.url);
        for (k, v) in &http.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let mut resp = req
            .send(http.body.to_string())
            .map_err(|e| format!("request to {} failed: {e}", http.url))?;
        let status = resp.status().as_u16();
        if status >= 400 {
            if i + 1 < count && no_such_endpoint(status) {
                continue;
            }
            let text = resp.body_mut().read_to_string().unwrap_or_default();
            return Err(format!("HTTP {status}: {}", error_message(&text)));
        }
        if detecting {
            detected().lock().unwrap().insert(base_url.to_string(), wire);
        }
        return read_stream(resp, decoder, tx);
    }
    Err("no endpoint to send the request to".into())
}

fn read_stream(
    resp: ureq::http::Response<ureq::Body>,
    mut decoder: Box<dyn Decoder>,
    tx: &mpsc::UnboundedSender<Event>,
) -> Result<(), String> {

    let mut events = sse::Reader::new(BufReader::new(resp.into_body().into_reader()));
    let mut out = Vec::new();
    loop {
        let event = events.next_event().map_err(|e| format!("connection lost: {e}"))?;
        let Some(event) = event else { break };
        let done = decoder.feed(&event, &mut out)?;
        for e in out.drain(..) {
            if tx.unbounded_send(e).is_err() {
                return Ok(()); // nobody is listening any more: cancelled
            }
        }
        if done {
            break;
        }
    }
    let usage = decoder.usage();
    let (turn, stop) = decoder.finish()?;
    let _ = tx.unbounded_send(Event::Done(turn, stop, usage));
    Ok(())
}

/// The human-readable part of an error body, from either API's shape.
fn error_message(body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let msg = match &v["error"] {
        Value::String(s) => Some(s.clone()),
        e => e["message"].as_str().map(str::to_string),
    };
    msg.unwrap_or_else(|| {
        let body = body.trim();
        match body.char_indices().nth(300) {
            Some((i, _)) => format!("{}…", &body[..i]),
            None => body.to_string(),
        }
    })
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use pots_mock::serve_once;
    use serde_json::json;

    use super::*;

    fn collect(provider: &Provider, req: &Request) -> Vec<Event> {
        futures::executor::block_on(stream(provider, req).collect::<Vec<_>>())
    }

    fn request() -> Request {
        Request {
            model: "m".into(),
            system: "be brief".into(),
            turns: vec![Turn::user("hi")],
            tools: vec![],
            max_tokens: None,
            extra: Some(json!({"output_config": {"effort": "low"}})),
        }
    }

    #[test]
    fn streams_an_anthropic_reply_over_http() {
        let (url, server) = serve_once(
            "200 OK",
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{}}\n\n\
             event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
             event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n\
             event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n\n\
             event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
             event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n\
             event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let provider = provider(Api::Anthropic, url, Some("k"), Endpoint::Auto);
        let events = collect(&provider, &request());
        let raw = server.join().unwrap();
        assert!(raw.starts_with("POST /v1/messages "), "{raw}");
        assert!(raw.contains("x-api-key: k"), "{raw}");
        assert!(raw.contains("\"effort\":\"low\""), "extra_body is merged: {raw}");

        let text: String = events.iter().filter_map(|e| match e { Event::Text(t) => Some(t.as_str()), _ => None }).collect();
        assert_eq!(text, "Hello");
        match events.last() {
            Some(Event::Done(turn, Stop::EndTurn, _)) => assert_eq!(turn, &Turn::assistant("Hello")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn reports_http_errors_with_the_api_message() {
        let (url, server) = serve_once(
            "401 Unauthorized",
            "{\"error\":{\"message\":\"Incorrect API key provided\",\"type\":\"invalid_request_error\"}}",
        );
        let provider = provider(Api::Openai, url, Some("bad"), Endpoint::Chat);
        let events = collect(&provider, &request());
        let raw = server.join().unwrap();
        assert!(raw.starts_with("POST /chat/completions "), "{raw}");
        assert!(raw.contains("authorization: Bearer bad"), "{raw}");
        match events.as_slice() {
            [Event::Error(e)] => assert_eq!(e, "HTTP 401: Incorrect API key provided"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_stream_cut_short_is_an_error() {
        let (url, server) = serve_once(
            "200 OK",
            "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
        );
        let provider = provider(Api::Openai, url, None, Endpoint::Chat);
        let events = collect(&provider, &request());
        server.join().unwrap();
        assert!(matches!(events.as_slice(), [Event::Text(_), Event::Error(e)] if e.contains("ended")), "{events:?}");
    }

    const CHAT_REPLY: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    const RESPONSES_REPLY: &str = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"ok\"}\n\n\
         event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n\n";

    fn served(responses: &[(&str, &str)]) -> (String, thread::JoinHandle<Vec<String>>) {
        pots_mock::serve(responses.iter().map(|(s, b)| (s.to_string(), b.to_string())).collect())
    }

    fn reply_text(events: &[Event]) -> Option<&Turn> {
        match events.last() {
            Some(Event::Done(turn, Stop::EndTurn, _)) => Some(turn),
            _ => None,
        }
    }

    #[test]
    fn auto_prefers_the_responses_api_and_remembers_it() {
        let (url, server) = served(&[("200 OK", RESPONSES_REPLY), ("200 OK", RESPONSES_REPLY)]);
        let provider = provider(Api::Openai, url, Some("k"), Endpoint::Auto);
        for _ in 0..2 {
            let events = collect(&provider, &request());
            assert_eq!(reply_text(&events), Some(&Turn::assistant("ok")), "{events:?}");
        }
        let raw = server.join().unwrap();
        assert!(raw.iter().all(|r| r.starts_with("POST /responses ")), "{raw:?}");
        assert!(raw[0].contains("\"instructions\":\"be brief\""), "{}", raw[0]);
    }

    #[test]
    fn auto_falls_back_to_chat_completions_and_remembers_it() {
        let (url, server) = served(&[
            ("404 Not Found", "{\"error\":{\"message\":\"no such route\"}}"),
            ("200 OK", CHAT_REPLY),
            ("200 OK", CHAT_REPLY),
        ]);
        let provider = provider(Api::Openai, url, None, Endpoint::Auto);
        for _ in 0..2 {
            let events = collect(&provider, &request());
            assert_eq!(reply_text(&events), Some(&Turn::assistant("ok")), "{events:?}");
        }
        let raw = server.join().unwrap();
        let paths: Vec<&str> = raw.iter().map(|r| r.split(' ').nth(1).unwrap()).collect();
        assert_eq!(paths, ["/responses", "/chat/completions", "/chat/completions"], "one probe, then straight to chat");
    }

    #[test]
    fn auto_does_not_fall_back_on_other_errors() {
        let (url, server) = served(&[("401 Unauthorized", "{\"error\":{\"message\":\"bad key\"}}")]);
        let provider = provider(Api::Openai, url, Some("bad"), Endpoint::Auto);
        let events = collect(&provider, &request());
        assert_eq!(server.join().unwrap().len(), 1);
        assert!(matches!(events.as_slice(), [Event::Error(e)] if e == "HTTP 401: bad key"), "{events:?}");
    }

    #[test]
    fn a_failed_fallback_reports_the_last_error_and_remembers_nothing() {
        let (url, server) = served(&[
            ("404 Not Found", "not found"),
            ("404 Not Found", "{\"error\":{\"message\":\"wrong base URL?\"}}"),
        ]);
        let provider = provider(Api::Openai, url.clone(), None, Endpoint::Auto);
        let events = collect(&provider, &request());
        server.join().unwrap();
        assert!(matches!(events.as_slice(), [Event::Error(e)] if e == "HTTP 404: wrong base URL?"), "{events:?}");
        assert!(!detected().lock().unwrap().contains_key(&url));
    }

    fn provider(api: Api, base_url: String, key: Option<&str>, endpoint: Endpoint) -> Provider {
        Provider {
            api,
            base_url,
            key: key.map(str::to_string),
            headers: BTreeMap::new(),
            prompt_cache: false,
            stream_usage: false,
            endpoint,
            command: None,
        }
    }
}
