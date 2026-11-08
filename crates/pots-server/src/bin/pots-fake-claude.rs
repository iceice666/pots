//! 測試用的假 Claude Code CLI：說和官方 `claude --print --input-format stream-json
//! --output-format stream-json` 一樣的協定，照 JSON 劇本確定性地輸出，平台工具則真的透過
//! MCP（rmcp streamable HTTP client）呼叫。不是給使用者的程式。
//!
//! # 參數
//!
//! 接受 adapter 附加的參數（`--flag value` 或 `--flag=value` 都可以）：`--mcp-config`、
//! `--session-id`、`--resume`、`--model`、`--allowedTools`、`--disallowedTools`、
//! `--append-system-prompt`、`--permission-mode`…；不認得的參數忽略。
//!
//! # 環境變數
//!
//! - `POTS_FAKE_CLAUDE_SCRIPT`：劇本檔路徑（格式見下）。沒設時劇本是空的（第一則訊息直接得到
//!   一個成功的 `result`）。劇本讀不到或格式錯時 exit 2。
//! - `POTS_FAKE_CLAUDE_LOG`：記錄檔路徑（append）。每行一個 JSON：
//!   - `{"kind":"argv","argv":[...],"cwd":"..."}`：啟動時一次（`argv` 不含程式名）。
//!   - `{"kind":"stdin","frame":{...}}`：每一個收到的 stdin frame（原樣；非 JSON 行記成字串）。
//!   - `{"kind":"tool","name":"...","input":{...},"is_error":bool,"output":"..."}`：每個執行完的
//!     工具（MCP 或原生）。
//! - `CLAUDE_CONFIG_DIR`（沒設時是 `$HOME/.claude`）：session transcript 放在
//!   `<config>/projects/<cwd 把非 ASCII 英數字元換成 '-'>/<session_id>.jsonl`。
//!
//! # Session
//!
//! - `--session-id=<id>`：新的 session；transcript 已經存在時像真的 CLI 一樣回錯誤
//!   （`Session ID <id> is already in use.`）並 exit 1。
//! - `--resume=<id>`：transcript 不存在時立刻輸出錯誤 `result`
//!   （`is_error: true`，`result` 為 `No conversation found with session ID: <id>`），stderr 也
//!   印同一行，exit 1——不等 `initialize`。
//! - 都沒有時自己產生一個 UUID。
//! - 每則使用者訊息與 assistant 訊息都 append 到 transcript（第一則使用者訊息時建立檔案）。
//!
//! # 協定
//!
//! - `control_request` `initialize` → `control_response` success（隨時都會回）。
//! - `control_request` `interrupt` → 回 success；如果正在跑某一輪，放棄這輪剩下的步驟（直到並含
//!   下一個 `end_turn`），輸出 `result`（`subtype: "error_during_execution"`、`is_error: true`），
//!   然後等下一則使用者訊息或 EOF。閒置時的 interrupt 只回 success。
//! - 其他 `control_request` → 回 `error`。
//! - stdin 由獨立的 task 讀，所以 sleep、MCP 呼叫、等權限時也看得到 interrupt。
//! - 每一輪（收到一則使用者訊息）開始時輸出 `system/init`：`session_id`、`cwd`、`model`、
//!   `tools`（原生工具扣掉 `--disallowedTools`，加上 MCP server 列出的 `mcp__pots__*`）、
//!   `mcp_servers`（`[{"name":"pots","status":"connected"|"failed"}]`）、
//!   `claude_code_version: "0.0.0-fake"`。MCP server 在第一輪開始時連線（10 秒內）。
//! - stdin EOF：閒置時 exit 0；正在跑的一輪會跑完（到 `end_turn` 或劇本結束）才結束。
//! - stdout 每行 flush。
//!
//! # 劇本
//!
//! ```json
//! {
//!   "new":    [ <step>, ... ],
//!   "resume": [ <step>, ... ]
//! }
//! ```
//!
//! 有 `--resume` 時用 `resume`（省略時沿用 `new`），否則用 `new`。也可以直接給一個陣列
//! （等同只有 `new`）。步驟在收到第一則使用者訊息後依序執行；一輪的 `num_turns` 是這輪輸出的
//! assistant 訊息數（至少 1），`total_cost_usd` 每個 `result` 累加 0.01。
//!
//! | 步驟 | 行為 |
//! |---|---|
//! | `{"text": "..."}` | assistant 訊息，一個 `text` block。 |
//! | `{"mcp": {"tool": "<name>", "args": {...}}}` | assistant `tool_use`（`mcp__pots__<name>`）；工具不在 `--allowedTools`（完整名稱、`mcp__pots` 或 `mcp__pots__*`）時先送 `can_use_tool` 等回覆；允許的話用 mcp-config 裡 `pots` server（`type: "http"`、`url`、`headers`）呼叫工具；然後輸出 user `tool_result`（結果文字、`is_error`）。連不上或呼叫失敗也是 `is_error` 的 `tool_result`。 |
//! | `{"native": {"tool": "Bash", "input": {"command": "..."}}}` | assistant `tool_use`；在 `--disallowedTools` 裡時直接回錯誤的 `tool_result`；在 `--allowedTools` 裡時不問；否則送 `can_use_tool` 等 `control_response`。允許時 Bash 以 `sh -c` 在 cwd 執行（用回覆的 `updatedInput`），`tool_result` 是 stdout+stderr，結束碼非零時 `is_error`；其他原生工具回 `ok`。拒絕時 `tool_result` `is_error`，內容是拒絕訊息。 |
//! | `{"expect_contains": "..."}` | 上一個 `tool_result` 的文字必須包含這段，不然 stderr 說明並 exit 3。 |
//! | `{"expect_is_error": true}` | 上一個 `tool_result` 的 `is_error` 必須相符，不然 exit 3。 |
//! | `{"end_turn": "..."}` | assistant 文字（非空時）＋ `result`（`subtype: "success"`、`result` 是這段文字），然後等下一則使用者訊息（繼續剩下的步驟）或 EOF（exit 0）。 |
//! | `{"error": "<code>", "text": "..."}` | assistant 訊息帶 `error: <code>`（`authentication_failed`、`rate_limit`、`billing_error`…）與文字（省略時依 code 用真的 CLI 的訊息，例如 `Invalid API key · Please run /login`），接著 `result` `is_error: true`，然後 exit 1。 |
//! | `{"rate_limit_rejected": true}` | `rate_limit_event`，`status: "rejected"`。 |
//! | `{"rate_limit_event": "<status>"}` | `rate_limit_event`，任意 `status`（例如 `allowed_warning`）。 |
//! | `{"result_error": "..."}` | 只輸出 `result` `is_error: true`（`subtype: "success"`、`result` 是這段文字），然後 exit 1。 |
//! | `{"sleep_ms": n}` | 等 n 毫秒（可被 interrupt）。 |
//! | `{"stdout_raw": "..."}` | 原樣輸出一行（非 JSON 的雜訊）。 |
//! | `{"stderr": "..."}` | 在 stderr 輸出一行。 |
//! | `{"exit": code}` | 立刻以這個結束碼結束（不輸出 `result`）。 |
//! | `{"hang": true, "ignore_sigterm": false}` | 不再回應 interrupt（連 control_response 都不回）、不理 EOF，永遠睡下去；`ignore_sigterm: true` 時也忽略 SIGTERM（只能 SIGKILL）。 |
//!
//! 劇本跑完卻沒有 `end_turn` 時輸出最後一個 `result`（文字是最後一段 assistant 文字）；之後每則
//! 使用者訊息都直接得到一個空的成功 `result`，EOF 時 exit 0。

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};

