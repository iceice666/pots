//! Agent replies through the owner's own Claude Code CLI, on their Claude
//! subscription (`api = "claude-code"`).
//!
//! Claude Code runs the whole agent loop itself, so a reply is one
//! `claude -p` process:
//!
//! - an agent with a workspace runs it in its container (`docker exec`),
//!   with Claude Code's own Bash/Read/Edit/… tools and a token from
//!   `claude setup-token`, since the container can't see the host's login;
//! - an agent without one runs it on this machine with no built-in tools,
//!   logged in as the owner already is.
//!
//! pots's chat tools (ask_owner, pass, start_task, …) reach it through a
//! small MCP server ([`crate::mcp`]) that lives as long as the reply. Its
//! output (`--output-format stream-json`) streams into the reply as usual.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::channel::mpsc;
use serde_json::{Value, json};

use pots_llm::{Part, Role, ToolSpec, Turn, Usage};

use crate::agent::tools;
use crate::config::{WorkspaceMode, expand_home};
use crate::mcp;
use crate::reply::{Host, Outcome, Reply, blocking};
use crate::sandbox;
use crate::store;

/// Claude Code's built-in tools for each kind of workspace.
const WRITE_TOOLS: &str = "Bash,Read,Edit,Write,Glob,Grep";
const REVIEW_TOOLS: &str = "Bash,Read,Glob,Grep";
/// What `claude setup-token` prints goes in this variable.
const TOKEN_VAR: &str = "CLAUDE_CODE_OAUTH_TOKEN";
/// Leave out of a host `claude`'s environment every `ANTHROPIC_*` and
/// `CLAUDE*` variable Pots itself was started with (but where Claude Code
/// keeps its config): they'd send it somewhere other than the owner's
/// subscription (an API key wins over the login), or, when Pots was started
/// from inside a Claude Code session, make it act as part of that session
/// (renaming MCP tools, reusing its auth).
fn scrub_env(cmd: &mut Command) {
    for (name, _) in std::env::vars_os() {
        let n = name.to_string_lossy();
        if (n.starts_with("ANTHROPIC_") || n.starts_with("CLAUDE")) && n != "CLAUDE_CONFIG_DIR" {
            cmd.env_remove(&name);
        }
    }
}
/// How a container reaches this machine (Docker Desktop, OrbStack).
const DOCKER_HOST: &str = "host.docker.internal";
const LOGIN_HINT: &str = "Log in by running `claude` once in a terminal, or run `claude setup-token` and paste the \
                          token into this provider in Settings → Providers.";
const MAX_STDERR: usize = 4000;

enum Incoming {
    /// A line Claude Code printed.
    Line(String),
    /// It closed its output: it's done or gone.
    Eof,
    /// It called one of pots's tools.
    Call(mcp::Call),
}

