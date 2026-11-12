//! A small MCP server over HTTP (the "streamable HTTP" transport, answering
//! every request with plain JSON) that offers pots's chat tools to a Claude
//! Code reply. It handles the tool list itself; each tool call is handed to
//! whoever started the server, which answers it through [`Call::answer`].
//!
//! It serves one reply: one secret path, on a port of its own, gone when
//! dropped.

use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;

use serde_json::{Value, json};

use pots_llm::ToolSpec;

/// What clients are told the server is called; tools show up in Claude
/// Code as `mcp__pots__<name>`.
pub const NAME: &str = "pots";
const PROTOCOL_VERSION: &str = "2025-06-18";
const MAX_BODY: usize = 16 << 20;

/// A tool call waiting for its result.
pub struct Call {
    pub name: String,
    pub input: Value,
    answer: mpsc::Sender<(String, bool)>,
}

impl Call {
    /// Send the result text, and whether it failed, back to the client.
    pub fn answer(self, text: String, is_error: bool) {
        let _ = self.answer.send((text, is_error));
    }
}

pub struct Server {
    addr: SocketAddr,
    path: String,
    stopping: Arc<AtomicBool>,
}

impl Server {
    /// Serve `tools` on `bind` (port 0 picks a free one) at `/mcp/<secret>`.
    /// Each tool call goes to `calls`; one whose `Call` is dropped
    /// unanswered fails.
    pub fn start(
        bind: &str,
        secret: &str,
        tools: Vec<ToolSpec>,
        calls: impl Fn(Call) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(bind)?;
        let addr = listener.local_addr()?;
        let path = format!("/mcp/{secret}");
        let stopping = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Shared { path: path.clone(), tools, calls: Box::new(calls) });
        let stop = stopping.clone();
        thread::Builder::new().name("pots-mcp".into()).spawn(move || {
            for conn in listener.incoming() {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(conn) = conn else { continue };
                let shared = shared.clone();
                let _ = thread::Builder::new().name("pots-mcp-conn".into()).spawn(move || {
                    let _ = serve(conn, &shared);
                });
            }
        })?;
        Ok(Self { addr, path, stopping })
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The URL to reach the server at, as seen from `host`.
    pub fn url(&self, host: &str) -> String {
        format!("http://{host}:{}{}", self.port(), self.path)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        // Wake the accept loop so it sees it's stopping.
        let _ = TcpStream::connect(("127.0.0.1", self.addr.port()));
    }
}

struct Shared {
    path: String,
    tools: Vec<ToolSpec>,
    calls: Box<dyn Fn(Call) + Send + Sync>,
}

struct HttpRequest {
    method: String,
    path: String,
    close: bool,
    body: Vec<u8>,
}

/// Answer requests on one connection until the client closes it.
fn serve(conn: TcpStream, shared: &Shared) -> io::Result<()> {
    let mut reader = BufReader::new(conn.try_clone()?);
    let mut out = conn;
    while let Some(req) = read_request(&mut reader)? {
        let (status, body) = respond(&req, shared);
        let mut head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n", body.len());
        if !body.is_empty() {
            head.push_str("Content-Type: application/json\r\n");
        }
        if status.starts_with("405") {
            head.push_str("Allow: POST\r\n");
        }
        head.push_str("\r\n");
        out.write_all(head.as_bytes())?;
        out.write_all(&body)?;
        out.flush()?;
        if req.close {
            break;
        }
    }
    Ok(())
}

fn respond(req: &HttpRequest, shared: &Shared) -> (&'static str, Vec<u8>) {
    if req.path.split('?').next() != Some(shared.path.as_str()) {
        return ("404 Not Found", Vec::new());
    }
    if req.method != "POST" {
        // No server-to-client stream, and no sessions to end.
        return ("405 Method Not Allowed", Vec::new());
    }
    let Ok(message) = serde_json::from_slice::<Value>(&req.body) else {
        let error = json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "Parse error"}});
        return ("400 Bad Request", error.to_string().into_bytes());
    };
    let answer = match message {
        Value::Array(batch) => {
            let answers: Vec<Value> = batch.iter().filter_map(|m| handle(m, shared)).collect();
            (!answers.is_empty()).then(|| Value::Array(answers))
        }
        m => handle(&m, shared),
    };
    match answer {
        Some(a) => ("200 OK", a.to_string().into_bytes()),
        // Only notifications or responses: nothing to say.
        None => ("202 Accepted", Vec::new()),
    }
}

/// The response to one JSON-RPC message; `None` for notifications.
fn handle(message: &Value, shared: &Shared) -> Option<Value> {
    let id = message.get("id")?.clone();
    let method = message["method"].as_str()?;
    let params = &message["params"];
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or(PROTOCOL_VERSION),
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": NAME, "version": env!("CARGO_PKG_VERSION")},
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({
            "tools": shared.tools.iter().map(|t| json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": t.input_schema,
            })).collect::<Vec<_>>(),
        })),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or_default();
            if !shared.tools.iter().any(|t| t.name == name) {
                Err((-32602, format!("Unknown tool: {name}")))
            } else {
                let input = match &params["arguments"] {
                    Value::Null => json!({}),
                    a => a.clone(),
                };
                let (tx, rx) = mpsc::channel();
                (shared.calls)(Call { name: name.to_string(), input, answer: tx });
                let (text, is_error) =
                    rx.recv().unwrap_or_else(|_| ("The reply ended before this tool ran.".to_string(), true));
                Ok(json!({"content": [{"type": "text", "text": text}], "isError": is_error}))
            }
        }
        _ => Err((-32601, format!("Method not found: {method}"))),
    };
    Some(match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    })
}