const VERSION: &str = "0.0.0-fake";
const NATIVE_TOOLS: &[&str] = &["Bash", "Read", "Edit", "Write", "Glob", "Grep"];
const MCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

type McpClient = RunningService<rmcp::RoleClient, ()>;

#[tokio::main]
async fn main() {
    let code = run().await;
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}

/// 解析後的參數。
#[derive(Default, Debug)]
struct Args {
    mcp_config: Option<String>,
    session_id: Option<String>,
    resume: Option<String>,
    model: Option<String>,
    allowed: Vec<String>,
    disallowed: Vec<String>,
}

fn parse_args(argv: &[String]) -> Args {
    const WITH_VALUE: &[&str] = &[
        "--mcp-config",
        "--session-id",
        "--resume",
        "--model",
        "--allowedTools",
        "--allowed-tools",
        "--disallowedTools",
        "--disallowed-tools",
        "--append-system-prompt",
        "--system-prompt",
        "--permission-mode",
        "--permission-prompt-tool",
        "--setting-sources",
        "--settings",
        "--input-format",
        "--output-format",
        "--max-turns",
        "--tools",
    ];
    let list = |v: &str| -> Vec<String> {
        v.split([',', ' '])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect()
    };
    let mut args = Args::default();
    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        i += 1;
        let (flag, value) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value.to_string())),
            _ => (arg.as_str(), None),
        };
        if !WITH_VALUE.contains(&flag) {
            continue;
        }
        let value = match value {
            Some(value) => value,
            None => match argv.get(i) {
                Some(value) => {
                    i += 1;
                    value.clone()
                }
                None => continue,
            },
        };
        match flag {
            "--mcp-config" => args.mcp_config = Some(value),
            "--session-id" => args.session_id = Some(value),
            "--resume" => args.resume = Some(value),
            "--model" => args.model = Some(value),
            "--allowedTools" | "--allowed-tools" => args.allowed.extend(list(&value)),
            "--disallowedTools" | "--disallowed-tools" => args.disallowed.extend(list(&value)),
            _ => {}
        }
    }
    args
}