/// Run `reply` through Claude Code to its end.
pub async fn run(reply: Reply, host: &mut impl Host) -> Result<Outcome, String> {
    let Reply { provider, max_steps, request, mut session, mut worker, conflicts } = reply;
    // Workspace tools are Claude Code's own; the rest are pots's. Its tool
    // list is fixed when it starts, so it gets every artifact tool now.
    let mut tools: Vec<ToolSpec> = request.tools.iter().filter(|t| !sandbox::is_tool(t.name)).cloned().collect();
    crate::artifact_tools::load_all(&mut tools);
    let (tx, mut rx) = mpsc::unbounded();
    let calls = tx.clone();
    let server = mcp::Server::start("127.0.0.1:0", &secret(), tools, move |call| {
        let _ = calls.unbounded_send(Incoming::Call(call));
    })
    .map_err(|e| format!("Couldn't start pots's tool server for Claude Code: {e}"))?;

    let session_id = uuid();
    let system = format!(
        "{}\n\nYour team-chat tools are MCP tools named `mcp__{name}__<tool>` (mcp__{name}__ask_owner, \
         mcp__{name}__pass, …); these instructions call them by the short name.",
        request.system,
        name = mcp::NAME
    );
    let mut args: Vec<String> = [
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--strict-mcp-config",
        "--permission-mode",
        "dontAsk",
    ]
    .map(String::from)
    .to_vec();
    args.extend(["--model".into(), request.model.clone(), "--max-turns".into(), max_steps.to_string()]);
    args.extend(["--session-id".into(), session_id.clone()]);

    let (mut cmd, program) = match session.take() {
        Some(mut s) => {
            let token = provider.key.clone().ok_or_else(|| {
                format!(
                    "Claude Code in a container can't use this computer's login. Run `claude setup-token` in a \
                     terminal and paste the token into the {} provider in Settings → Providers.",
                    provider.api.as_str()
                )
            })?;
            host.progress("", Some("Starting Claude Code in the workspace…".into()))?;
            let (ready, s) = blocking(move || (s.prepare_claude(), s)).await?;
            session = Some(s);
            ready?;
            let s = session.as_ref().expect("just set");
            let builtin = match s.env.mode {
                WorkspaceMode::Write => WRITE_TOOLS,
                WorkspaceMode::Review | WorkspaceMode::Scratch => REVIEW_TOOLS,
            };
            args.extend(["--tools".into(), builtin.into()]);
            args.extend(["--allowedTools".into(), format!("{builtin},mcp__{}", mcp::NAME)]);
            // Keep Claude Code's own prompt: it's how it knows its tools.
            args.extend(["--append-system-prompt".into(), system]);
            args.extend(["--mcp-config".into(), mcp_config(&server.url(DOCKER_HOST))]);
            let mut cmd = s.claude_command(&[TOKEN_VAR, "DISABLE_AUTOUPDATER"], &args);
            cmd.env(TOKEN_VAR, token).env("DISABLE_AUTOUPDATER", "1");
            (cmd, format!("claude in container {}", s.env.container))
        }
        None => {
            let claude = find_claude(provider.command.as_deref())?;
            args.extend(["--tools".into(), String::new()]);
            args.extend(["--allowedTools".into(), format!("mcp__{}", mcp::NAME)]);
            args.extend(["--system-prompt".into(), system]);
            // Not the owner's own settings, hooks and MCP servers.
            args.extend(["--setting-sources".into(), String::new()]);
            args.extend(["--mcp-config".into(), mcp_config(&server.url("127.0.0.1"))]);
            let dir = store::home().join("claude-code");
            std::fs::create_dir_all(&dir).map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;
            let mut cmd = Command::new(&claude);
            cmd.args(&args).current_dir(&dir);
            scrub_env(&mut cmd);
            if let Some(token) = &provider.key {
                cmd.env(TOKEN_VAR, token);
            }
            (cmd, claude.display().to_string())
        }
    };

    let child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Couldn't start Claude Code ({program}): {e}"))?;
    let mut running = Running {
        child: Some(child),
        container: session.as_ref().map(|s| s.env.clone()),
        tag: session_id,
        finished: false,
    };
    let child = running.child.as_mut().expect("just set");
    if let Some(mut stdin) = child.stdin.take() {
        let text = prompt(&request.turns);
        thread::spawn(move || {
            let _ = stdin.write_all(text.as_bytes());
        });
    }
    let stdout = child.stdout.take().expect("piped");
    let lines = tx.clone();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if lines.unbounded_send(Incoming::Line(line)).is_err() {
                return;
            }
        }
        let _ = lines.unbounded_send(Incoming::Eof);
    });
    let stderr = Arc::new(Mutex::new(String::new()));
    let stderr_reader = {
        let (mut pipe, buf) = (child.stderr.take().expect("piped"), stderr.clone());
        thread::spawn(move || {
            let mut text = String::new();
            let _ = pipe.read_to_string(&mut text);
            *buf.lock().unwrap() = text;
        })
    };
    drop(tx);

    let mut parser = Parser::new(max_steps);
    let mut shown = Pending::default();
    loop {
        // Take everything that has arrived, and update the host once.
        let item = match rx.try_recv() {
            Ok(item) => item,
            _ => {
                shown.flush(host)?;
                match rx.next().await {
                    Some(item) => item,
                    None => return Err("Claude Code's output closed unexpectedly.".into()),
                }
            }
        };
        match item {
            Incoming::Line(line) => {
                for out in parser.feed(&line) {
                    match out {
                        Out::Text(t) => shown.text.push_str(&t),
                        Out::Activity(a) => shown.activity = Some(a),
                        Out::Usage(u) => shown.usage = Some(u),
                        Out::Done(usage, result) => {
                            running.finished = true;
                            shown.usage = Some(usage);
                            shown.flush(host)?;
                            return result.map(|()| Outcome::Replied);
                        }
                    }
                }
            }
            Incoming::Eof => {
                shown.flush(host)?;
                let _ = blocking(move || stderr_reader.join()).await;
                let stderr = stderr.lock().unwrap().trim().to_string();
                let tail: String = stderr.chars().rev().take(MAX_STDERR).collect::<Vec<_>>().into_iter().rev().collect();
                return Err(match tail.is_empty() {
                    true => "Claude Code stopped without finishing its reply.".to_string(),
                    false => format!("Claude Code stopped without finishing its reply:\n\n{}", explain(&tail)),
                });
            }
            Incoming::Call(call) => {
                shown.flush(host)?;
                host.progress("", Some(tools::activity(&call.name, &call.input)))?;
                match call.name.as_str() {
                    tools::PASS => {
                        call.answer("Nothing will be posted.".into(), false);
                        return Ok(Outcome::Passed);
                    }
                    crate::worker::ASK_WORKER => {
                        let (text, is_error) = match worker.as_mut() {
                            Some(job) => job.ask(&call.input, host).await?,
                            None => ("There is no worker: add a [worker] to config.toml.".to_string(), true),
                        };
                        call.answer(text, is_error);
                    }
                    sandbox::CHECK_MERGE_CONFLICTS => {
                        let (text, is_error) = crate::reply::check_conflicts(conflicts.clone(), call.input.clone()).await?;
                        call.answer(text, is_error);
                    }
                    "ask_owner" | "request_merge" => {
                        let card = match (call.name.as_str(), session.take()) {
                            ("ask_owner", s) => {
                                session = s;
                                tools::choice_card(&call.input)
                            }
                            (_, Some(mut s)) => {
                                let (input, others) = (call.input.clone(), conflicts.clone());
                                let (card, s) = blocking(move || (s.merge_card(&input, others.as_ref()), s)).await?;
                                session = Some(s);
                                card
                            }
                            (_, None) => Err("You have no workspace, so there is no branch to merge.".into()),
                        };
                        match card {
                            // A card for the owner ends the reply: their answer is a new message.
                            Ok(card) => {
                                let shown = crate::reply::shown(&card);
                                host.card(card)?;
                                call.answer(shown, false);
                                return Ok(Outcome::Replied);
                            }
                            Err(e) => call.answer(e, true),
                        }
                    }
                    _ => {
                        let (text, is_error) = host.run_tool(&call.name, &call.input)?;
                        call.answer(text, is_error);
                    }
                }
            }
        }
    }
}