/// The next request on the connection, or `None` once it's closed.
fn read_request(reader: &mut impl BufRead) -> io::Result<Option<HttpRequest>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad request line"));
    };
    let (method, path) = (method.to_string(), path.to_string());
    let (mut length, mut chunked, mut close) = (0usize, false, false);
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else { continue };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => length = value.parse().unwrap_or(0),
            "transfer-encoding" => chunked = value.eq_ignore_ascii_case("chunked"),
            "connection" => close = value.eq_ignore_ascii_case("close"),
            _ => {}
        }
    }
    let body = match chunked {
        true => read_chunked(reader)?,
        false if length > MAX_BODY => return Err(io::Error::new(io::ErrorKind::InvalidData, "body too large")),
        false => {
            let mut body = vec![0; length];
            reader.read_exact(&mut body)?;
            body
        }
    };
    Ok(Some(HttpRequest { method, path, close, body }))
}

fn read_chunked(reader: &mut impl BufRead) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        reader.read_line(&mut line)?;
        let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or(""), 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
        if size == 0 {
            // Trailers, up to the blank line.
            loop {
                line.clear();
                if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
                    return Ok(body);
                }
            }
        }
        if body.len() + size > MAX_BODY {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "body too large"));
        }
        let start = body.len();
        body.resize(start + size, 0);
        reader.read_exact(&mut body[start..])?;
        line.clear();
        reader.read_line(&mut line)?;
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    fn post(server: &Server, body: &Value) -> (String, Value) {
        let mut conn = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
        let body = body.to_string();
        write!(
            conn,
            "POST {} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            server.path,
            body.len()
        )
        .unwrap();
        let mut response = String::new();
        conn.read_to_string(&mut response).unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        let status = head.lines().next().unwrap().to_string();
        (status, serde_json::from_str(body).unwrap_or(Value::Null))
    }

    fn server() -> Server {
        let tools = vec![ToolSpec {
            name: "echo",
            description: "Echo it.",
            input_schema: json!({"type": "object", "properties": {"text": {"type": "string"}}}),
        }];
        Server::start("127.0.0.1:0", "s3cret", tools, |call: Call| {
            let text = call.input["text"].as_str().unwrap_or_default().to_string();
            match text.as_str() {
                "drop" => drop(call),
                _ => call.answer(format!("echo: {text}"), false),
            }
        })
        .unwrap()
    }

    #[test]
    fn speaks_mcp() {
        let server = server();
        let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}});
        let (status, body) = post(&server, &init);
        assert!(status.contains("200"), "{status}");
        assert_eq!(body["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(body["result"]["serverInfo"]["name"], "pots");

        let (status, _) = post(&server, &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        assert!(status.contains("202"), "{status}");

        let (_, body) = post(&server, &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
        assert_eq!(body["result"]["tools"][0]["name"], "echo");
        assert_eq!(body["result"]["tools"][0]["inputSchema"]["type"], "object");

        let call = json!({"jsonrpc": "2.0", "id": "c", "method": "tools/call", "params": {"name": "echo", "arguments": {"text": "hi"}}});
        let (_, body) = post(&server, &call);
        assert_eq!(body["id"], "c");
        assert_eq!(body["result"]["content"][0]["text"], "echo: hi");
        assert_eq!(body["result"]["isError"], false);

        let dropped = json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "echo", "arguments": {"text": "drop"}}});
        assert_eq!(post(&server, &dropped).1["result"]["isError"], true);

        let unknown = json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "rm", "arguments": {}}});
        assert_eq!(post(&server, &unknown).1["error"]["code"], -32602);
    }

    #[test]
    fn only_answers_its_secret_path() {
        let server = server();
        let mut conn = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
        write!(conn, "POST /mcp/guess HTTP/1.1\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}").unwrap();
        let mut response = String::new();
        conn.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 404"), "{response}");
    }

    #[test]
    fn reads_chunked_bodies_and_keeps_connections_open() {
        let server = server();
        let mut conn = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
        let ping = json!({"jsonrpc": "2.0", "id": 7, "method": "ping"}).to_string();
        write!(conn, "POST {} HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{ping}\r\n0\r\n\r\n", server.path, ping.len()).unwrap();
        write!(conn, "GET {} HTTP/1.1\r\nConnection: close\r\n\r\n", server.path).unwrap();
        let mut response = String::new();
        conn.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("\"id\":7"), "{response}");
        assert!(response.contains("HTTP/1.1 405"), "{response}");
    }
}