/// 一輪劇本的一個步驟。
#[derive(Clone, Debug)]
enum Step {
    Text(String),
    Mcp { tool: String, args: Value },
    Native { tool: String, input: Value },
    ExpectContains(String),
    ExpectIsError(bool),
    EndTurn(String),
    Error { code: String, text: Option<String> },
    RateLimit(String),
    ResultError(String),
    Sleep(u64),
    StdoutRaw(String),
    Stderr(String),
    Exit(i32),
    Hang { ignore_sigterm: bool },
}

fn parse_step(v: &Value) -> Result<Step, String> {
    let s = |key: &str| v[key].as_str().map(str::to_string);
    if let Some(text) = s("text").filter(|_| v.get("error").is_none()) {
        return Ok(Step::Text(text));
    }
    if let Some(mcp) = v.get("mcp") {
        let tool = mcp["tool"].as_str().ok_or("mcp.tool 必須是字串")?;
        return Ok(Step::Mcp {
            tool: tool.to_string(),
            args: mcp.get("args").cloned().unwrap_or_else(|| json!({})),
        });
    }
    if let Some(native) = v.get("native") {
        let tool = native["tool"].as_str().ok_or("native.tool 必須是字串")?;
        return Ok(Step::Native {
            tool: tool.to_string(),
            input: native.get("input").cloned().unwrap_or_else(|| json!({})),
        });
    }
    if let Some(text) = s("expect_contains") {
        return Ok(Step::ExpectContains(text));
    }
    if let Some(flag) = v["expect_is_error"].as_bool() {
        return Ok(Step::ExpectIsError(flag));
    }
    if let Some(text) = s("end_turn") {
        return Ok(Step::EndTurn(text));
    }
    if let Some(code) = s("error") {
        return Ok(Step::Error {
            code,
            text: s("text"),
        });
    }
    if v["rate_limit_rejected"].as_bool() == Some(true) {
        return Ok(Step::RateLimit("rejected".into()));
    }
    if let Some(status) = s("rate_limit_event") {
        return Ok(Step::RateLimit(status));
    }
    if let Some(text) = s("result_error") {
        return Ok(Step::ResultError(text));
    }
    if let Some(ms) = v["sleep_ms"].as_u64() {
        return Ok(Step::Sleep(ms));
    }
    if let Some(text) = s("stdout_raw") {
        return Ok(Step::StdoutRaw(text));
    }
    if let Some(text) = s("stderr") {
        return Ok(Step::Stderr(text));
    }
    if let Some(code) = v["exit"].as_i64() {
        return Ok(Step::Exit(code as i32));
    }
    if v["hang"].as_bool() == Some(true) {
        return Ok(Step::Hang {
            ignore_sigterm: v["ignore_sigterm"].as_bool().unwrap_or(false),
        });
    }
    Err(format!("不認得的步驟：{v}"))
}