/// What has come in since the host was last updated.
#[derive(Default)]
struct Pending {
    text: String,
    activity: Option<String>,
    usage: Option<Usage>,
}

impl Pending {
    fn flush(&mut self, host: &mut impl Host) -> Result<(), String> {
        if !self.text.is_empty() || self.activity.is_some() {
            host.progress(&std::mem::take(&mut self.text), self.activity.take())?;
        }
        if let Some(usage) = self.usage.take() {
            host.usage(usage)?;
        }
        Ok(())
    }
}

/// The Claude Code process of a reply. Dropping it (the reply ended, or
/// was stopped) stops the process, in the container too.
struct Running {
    child: Option<Child>,
    container: Option<sandbox::Env>,
    /// On the process's command line: what to kill in the container.
    tag: String,
    /// It printed its result, so it's about to exit on its own.
    finished: bool,
}

impl Drop for Running {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else { return };
        let (container, tag, finished) = (self.container.take(), std::mem::take(&mut self.tag), self.finished);
        thread::spawn(move || {
            if finished {
                let deadline = Instant::now() + Duration::from_secs(10);
                while Instant::now() < deadline {
                    if let Ok(Some(_)) = child.try_wait() {
                        return;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
            let _ = child.kill();
            let _ = child.wait();
            if let Some(env) = container {
                sandbox::Session::new(env).kill_tagged(&tag);
            }
        });
    }
}

/// The `claude` CLI: the configured one, or the first found on PATH or
/// where its installers put it (GUI apps don't get the shell's PATH).
pub fn find_claude(configured: Option<&str>) -> Result<PathBuf, String> {
    if let Some(c) = configured.map(str::trim).filter(|c| !c.is_empty()) {
        return Ok(expand_home(c));
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let user = std::env::var("USER").unwrap_or_default();
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|d| d.join("claude"))
        .chain(
            [
                format!("{home}/.local/bin/claude"),
                format!("{home}/.claude/local/claude"),
                "/opt/homebrew/bin/claude".to_string(),
                "/usr/local/bin/claude".to_string(),
                format!("{home}/.npm-global/bin/claude"),
                format!("{home}/.bun/bin/claude"),
                // Nix: home-manager, nix profiles, nix-darwin.
                format!("/etc/profiles/per-user/{user}/bin/claude"),
                format!("{home}/.nix-profile/bin/claude"),
                "/run/current-system/sw/bin/claude".to_string(),
            ]
            .map(PathBuf::from),
        )
        .find(|p| p.is_file())
        .ok_or_else(|| {
            "Claude Code isn't installed here: there's no `claude` on PATH or in ~/.local/bin. Install it \
             (`curl -fsSL https://claude.ai/install.sh | bash`) and log in once, or set `command` for this \
             provider in config.toml."
                .to_string()
        })
}

fn mcp_config(url: &str) -> String {
    json!({"mcpServers": {mcp::NAME: {"type": "http", "url": url}}}).to_string()
}

/// The conversation, as the one message Claude Code is given.
fn prompt(turns: &[Turn]) -> String {
    let mut out = String::from(
        "The conversation so far, oldest first. Your own earlier messages start with \"You:\". \
         Write your reply to the latest message.\n\n",
    );
    for turn in turns {
        for part in &turn.parts {
            if let Part::Text(text) = part {
                if turn.role == Role::Assistant {
                    out.push_str("You: ");
                }
                out.push_str(text.trim());
                out.push_str("\n\n");
            }
        }
    }
    out
}

/// What one line of output means for the reply.
#[derive(Debug, PartialEq)]
enum Out {
    Text(String),
    Activity(String),
    /// Tokens used so far.
    Usage(Usage),
    /// The reply is over: tokens used, and how it ended.
    Done(Usage, Result<(), String>),
}

/// Reads `--output-format stream-json --include-partial-messages` lines.
struct Parser {
    max_turns: u32,
    /// Tokens of the model calls before the current one.
    earlier: Usage,
    current: Usage,
    wrote: bool,
    /// The next text starts a new paragraph: it's from a later model call.
    separate: bool,
}

impl Parser {
    fn new(max_turns: u32) -> Self {
        Self { max_turns, earlier: Usage::default(), current: Usage::default(), wrote: false, separate: false }
    }

    fn total(&self) -> Usage {
        let mut total = self.earlier;
        total.add(self.current);
        total
    }

    fn feed(&mut self, line: &str) -> Vec<Out> {
        let Ok(v) = serde_json::from_str::<Value>(line) else { return vec![] };
        // What subagents do isn't part of the reply.
        if !v["parent_tool_use_id"].is_null() {
            return vec![];
        }
        match v["type"].as_str().unwrap_or_default() {
            "system" if v["subtype"] == "init" => {
                let servers = v["mcp_servers"].as_array().into_iter().flatten();
                match servers.into_iter().find(|s| s["name"] == mcp::NAME) {
                    Some(s) if s["status"] == "failed" => vec![Out::Done(
                        Usage::default(),
                        Err("Claude Code couldn't connect to pots's tools (its MCP server `pots`).".into()),
                    )],
                    _ => vec![],
                }
            }
            "stream_event" => {
                let e = &v["event"];
                match e["type"].as_str().unwrap_or_default() {
                    "message_start" => {
                        self.earlier.add(self.current);
                        self.current = usage(&e["message"]["usage"]);
                        self.separate = self.wrote;
                        vec![]
                    }
                    "content_block_start" if e["content_block"]["type"] == "tool_use" => {
                        let name = e["content_block"]["name"].as_str().unwrap_or("a tool");
                        vec![Out::Activity(activity(name, &Value::Null))]
                    }
                    "content_block_delta" if e["delta"]["type"] == "text_delta" => {
                        let mut text = e["delta"]["text"].as_str().unwrap_or_default().to_string();
                        if text.is_empty() {
                            return vec![];
                        }
                        if self.separate {
                            self.separate = false;
                            text.insert_str(0, "\n\n");
                        }
                        self.wrote = true;
                        vec![Out::Text(text)]
                    }
                    "message_delta" => {
                        let u = &e["usage"];
                        if let Some(n) = u["output_tokens"].as_u64() {
                            self.current.output = n;
                        }
                        vec![Out::Usage(self.total())]
                    }
                    _ => vec![],
                }
            }
            // The whole message, after its stream: now tool inputs are known.
            "assistant" => v["message"]["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|b| b["type"] == "tool_use")
                .map(|b| Out::Activity(activity(b["name"].as_str().unwrap_or("a tool"), &b["input"])))
                .collect(),
            "result" => {
                let used = match v["usage"].is_object() {
                    true => usage(&v["usage"]),
                    false => self.total(),
                };
                let result = match v["subtype"].as_str().unwrap_or_default() {
                    "error_max_turns" => {
                        Err(format!("Stopped after {} turns (max_steps in config.toml).", self.max_turns))
                    }
                    "success" if v["is_error"] != true => Ok(()),
                    subtype => {
                        let why = v["result"].as_str().filter(|r| !r.trim().is_empty()).unwrap_or(subtype);
                        Err(explain(why))
                    }
                };
                vec![Out::Done(used, result)]
            }
            _ => vec![],
        }
    }
}

fn usage(u: &Value) -> Usage {
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    Usage {
        input: n("input_tokens"),
        output: n("output_tokens"),
        cache_write: n("cache_creation_input_tokens"),
        cache_read: n("cache_read_input_tokens"),
    }
}

/// An error from Claude Code, with what to do about a missing login.
fn explain(error: &str) -> String {
    let lower = error.to_lowercase();
    let login = ["not logged in", "/login", "oauth", "authentication", "invalid api key"];
    match login.iter().any(|k| lower.contains(k)) {
        true => format!("Claude Code: {}\n\n{LOGIN_HINT}", error.trim()),
        false => format!("Claude Code: {}", error.trim()),
    }
}

/// What the UI shows while Claude Code uses a tool.
fn activity(name: &str, input: &Value) -> String {
    if let Some(tool) = name.strip_prefix(&format!("mcp__{}__", mcp::NAME)) {
        return tools::activity(tool, input);
    }
    let path = input["file_path"].as_str().unwrap_or("a file");
    match name {
        "Bash" => sandbox::activity("bash", input).unwrap_or_default(),
        "Read" => format!("Reading {path}…"),
        "Write" => format!("Writing {path}…"),
        "Edit" | "MultiEdit" => format!("Editing {path}…"),
        "Grep" => match input["pattern"].as_str() {
            Some(p) => format!("Searching for `{}`…", p.chars().take(40).collect::<String>()),
            None => "Searching…".to_string(),
        },
        "Glob" => "Looking for files…".to_string(),
        _ => format!("Using {name}…"),
    }
}

fn random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    h.write_u128(now);
    h.write_u32(std::process::id());
    h.finish()
}

/// A hard-to-guess path segment for the tool server.
fn secret() -> String {
    format!("{:016x}{:016x}", random_u64(), random_u64())
}

/// A random (version 4) UUID, as Claude Code wants for `--session-id`.
fn uuid() -> String {
    let a = (random_u64() & !0xF000) | 0x4000;
    let b = (random_u64() & !(0b11 << 62)) | (0b10 << 62);
    format!("{:08x}-{:04x}-{:04x}-{:04x}-{:012x}", a >> 32, (a >> 16) & 0xffff, a & 0xffff, b >> 48, b & 0xffff_ffff_ffff)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;

    use pots_llm::{Api, Endpoint, Provider, Request};

    use super::*;
    use crate::data::Card;
    use crate::reply::tests::Recorder;

    #[test]
    fn uuids_are_version_4() {
        let id = uuid();
        assert_eq!(id.len(), 36, "{id}");
        assert_eq!(&id[14..15], "4", "{id}");
        assert!("89ab".contains(&id[19..20]), "{id}");
        assert_ne!(uuid(), uuid());
    }

    #[test]
    fn parses_a_streamed_reply() {
        let mut p = Parser::new(8);
        let lines = [
            r#"{"type":"system","subtype":"init","mcp_servers":[{"name":"pots","status":"connected"}],"parent_tool_use_id":null}"#,
            r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":5}}}}"#,
            r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Let me look."}}}"#,
            r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_start","content_block":{"type":"tool_use","name":"Bash"}}}"#,
            r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"message_delta","usage":{"output_tokens":7}}}"#,
            r#"{"type":"assistant","parent_tool_use_id":null,"message":{"content":[{"type":"text","text":"Let me look."},{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}]}}"#,
            r#"{"type":"stream_event","parent_tool_use_id":"toolu_1","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"subagent"}}}"#,
            r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"message_start","message":{"usage":{"input_tokens":20}}}}"#,
            r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"All green."}}}"#,
            r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"message_delta","usage":{"output_tokens":3}}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"All green.","usage":{"input_tokens":30,"output_tokens":10,"cache_read_input_tokens":5}}"#,
        ];
        let out: Vec<Out> = lines.iter().flat_map(|l| p.feed(l)).collect();
        assert_eq!(
            out,
            vec![
                Out::Text("Let me look.".into()),
                Out::Activity("Running a command…".into()),
                Out::Usage(Usage { input: 10, output: 7, cache_write: 0, cache_read: 5 }),
                Out::Activity("Running `cargo test`".into()),
                Out::Text("\n\nAll green.".into()),
                Out::Usage(Usage { input: 30, output: 10, cache_write: 0, cache_read: 5 }),
                Out::Done(Usage { input: 30, output: 10, cache_write: 0, cache_read: 5 }, Ok(())),
            ]
        );
    }

    #[test]
    fn explains_how_it_ended() {
        let done = |line: &str| match Parser::new(8).feed(line).pop() {
            Some(Out::Done(_, result)) => result,
            other => panic!("{other:?}"),
        };
        let err = done(r#"{"type":"result","subtype":"success","is_error":true,"result":"Not logged in · Please run /login"}"#);
        assert!(err.unwrap_err().contains("claude setup-token"));
        let err = done(r#"{"type":"result","subtype":"error_max_turns","is_error":true}"#);
        assert_eq!(err.unwrap_err(), "Stopped after 8 turns (max_steps in config.toml).");
        let err = done(r#"{"type":"system","subtype":"init","mcp_servers":[{"name":"pots","status":"failed"}]}"#);
        assert!(err.unwrap_err().contains("couldn't connect"));
    }

    #[test]
    fn names_what_tools_do() {
        assert_eq!(activity("mcp__pots__list_channels", &json!({})), "Looking at channels…");
        assert_eq!(activity("Edit", &json!({"file_path": "src/main.rs"})), "Editing src/main.rs…");
        assert_eq!(activity("Grep", &json!({"pattern": "fn main"})), "Searching for `fn main`…");
    }

    #[test]
    fn the_prompt_is_the_conversation() {
        let turns = vec![Turn::user("Owner: hi"), Turn::assistant("Hello!"), Turn::user("Owner: status?")];
        let p = prompt(&turns);
        assert!(p.ends_with("Owner: hi\n\nYou: Hello!\n\nOwner: status?\n\n"), "{p}");
    }

    /// A stand-in `claude`: checks its arguments, calls pots's tools over
    /// MCP with curl, and prints stream-json, as told by `script`.
    fn fake_claude(dir: &std::path::Path, script: &str) -> String {
        let path = dir.join("claude");
        let body = format!(
            r#"#!/bin/sh
while [ $# -gt 0 ]; do
  case "$1" in
    --mcp-config) cfg="$2"; shift;;
    --tools) tools="$2"; shift;;
    --model) model="$2"; shift;;
  esac
  shift
done
url=$(printf '%s' "$cfg" | sed 's/.*"url":"\([^"]*\)".*/\1/')
prompt=$(cat)
call() {{
  curl -s -X POST -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
    -d "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{{\"name\":\"$1\",\"arguments\":$2}}}}" "$url"
}}
say() {{
  printf '{{"type":"stream_event","parent_tool_use_id":null,"event":{{"type":"content_block_delta","delta":{{"type":"text_delta","text":"%s"}}}}}}\n' "$1"
}}
done_ok() {{
  echo '{{"type":"result","subtype":"success","is_error":false,"usage":{{"input_tokens":12,"output_tokens":3}}}}'
}}
echo '{{"type":"system","subtype":"init","mcp_servers":[{{"name":"pots","status":"connected"}}]}}'
{script}
"#
        );
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.display().to_string()
    }

    fn reply(command: String) -> Reply {
        let provider = Provider {
            api: Api::ClaudeCode,
            base_url: String::new(),
            key: None,
            headers: BTreeMap::new(),
            prompt_cache: false,
            stream_usage: false,
            endpoint: Endpoint::Auto,
            command: Some(command),
        };
        let mut tools = tools::specs();
        tools.push(tools::start_task_spec());
        let request = Request {
            model: "opus".into(),
            system: "You are PM.".into(),
            turns: vec![Turn::user("Owner: what's going on?")],
            tools,
            max_tokens: None,
            extra: None,
        };
        Reply { provider, max_steps: 8, request, session: None, worker: None, conflicts: None }
    }

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pots-cc-{name}-{}", secret()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn runs_pots_tools_over_mcp_and_streams_the_answer() {
        let dir = tempdir("tools");
        let claude = fake_claude(
            &dir,
            r#"[ "$tools" = "" ] || { echo "built-in tools: $tools" >&2; exit 2; }
case "$prompt" in *"Owner: what's going on?"*) ;; *) echo "no conversation in the prompt" >&2; exit 2;; esac
r=$(call list_channels '{}')
case "$r" in *general*) say "Channels: general";; *) say "no channels: $r";; esac
done_ok"#,
        );
        let mut host = Recorder::new();
        let outcome = futures::executor::block_on(reply(claude).run(&mut host));
        assert_eq!(outcome, Ok(Outcome::Replied), "{}", host.text);
        assert_eq!(host.text, "Channels: general");
        assert!(host.activities.contains(&"Looking at channels…".to_string()), "{:?}", host.activities);
        assert_eq!(host.usage, Usage { input: 12, output: 3, ..Usage::default() });
    }

    #[test]
    fn claude_code_gets_every_artifact_tool_up_front() {
        let dir = tempdir("artifact-tools");
        let claude = fake_claude(
            &dir,
            r#"r=$(curl -s -X POST -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' "$url")
case "$r" in *use_artifact_tools*) say "loader offered";; *create_whiteboard*) say "all artifact tools";; *) say "missing: $r";; esac
done_ok"#,
        );
        let mut host = Recorder::new();
        futures::executor::block_on(reply(claude).run(&mut host)).unwrap();
        assert_eq!(host.text, "all artifact tools");
    }

    #[test]
    fn start_task_changes_the_workspace() {
        let dir = tempdir("task");
        let claude = fake_claude(
            &dir,
            r#"r=$(call start_task '{"channel":"engineering","title":"Login page","brief":"Add one.","assignee":"Leo"}')
case "$r" in *"has the brief"*) say "Started.";; *) say "failed: $r";; esac
done_ok"#,
        );
        let mut host = Recorder::new();
        let threads = host.ws.threads.len();
        futures::executor::block_on(reply(claude).run(&mut host)).unwrap();
        assert_eq!(host.text, "Started.");
        assert_eq!(host.ws.threads.len(), threads + 1);
    }

    #[test]
    fn a_card_ends_the_reply_and_pass_posts_nothing() {
        let dir = tempdir("card");
        let claude = fake_claude(
            &dir,
            r#"say "Your call."
call ask_owner '{"question":"Ship it?","options":["Yes","Not yet"]}' >/dev/null
sleep 5
say "unreached"
done_ok"#,
        );
        let mut host = Recorder::new();
        let outcome = futures::executor::block_on(reply(claude).run(&mut host));
        assert_eq!(outcome, Ok(Outcome::Replied));
        assert_eq!(host.text, "Your call.");
        assert!(matches!(&host.cards[..], [Card::Choice { question, .. }] if question == "Ship it?"), "{:?}", host.cards);

        let claude = fake_claude(&dir, r#"call pass '{"reason":"not mine"}' >/dev/null; sleep 5; done_ok"#);
        let mut host = Recorder::new();
        assert_eq!(futures::executor::block_on(reply(claude).run(&mut host)), Ok(Outcome::Passed));
    }

    #[test]
    fn reports_a_crash_with_its_stderr() {
        let dir = tempdir("crash");
        let claude = fake_claude(&dir, r#"echo "boom: Not logged in" >&2; exit 1"#);
        let mut host = Recorder::new();
        let err = futures::executor::block_on(reply(claude).run(&mut host)).unwrap_err();
        assert!(err.contains("boom: Not logged in") && err.contains("setup-token"), "{err}");
    }

    #[test]
    fn says_when_claude_is_missing() {
        let mut host = Recorder::new();
        let err = futures::executor::block_on(reply("/nonexistent/claude".into()).run(&mut host)).unwrap_err();
        assert!(err.contains("Couldn't start Claude Code"), "{err}");
    }
}

/// Checks against a real Claude Code (run them one at a time):
///
/// - `real_claude_sees_pots_tools` needs no login: Claude Code connects to
///   its MCP servers first. Set `POTS_CLAUDE_CONTAINER=<running container
///   with claude>` to try it from a container.
/// - `real_reply_*` use the owner's login and a little of their plan, on
///   the cheapest model.
///
/// `cargo test -p pots-agent real_ -- --ignored --test-threads=1`; the
/// `claude` used is the one Pots would find, or `POTS_CLAUDE`.
#[cfg(test)]
mod real {
    use serde_json::Value;

    use super::*;
    use crate::agent::Agents;
    use crate::config::Config;
    use crate::data::{self, Card, Conv};
    use crate::reply::tests::Recorder;

    fn claude() -> PathBuf {
        find_claude(std::env::var("POTS_CLAUDE").ok().as_deref()).unwrap()
    }

    #[test]
    #[ignore = "needs a real claude"]
    fn real_claude_sees_pots_tools() {
        let container = std::env::var("POTS_CLAUDE_CONTAINER").ok();
        let server = mcp::Server::start("127.0.0.1:0", &secret(), tools::specs(), |call| call.answer("ok".into(), false))
            .unwrap();
        let host = if container.is_some() { DOCKER_HOST } else { "127.0.0.1" };
        let args = [
            "-p", "--output-format", "stream-json", "--verbose", "--strict-mcp-config", "--tools", "",
            "--setting-sources", "", "--max-turns", "1", "--mcp-config", &mcp_config(&server.url(host)),
        ];
        let mut cmd = match &container {
            Some(c) => {
                let mut cmd = Command::new(sandbox::find_docker(None));
                cmd.args(["exec", "-i", c, "sh", "-c", "export PATH=\"$HOME/.local/bin:$PATH\"; exec claude \"$@\"", "claude"]);
                cmd
            }
            None => Command::new(claude()),
        };
        scrub_env(&mut cmd);
        let out = cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        out.stdin.as_ref().unwrap().write_all(b"hi").unwrap();
        let out = out.wait_with_output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let init: Value = text
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find(|v| v["subtype"] == "init")
            .unwrap_or_else(|| panic!("no init line:\n{text}"));
        let pots = init["mcp_servers"].as_array().unwrap().iter().find(|s| s["name"] == "pots").unwrap();
        assert_eq!(pots["status"], "connected", "{init}");
        let tools: Vec<&str> = init["tools"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        for name in ["mcp__pots__pass", "mcp__pots__ask_owner", "mcp__pots__read_channel"] {
            assert!(tools.contains(&name), "{tools:?}");
        }
    }

    /// A [`Recorder`] that also notes which tools reached the host.
    struct Counting {
        rec: Recorder,
        ran: Vec<String>,
    }

    impl Host for Counting {
        fn progress(&mut self, text: &str, activity: Option<String>) -> Result<(), String> {
            self.rec.progress(text, activity)
        }
        fn usage(&mut self, total: Usage) -> Result<(), String> {
            self.rec.usage(total)
        }
        fn card(&mut self, card: Card) -> Result<(), String> {
            self.rec.card(card)
        }
        fn run_tool(&mut self, name: &str, input: &Value) -> Result<(String, bool), String> {
            self.ran.push(name.to_string());
            self.rec.run_tool(name, input)
        }
    }

    /// PM, on a `claude-code` provider, answering `message` from the owner
    /// in its DM, through the same wiring the app uses.
    fn reply_to(message: &str) -> (Result<Outcome, String>, Counting) {
        let command = claude().display().to_string();
        let text = format!(
            "[providers.claude]\napi = \"claude-code\"\ncommand = {command:?}\n\
             [[agents]]\nname = \"PM\"\nprovider = \"claude\"\nmodel = \"haiku\"\nmax_steps = 4\n\
             system_prompt = \"You are PM. Follow the owner's instructions exactly and keep replies to one line.\"\n"
        );
        let config = Config::parse(&text).unwrap();
        let mut ws = data::seed();
        let agents = Agents::from_config(&mut ws, &config);
        assert_eq!(agents.price(0), None, "no prices on a subscription");
        let pm = agents.list[0].user;
        let dm = ws.channels.iter().position(|c| c.dm == Some(pm)).unwrap();
        let conv = Conv::Channel(dm);
        ws.post(conv, ws.me, message);
        let ix = ws.post(conv, pm, "");
        ws.message_mut(conv, ix).pending = true;
        let reply = agents.reply(0, &ws, conv, ix);
        let mut host = Counting { rec: Recorder::new(), ran: vec![] };
        host.rec.ws = ws;
        let outcome = futures::executor::block_on(reply.run(&mut host));
        (outcome, host)
    }

    /// Whether any `claude` Pots started is still running.
    fn leftover_claude() -> String {
        // Wait for the reaper thread to stop it.
        thread::sleep(Duration::from_secs(2));
        let out = Command::new("ps").args(["-axo", "command"]).output().unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| l.contains("--allowedTools mcp__pots") && !l.contains("ps -axo"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    #[ignore = "uses the owner's Claude login"]
    fn real_reply_runs_a_pots_tool() {
        let (outcome, host) = reply_to(
            "Call the list_channels tool, then reply with just the channel names it lists, comma-separated.",
        );
        let text = &host.rec.text;
        assert_eq!(outcome, Ok(Outcome::Replied), "{text}");
        // Only the tool's result has these channel names.
        assert!(host.ran.contains(&"list_channels".to_string()), "tools that ran: {:?}", host.ran);
        let lower = text.to_lowercase();
        assert!(lower.contains("design") && lower.contains("random"), "{text}");
        assert!(host.rec.activities.contains(&"Looking at channels…".to_string()), "{:?}", host.rec.activities);
        assert!(host.rec.usage.output > 0, "{:?}", host.rec.usage);
        eprintln!("reply: {text}\nusage: {:?}", host.rec.usage);
        assert_eq!(leftover_claude(), "");
    }

    #[test]
    #[ignore = "uses the owner's Claude login"]
    fn real_reply_ends_at_a_card_or_a_pass() {
        let (outcome, host) = reply_to(
            "Use the ask_owner tool to ask me \"Ship it?\" with the options Yes and No. Don't do anything else.",
        );
        assert_eq!(outcome, Ok(Outcome::Replied), "{}", host.rec.text);
        assert!(matches!(&host.rec.cards[..], [Card::Choice { options, .. }] if options.len() == 2), "{:?}", host.rec.cards);
        assert_eq!(leftover_claude(), "", "the card ended the reply, so its claude was stopped");

        let (outcome, host) = reply_to("This message isn't for you: call the pass tool and nothing else.");
        assert_eq!(outcome, Ok(Outcome::Passed), "{}", host.rec.text);
        assert_eq!(leftover_claude(), "");
    }

    /// Engineer, in a container of `rust:latest` on a throwaway repository,
    /// with the token of the Claude Code provider in ~/.pots (Settings →
    /// Providers). The container is removed afterwards.
    #[test]
    #[ignore = "uses the owner's Claude token and docker"]
    fn real_reply_in_a_container() {
        let config = store::load_config().unwrap();
        let pc = config
            .providers
            .values()
            .find(|p| p.api == pots_llm::Api::ClaudeCode && p.key().is_some())
            .expect("add a Claude Code provider with a token in Settings → Providers");
        let dir = std::env::temp_dir().join(format!("pots-cc-container-{}", &secret()[..8]));
        let repo = dir.join("project");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |dir: &std::path::Path, args: &[&str]| {
            let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        std::fs::write(repo.join("README.md"), "A test project.\n").unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["add", "."]);
        git(&repo, &["-c", "user.name=Owner", "-c", "user.email=owner@example.com", "commit", "-qm", "Start"]);

        let env = sandbox::Env::new(
            WorkspaceMode::Write,
            sandbox::find_docker(None),
            "rust:latest".into(),
            repo,
            &dir.join("tasks"),
            "t0-cc-test",
            "Engineer",
        );
        let (container, writer, branch) = (env.container.clone(), env.writer_dir(), env.branch.clone());
        let mut tools = tools::specs();
        tools.extend(sandbox::specs(WorkspaceMode::Write).into_iter().filter(|t| !sandbox::is_tool(t.name)));
        let request = pots_llm::Request {
            model: "haiku".into(),
            system: format!("You are Engineer.\n\n{}", env.describe_claude_code()),
            turns: vec![Turn::user(
                "Owner: Create hello.txt containing exactly `hi from claude`, commit it with the message \
                 \"Add hello\", then call request_merge with a one-line summary.",
            )],
            tools,
            max_tokens: None,
            extra: None,
        };
        let reply = Reply {
            provider: pots_llm::Provider::new(pc),
            max_steps: 15,
            request,
            session: Some(sandbox::Session::new(env)),
            worker: None,
            conflicts: None,
        };
        let mut host = Recorder::new();
        let outcome = futures::executor::block_on(reply.run(&mut host));
        let log = git(&writer, &["log", "--format=%s", &branch]);
        let file = std::fs::read_to_string(writer.join("hello.txt")).unwrap_or_default();
        thread::sleep(Duration::from_secs(2));
        let _ = Command::new(sandbox::find_docker(None)).args(["rm", "-f", &container]).output();

        eprintln!("reply: {}\nactivities: {:?}\nusage: {:?}", host.text, host.activities, host.usage);
        assert_eq!(outcome, Ok(Outcome::Replied), "{}", host.text);
        assert_eq!(file.trim(), "hi from claude");
        assert!(log.lines().any(|l| l == "Add hello"), "{log}");
        assert!(
            matches!(&host.cards[..], [Card::Merge { commits, .. }] if commits.iter().any(|c| c.ends_with("Add hello"))),
            "{:?}",
            host.cards
        );
    }
}