fn load_script(resume: bool) -> Result<Vec<Step>, String> {
    let Ok(path) = std::env::var("POTS_FAKE_CLAUDE_SCRIPT") else {
        return Ok(Vec::new());
    };
    let text = std::fs::read_to_string(&path).map_err(|e| format!("讀不到劇本 {path}：{e}"))?;
    let script: Value = serde_json::from_str(&text).map_err(|e| format!("劇本不是 JSON：{e}"))?;
    let steps = match &script {
        Value::Array(_) => &script,
        _ if resume && script.get("resume").is_some() => &script["resume"],
        _ => &script["new"],
    };
    match steps {
        Value::Null => Ok(Vec::new()),
        Value::Array(steps) => steps.iter().map(parse_step).collect(),
        _ => Err("劇本的步驟必須是陣列".into()),
    }
}

/// 跨 task 共用的輸出與狀態。
struct Shared {
    log: Option<Mutex<std::fs::File>>,
    /// 等 `can_use_tool` 回覆的請求。
    pending: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    /// 正在跑一輪：這時的 interrupt 才放棄步驟。
    in_turn: AtomicBool,
    /// `hang` 之後不再回應 interrupt。
    deaf: AtomicBool,
    interrupts: watch::Sender<u64>,
}

impl Shared {
    fn log(&self, record: Value) {
        if let Some(log) = &self.log {
            let mut file = log.lock().unwrap_or_else(|e| e.into_inner());
            let _ = writeln!(file, "{record}");
        }
    }
}

fn emit(value: &Value) {
    emit_raw(&value.to_string());
}

fn emit_raw(line: &str) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// 從 stdin 交給主流程的訊息。
enum Inbound {
    User(Value),
    Eof,
}

/// 讀 stdin 的執行緒（tokio 沒開 `io-std`，用阻塞讀取；處理本身都是同步的）。
fn read_stdin(shared: Arc<Shared>, inbound: mpsc::UnboundedSender<Inbound>) {
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
            shared.log(json!({"kind": "stdin", "frame": line}));
            continue;
        };
        shared.log(json!({"kind": "stdin", "frame": frame}));
        match frame["type"].as_str() {
            Some("user") => {
                let _ = inbound.send(Inbound::User(frame));
            }
            Some("control_request") => {
                let id = frame["request_id"].as_str().unwrap_or_default();
                match frame["request"]["subtype"].as_str() {
                    Some("initialize") => emit(&control_success(
                        id,
                        json!({"commands": [], "output_style": "default"}),
                    )),
                    Some("interrupt") => {
                        if shared.deaf.load(Ordering::SeqCst) {
                            continue;
                        }
                        emit(&control_success(id, json!({})));
                        if shared.in_turn.load(Ordering::SeqCst) {
                            shared.interrupts.send_modify(|n| *n += 1);
                        }
                    }
                    other => emit(&json!({
                        "type": "control_response",
                        "response": {
                            "subtype": "error",
                            "request_id": id,
                            "error": format!("Unsupported control request subtype: {other:?}"),
                        },
                    })),
                }
            }
            Some("control_response") => {
                let id = frame["response"]["request_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let waiter = shared
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                if let Some(waiter) = waiter {
                    let _ = waiter.send(frame["response"].clone());
                }
            }
            _ => {}
        }
    }
    let _ = inbound.send(Inbound::Eof);
}

fn control_success(id: &str, response: Value) -> Value {
    json!({
        "type": "control_response",
        "response": {"subtype": "success", "request_id": id, "response": response},
    })
}

/// 把工作目錄轉成 transcript 的目錄名：非 ASCII 英數字元都換成 `-`。
fn sanitize(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn config_dir() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(".claude"),
    }
}

fn new_uuid() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// 一個行程的劇本執行狀態。
struct Fake {
    shared: Arc<Shared>,
    args: Args,
    cwd: PathBuf,
    session_id: String,
    transcript: PathBuf,
    mcp: Option<McpClient>,
    mcp_tools: Vec<String>,
    mcp_connected: bool,
    cost: f64,
    /// 這輪輸出的 assistant 訊息數。
    turn_messages: u32,
    last_text: String,
    last_tool: Option<(String, bool)>,
    next_id: AtomicU64,
}

/// 一個步驟之後怎麼走。
enum Flow {
    Next,
    EndTurn(String),
    Exit(i32),
}

impl Fake {
    fn id(&self, prefix: &str) -> String {
        format!("{prefix}_{}", self.next_id.fetch_add(1, Ordering::SeqCst))
    }

    fn record(&self, value: &Value) {
        if let Some(dir) = self.transcript.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.transcript)
        {
            let _ = writeln!(file, "{value}");
        }
    }

    fn assistant(&mut self, content: Value, error: Option<&str>) {
        self.turn_messages += 1;
        let mut message = json!({
            "type": "assistant",
            "message": {
                "id": self.id("msg"),
                "type": "message",
                "role": "assistant",
                "model": self.args.model.as_deref().unwrap_or("fake-model"),
                "content": content,
                "stop_reason": null,
                "usage": {"input_tokens": 1, "output_tokens": 1},
            },
            "parent_tool_use_id": null,
            "session_id": self.session_id,
            "uuid": new_uuid(),
        });
        if let Some(error) = error {
            message["error"] = json!(error);
        }
        self.record(&message);
        emit(&message);
    }

    fn tool_result(
        &mut self,
        tool_use_id: &str,
        name: &str,
        input: &Value,
        text: &str,
        is_error: bool,
    ) {
        let message = json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": text,
                    "is_error": is_error,
                }],
            },
            "parent_tool_use_id": null,
            "session_id": self.session_id,
            "uuid": new_uuid(),
        });
        self.record(&message);
        emit(&message);
        self.shared.log(json!({
            "kind": "tool", "name": name, "input": input, "is_error": is_error, "output": text,
        }));
        self.last_tool = Some((text.to_string(), is_error));
    }

    fn result(&mut self, subtype: &str, is_error: bool, text: &str) {
        self.cost += 0.01;
        let result = json!({
            "type": "result",
            "subtype": subtype,
            "is_error": is_error,
            "duration_ms": 1,
            "duration_api_ms": 1,
            "num_turns": self.turn_messages.max(1),
            "result": text,
            "session_id": self.session_id,
            "total_cost_usd": (self.cost * 100.0).round() / 100.0,
            "usage": {"input_tokens": 1, "output_tokens": 1},
            "uuid": new_uuid(),
        });
        emit(&result);
        self.turn_messages = 0;
    }

    fn system_init(&self) {
        let mut tools: Vec<String> = NATIVE_TOOLS
            .iter()
            .filter(|t| !self.args.disallowed.iter().any(|d| d == *t))
            .map(|t| t.to_string())
            .collect();
        tools.extend(self.mcp_tools.iter().map(|t| format!("mcp__pots__{t}")));
        let mcp_servers = if self.args.mcp_config.is_some() {
            json!([{
                "name": "pots",
                "status": if self.mcp_connected { "connected" } else { "failed" },
            }])
        } else {
            json!([])
        };
        emit(&json!({
            "type": "system",
            "subtype": "init",
            "cwd": self.cwd.display().to_string(),
            "session_id": self.session_id,
            "tools": tools,
            "mcp_servers": mcp_servers,
            "model": self.args.model.as_deref().unwrap_or("fake-model"),
            "permissionMode": "default",
            "apiKeySource": "none",
            "claude_code_version": VERSION,
            "uuid": new_uuid(),
        }));
    }

    async fn connect_mcp(&mut self) {
        let Some(path) = &self.args.mcp_config else {
            return;
        };
        let connected = tokio::time::timeout(MCP_CONNECT_TIMEOUT, connect(Path::new(path))).await;
        match connected {
            Ok(Ok(client)) => {
                if let Ok(tools) = client.list_all_tools().await {
                    self.mcp_tools = tools.into_iter().map(|t| t.name.to_string()).collect();
                }
                self.mcp = Some(client);
                self.mcp_connected = true;
            }
            Ok(Err(e)) => eprintln!("pots-fake-claude：MCP 連線失敗：{e}"),
            Err(_) => eprintln!("pots-fake-claude：MCP 連線逾時"),
        }
    }

    fn allowed(&self, name: &str) -> bool {
        self.args.allowed.iter().any(|a| {
            a == name
                || (name.starts_with("mcp__")
                    && (name.starts_with(&format!("{a}__"))
                        || a.strip_suffix('*').is_some_and(|p| name.starts_with(p))))
        })
    }

    /// 送 `can_use_tool` 等回覆；回傳 `Ok(實際輸入)` 或 `Err(拒絕訊息)`。
    async fn ask_permission(
        &self,
        name: &str,
        input: &Value,
        tool_use_id: &str,
    ) -> Result<Value, String> {
        let id = self.id("perm");
        let (tx, rx) = oneshot::channel();
        self.shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone(), tx);
        emit(&json!({
            "type": "control_request",
            "request_id": id,
            "request": {
                "subtype": "can_use_tool",
                "tool_name": name,
                "input": input,
                "permission_suggestions": [],
                "tool_use_id": tool_use_id,
            },
        }));
        let Ok(response) = rx.await else {
            return Err("沒有收到權限回覆".into());
        };
        if response["subtype"] == "error" {
            return Err(format!("權限回覆錯誤：{}", response["error"]));
        }
        let decision = &response["response"];
        match decision["behavior"].as_str() {
            Some("allow") => Ok(match &decision["updatedInput"] {
                Value::Null => input.clone(),
                updated => updated.clone(),
            }),
            _ => Err(decision["message"]
                .as_str()
                .unwrap_or("Permission denied")
                .to_string()),
        }
    }

    async fn step(&mut self, step: Step) -> Flow {
        match step {
            Step::Text(text) => {
                self.last_text = text.clone();
                self.assistant(json!([{"type": "text", "text": text}]), None);
            }
            Step::Mcp { tool, args } => {
                let name = format!("mcp__pots__{tool}");
                let tool_use_id = self.id("toolu");
                self.assistant(
                    json!([{"type": "tool_use", "id": tool_use_id, "name": name, "input": args}]),
                    None,
                );
                let permitted = if self.allowed(&name) {
                    Ok(args.clone())
                } else {
                    self.ask_permission(&name, &args, &tool_use_id).await
                };
                let (text, is_error) = match permitted {
                    Ok(args) => self.call_mcp(&tool, args).await,
                    Err(message) => (message, true),
                };
                self.tool_result(&tool_use_id, &name, &args, &text, is_error);
            }
            Step::Native { tool, input } => {
                let tool_use_id = self.id("toolu");
                self.assistant(
                    json!([{"type": "tool_use", "id": tool_use_id, "name": tool, "input": input}]),
                    None,
                );
                let permitted = if self.args.disallowed.contains(&tool) {
                    Err(format!("No such tool available: {tool}"))
                } else if self.allowed(&tool) {
                    Ok(input.clone())
                } else {
                    self.ask_permission(&tool, &input, &tool_use_id).await
                };
                let (text, is_error) = match permitted {
                    Ok(input) if tool == "Bash" => self.bash(&input).await,
                    Ok(_) => ("ok".to_string(), false),
                    Err(message) => (message, true),
                };
                self.tool_result(&tool_use_id, &tool, &input, &text, is_error);
            }
            Step::ExpectContains(needle) => {
                let text = self
                    .last_tool
                    .as_ref()
                    .map(|(t, _)| t.as_str())
                    .unwrap_or_default();
                if !text.contains(&needle) {
                    eprintln!("pots-fake-claude：工具結果不含 {needle:?}：{text:?}");
                    return Flow::Exit(3);
                }
            }
            Step::ExpectIsError(expected) => {
                let actual = self.last_tool.as_ref().map(|(_, e)| *e);
                if actual != Some(expected) {
                    eprintln!("pots-fake-claude：工具結果 is_error 是 {actual:?}，預期 {expected}");
                    return Flow::Exit(3);
                }
            }
            Step::EndTurn(text) => return Flow::EndTurn(text),
            Step::Error { code, text } => {
                let text = text.unwrap_or_else(|| default_error_text(&code).to_string());
                self.assistant(json!([{"type": "text", "text": text}]), Some(&code));
                self.result("success", true, &text);
                return Flow::Exit(1);
            }
            Step::RateLimit(status) => emit(&json!({
                "type": "rate_limit_event",
                "rate_limit_info": {
                    "status": status,
                    "resetsAt": 1_900_000_000i64,
                    "rateLimitType": "five_hour",
                },
                "session_id": self.session_id,
                "uuid": new_uuid(),
            })),
            Step::ResultError(text) => {
                self.result("success", true, &text);
                return Flow::Exit(1);
            }
            Step::Sleep(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
            Step::StdoutRaw(text) => emit_raw(&text),
            Step::Stderr(text) => eprintln!("{text}"),
            Step::Exit(code) => return Flow::Exit(code),
            Step::Hang { ignore_sigterm } => {
                self.shared.deaf.store(true, Ordering::SeqCst);
                #[cfg(unix)]
                if ignore_sigterm {
                    // SAFETY：只是把 SIGTERM 設成忽略。
                    unsafe {
                        libc::signal(libc::SIGTERM, libc::SIG_IGN);
                    }
                }
                #[cfg(not(unix))]
                let _ = ignore_sigterm;
                std::future::pending::<()>().await;
            }
        }
        Flow::Next
    }

    async fn call_mcp(&mut self, tool: &str, args: Value) -> (String, bool) {
        let Some(client) = &self.mcp else {
            return (format!("MCP server pots 沒有連上，無法呼叫 {tool}"), true);
        };
        let mut params = CallToolRequestParams::new(tool.to_string());
        if let Value::Object(args) = args {
            params = params.with_arguments(args);
        }
        match client.call_tool(params).await {
            Ok(result) => {
                let text: Vec<String> = result
                    .content
                    .iter()
                    .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                    .collect();
                (text.join("\n"), result.is_error.unwrap_or(false))
            }
            Err(e) => (format!("MCP error: {e}"), true),
        }
    }

    async fn bash(&self, input: &Value) -> (String, bool) {
        let command = input["command"].as_str().unwrap_or_default();
        let output = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&self.cwd)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output()
            .await;
        match output {
            Ok(output) => {
                let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&output.stderr));
                (text, !output.status.success())
            }
            Err(e) => (format!("無法執行 sh：{e}"), true),
        }
    }
}

fn default_error_text(code: &str) -> &'static str {
    match code {
        "authentication_failed" => "Invalid API key · Please run /login",
        "rate_limit" => "Claude AI usage limit reached",
        "billing_error" => "Credit balance is too low",
        _ => "API Error",
    }
}

/// 連上 mcp-config 裡的 `pots` server（只支援 `type: "http"`）。
async fn connect(path: &Path) -> Result<McpClient, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("讀不到 {}：{e}", path.display()))?;
    let config: Value =
        serde_json::from_str(&text).map_err(|e| format!("mcp-config 不是 JSON：{e}"))?;
    let server = &config["mcpServers"]["pots"];
    if server.is_null() {
        return Err("mcp-config 沒有 pots server".into());
    }
    if !matches!(
        server["type"].as_str(),
        Some("http") | Some("streamable-http")
    ) {
        return Err(format!("不支援的 MCP transport：{}", server["type"]));
    }
    let url = server["url"].as_str().ok_or("pots server 沒有 url")?;
    let mut transport = StreamableHttpClientTransportConfig::with_uri(url.to_string());
    let mut headers = HashMap::new();
    for (name, value) in server["headers"].as_object().into_iter().flatten() {
        let Some(value) = value.as_str() else {
            continue;
        };
        if name.eq_ignore_ascii_case("authorization")
            && let Some(token) = value.strip_prefix("Bearer ")
        {
            transport = transport.auth_header(token.to_string());
            continue;
        }
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| format!("header 名稱不合法：{e}"))?;
        let value = reqwest::header::HeaderValue::from_str(value)
            .map_err(|e| format!("header 值不合法：{e}"))?;
        headers.insert(name, value);
    }
    let transport = StreamableHttpClientTransport::from_config(transport.custom_headers(headers));
    ().serve(transport).await.map_err(|e| e.to_string())
}

async fn run() -> i32 {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = parse_args(&argv);
    let cwd = std::env::current_dir().unwrap_or_default();
    let log = std::env::var_os("POTS_FAKE_CLAUDE_LOG").and_then(|path| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
            .map(Mutex::new)
    });
    let (interrupts, mut interrupted) = watch::channel(0u64);
    let shared = Arc::new(Shared {
        log,
        pending: Mutex::new(HashMap::new()),
        in_turn: AtomicBool::new(false),
        deaf: AtomicBool::new(false),
        interrupts,
    });
    shared.log(json!({"kind": "argv", "argv": argv, "cwd": cwd.display().to_string()}));

    let resume = args.resume.is_some();
    let session_id = args
        .resume
        .clone()
        .or_else(|| args.session_id.clone())
        .unwrap_or_else(new_uuid);
    let transcript = config_dir()
        .join("projects")
        .join(sanitize(&cwd.display().to_string()))
        .join(format!("{session_id}.jsonl"));
    let mut fake = Fake {
        shared: shared.clone(),
        args,
        cwd,
        session_id: session_id.clone(),
        transcript,
        mcp: None,
        mcp_tools: Vec::new(),
        mcp_connected: false,
        cost: 0.0,
        turn_messages: 0,
        last_text: String::new(),
        last_tool: None,
        next_id: AtomicU64::new(1),
    };

    let startup_error = if resume && !fake.transcript.exists() {
        Some(format!(
            "No conversation found with session ID: {session_id}"
        ))
    } else if !resume && fake.args.session_id.is_some() && fake.transcript.exists() {
        Some(format!("Session ID {session_id} is already in use."))
    } else {
        None
    };
    if let Some(text) = startup_error {
        eprintln!("{text}");
        fake.result("error_during_execution", true, &text);
        return 1;
    }

    let mut steps = match load_script(resume) {
        Ok(steps) => steps.into_iter(),
        Err(e) => {
            eprintln!("pots-fake-claude：{e}");
            return 2;
        }
    };

    let (inbound_tx, mut inbound) = mpsc::unbounded_channel();
    {
        let shared = shared.clone();
        std::thread::spawn(move || read_stdin(shared, inbound_tx));
    }

    let mut first = true;
    loop {
        // 閒置：等下一則使用者訊息。
        let message = match inbound.recv().await {
            Some(Inbound::User(message)) => message,
            Some(Inbound::Eof) | None => return 0,
        };
        fake.record(&json!({
            "type": "user",
            "sessionId": fake.session_id,
            "message": message["message"],
            "uuid": new_uuid(),
        }));
        if first {
            first = false;
            fake.connect_mcp().await;
        }
        fake.system_init();

        let base = *interrupted.borrow_and_update();
        shared.in_turn.store(true, Ordering::SeqCst);
        let mut ran_any = false;
        let flow = loop {
            let Some(step) = steps.next() else {
                break None;
            };
            ran_any = true;
            let is_end = matches!(step, Step::EndTurn(_));
            let flow = tokio::select! {
                flow = fake.step(step) => flow,
                _ = interrupted.wait_for(|n| *n > base) => {
                    // 放棄這輪剩下的步驟（含下一個 end_turn）。
                    if !is_end {
                        for step in steps.by_ref() {
                            if matches!(step, Step::EndTurn(_)) {
                                break;
                            }
                        }
                    }
                    break Some(Interrupted::Yes);
                }
            };
            match flow {
                Flow::Next => continue,
                Flow::EndTurn(text) => break Some(Interrupted::No(text)),
                Flow::Exit(code) => return code,
            }
        };
        shared.in_turn.store(false, Ordering::SeqCst);
        match flow {
            Some(Interrupted::Yes) => {
                fake.result("error_during_execution", true, "");
            }
            Some(Interrupted::No(text)) => {
                if !text.is_empty() {
                    fake.last_text = text.clone();
                    fake.assistant(json!([{"type": "text", "text": text}]), None);
                }
                fake.result("success", false, &text);
            }
            None => {
                // 劇本跑完了：這輪以最後一段文字收尾。
                let text = if ran_any {
                    fake.last_text.clone()
                } else {
                    String::new()
                };
                fake.result("success", false, &text);
            }
        }
    }
}

/// 一輪怎麼結束。
enum Interrupted {
    Yes,
    No(String),
}
