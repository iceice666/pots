//! adapter 的單元測試：純函式，以及對著假的 CLI（`pots-fake-claude`）跑完整流程。

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Instant as StdInstant;

use tokio::io::AsyncWriteExt;

use super::*;

// ---- 純函式 ----

fn sample_run(session: SessionMode) -> ClaudeRun {
    ClaudeRun {
        command: tokio::process::Command::new("claude"),
        session,
        prompt: "做事".into(),
        append_system_prompt: "規則".into(),
        mcp_config_path: "/run/pots/mcp.json".into(),
        setting_sources: "user".into(),
        settings_path: Some("/run/pots/settings.json".into()),
        model: Some("sonnet".into()),
        allowed_tools: vec!["mcp__pots__complete_task".into(), "mcp__pots__note".into()],
        disallowed_tools: vec!["WebSearch".into()],
        policy: Arc::new(|_: &str, _: &Value| Permission::Allow),
        nudge: None,
        finished: Arc::new(AtomicBool::new(false)),
        stop: Arc::new(Notify::new()),
        grace: Duration::from_secs(1),
        hard_stop: None,
    }
}

#[test]
fn args_new_session() {
    let args = cli_args(&sample_run(SessionMode::New("-abc".into())));
    let expected: Vec<String> = [
        "--print",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--permission-prompt-tool",
        "stdio",
        "--permission-mode",
        "default",
        "--strict-mcp-config",
        "--mcp-config",
        "/run/pots/mcp.json",
        "--setting-sources=user",
        "--settings",
        "/run/pots/settings.json",
        "--append-system-prompt",
        "規則",
        "--model",
        "sonnet",
        "--allowedTools",
        "mcp__pots__complete_task,mcp__pots__note",
        "--disallowedTools",
        "WebSearch",
        "--session-id=-abc",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(args, expected);
}

#[test]
fn args_resume_omits_empty_options() {
    let mut run = sample_run(SessionMode::Resume("s1".into()));
    run.model = None;
    run.allowed_tools.clear();
    run.disallowed_tools.clear();
    run.append_system_prompt.clear();
    let args = cli_args(&run);
    assert_eq!(args.last().map(String::as_str), Some("--resume=s1"));
    for flag in [
        "--model",
        "--allowedTools",
        "--disallowedTools",
        "--append-system-prompt",
    ] {
        assert!(!args.iter().any(|a| a == flag), "{flag} 不該出現");
    }
}

#[test]
fn parse_line_skips_noise() {
    assert_eq!(parse_line(b""), None);
    assert_eq!(parse_line(b"  \r"), None);
    assert_eq!(parse_line(b"npm WARN something"), None);
    assert_eq!(parse_line(b"{broken"), None);
    assert_eq!(parse_line(b"42"), None);
    assert_eq!(
        parse_line(b" {\"type\":\"x\"}\r"),
        Some(json!({"type": "x"}))
    );
}

#[tokio::test]
async fn read_line_caps_long_lines() {
    let data: &[u8] = b"short\nthis-line-is-too-long\nok\npartial";
    let mut reader = BufReader::with_capacity(4, data);
    let mut buf = Vec::new();
    let mut got = Vec::new();
    while let Some(line) = read_line_capped(&mut reader, &mut buf, 10).await.unwrap() {
        got.push(line.map(|()| String::from_utf8(buf.clone()).unwrap()));
    }
    assert_eq!(
        got,
        vec![
            Ok("short".to_string()),
            Err(21),
            Ok("ok".to_string()),
            Ok("partial".to_string()),
        ]
    );
}

#[test]
fn classify_prefers_structured_fields() {
    assert!(matches!(
        classify(Some("authentication_failed"), None, "whatever"),
        ClaudeFailure::NeedsLogin(_)
    ));
    assert!(matches!(
        classify(Some("rate_limit"), None, ""),
        ClaudeFailure::UsageLimit(m) if m == "rate_limit"
    ));
    assert!(matches!(
        classify(Some("billing_error"), None, "x"),
        ClaudeFailure::UsageLimit(_)
    ));
    assert!(matches!(
        classify(None, Some(401), "x"),
        ClaudeFailure::NeedsLogin(_)
    ));
    assert!(matches!(
        classify(None, Some(429), "x"),
        ClaudeFailure::UsageLimit(_)
    ));
    assert!(matches!(
        classify(Some("server_error"), None, "API Error: 500"),
        ClaudeFailure::Cli(_)
    ));
}

#[test]
fn classify_falls_back_to_text() {
    for text in [
        "Invalid API key · Please run /login",
        "OAuth token has expired. Please obtain a new token",
    ] {
        assert!(
            matches!(classify(None, None, text), ClaudeFailure::NeedsLogin(_)),
            "{text}"
        );
    }
    assert!(matches!(
        classify(None, None, "Claude AI usage limit reached|1700000000"),
        ClaudeFailure::UsageLimit(_)
    ));
    assert_eq!(
        classify(None, None, "No conversation found with session ID: s1"),
        ClaudeFailure::Cli("No conversation found with session ID: s1".into())
    );
}

#[test]
fn result_failures() {
    assert_eq!(
        result_failure(&json!({"type": "result", "subtype": "success", "is_error": false})),
        None
    );
    assert!(matches!(
        result_failure(&json!({"subtype": "success", "is_error": false,
            "result": "Claude AI usage limit reached|1"})),
        Some(ClaudeFailure::UsageLimit(_))
    ));
    assert_eq!(
        result_failure(&json!({"subtype": "error_max_turns", "is_error": true})),
        Some(ClaudeFailure::Cli("error_max_turns".into()))
    );
    assert_eq!(
        result_failure(
            &json!({"subtype": "error_during_execution", "is_error": true,
            "errors": ["boom", "bang"]})
        ),
        Some(ClaudeFailure::Cli("boom；bang".into()))
    );
    assert!(matches!(
        result_failure(&json!({"subtype": "success", "is_error": true,
            "result": "API Error", "api_error_status": 401})),
        Some(ClaudeFailure::NeedsLogin(_))
    ));
}

#[test]
fn tool_summaries() {
    assert_eq!(
        tool_summary("Bash", &json!({"command": "cargo test\n--all"})),
        "cargo test …"
    );
    let long = "x".repeat(300);
    let summary = tool_summary("Bash", &json!({ "command": long }));
    assert_eq!(summary.chars().count(), SUMMARY_CHARS + 1);
    assert_eq!(
        tool_summary("Edit", &json!({"file_path": "/w/src/main.rs"})),
        "/w/src/main.rs"
    );
    assert_eq!(
        tool_summary("Grep", &json!({"pattern": "fn main", "path": "src"})),
        "fn main（src）"
    );
    assert_eq!(
        tool_summary("mcp__pots__complete_task", &json!({})),
        "平台工具 complete_task"
    );
    assert_eq!(
        tool_summary("mcp__github__create_pr", &json!({})),
        "MCP 工具 github/create_pr"
    );
    assert_eq!(tool_summary("Bash", &json!({})), "Bash");
    assert_eq!(tool_summary("Mystery", &json!({"a": 1})), "Mystery");
}

#[test]
fn state_maps_records() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut state = State::new(tx);
    let init = json!({"type": "system", "subtype": "init", "session_id": "s1",
        "claude_code_version": "2.1.0", "tools": [], "extra": {"new": true}});
    assert_eq!(state.handle(&init), Handled::Nothing);
    // 第二個 init 不重複發 Started。
    assert_eq!(state.handle(&init), Handled::Nothing);
    let assistant = json!({"type": "assistant", "parent_tool_use_id": null, "message": {"content": [
        {"type": "thinking", "thinking": "嗯"},
        {"type": "text", "text": "開始"},
        {"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "a.rs"}},
        {"type": "future_block"},
    ]}});
    state.handle(&assistant);
    let subagent = json!({"type": "assistant", "parent_tool_use_id": "t9",
        "message": {"content": [{"type": "text", "text": "子代理"}]}});
    state.handle(&subagent);
    assert_eq!(
        state.handle(&json!({"type": "stream_event", "event": {}})),
        Handled::Nothing
    );
    assert_eq!(
        state.handle(&json!({"type": "user", "isReplay": true, "message": {"content": "x"}})),
        Handled::Nothing
    );
    assert_eq!(
        state.handle(&json!({"type": "brand_new"})),
        Handled::Nothing
    );
    let result = json!({"type": "result", "subtype": "success", "is_error": false,
        "num_turns": 3, "total_cost_usd": 0.5, "session_id": "s1", "result": "完成"});
    assert_eq!(state.handle(&result), Handled::Result(None));

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    assert_eq!(
        events,
        vec![
            ClaudeEvent::Started {
                session_id: "s1".into(),
                version: Some("2.1.0".into())
            },
            ClaudeEvent::Text("開始".into()),
            ClaudeEvent::ToolUse {
                name: "Read".into(),
                summary: "a.rs".into()
            },
        ]
    );
    let outcome = state.outcome(ClaudeEnd::TurnEnded);
    assert_eq!(outcome.last_text, "開始");
    assert_eq!(outcome.num_turns, 3);
    assert_eq!(outcome.cost_usd, Some(0.5));
    assert_eq!(outcome.version.as_deref(), Some("2.1.0"));
}

#[test]
fn state_errors_and_control() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut state = State::new(tx);
    state.handle(
        &json!({"type": "assistant", "error": "authentication_failed",
        "message": {"content": [{"type": "text", "text": "Invalid API key · Please run /login"}]}}),
    );
    assert_eq!(state.last_text, "", "CLI 的錯誤訊息不算 assistant 文字");
    assert_eq!(
        state.handle(
            &json!({"type": "result", "subtype": "success", "is_error": true,
            "result": "Invalid API key · Please run /login"})
        ),
        Handled::Result(Some(ClaudeFailure::NeedsLogin(
            "Invalid API key · Please run /login".into()
        )))
    );

    state.handle(&json!({"type": "rate_limit_event", "rate_limit_info": {
        "status": "allowed_warning"}}));
    assert_eq!(state.pending, None);
    state.handle(&json!({"type": "rate_limit_event", "rate_limit_info": {
        "status": "rejected", "rateLimitType": "five_hour", "resetsAt": 1_700_000_000}}));
    assert!(
        matches!(&state.pending, Some(ClaudeFailure::UsageLimit(m)) if m.contains("five_hour"))
    );

    assert_eq!(
        state.handle(
            &json!({"type": "control_request", "request_id": "p1", "request": {
            "subtype": "can_use_tool", "tool_name": "Bash", "input": {"command": "ls"}}})
        ),
        Handled::Permission {
            id: "p1".into(),
            tool: "Bash".into(),
            input: json!({"command": "ls"})
        }
    );
    assert_eq!(
        state.handle(&json!({"type": "control_request", "request_id": "h1",
            "request": {"subtype": "hook_callback"}})),
        Handled::Reply(control_error(
            "h1",
            "不支援的 control request：hook_callback"
        ))
    );
    assert_eq!(
        state.handle(&json!({"type": "control_response", "response": {
            "subtype": "error", "request_id": "init-1", "error": "nope"}})),
        Handled::ControlResponse {
            id: "init-1".into(),
            error: Some("nope".into())
        }
    );
    assert_eq!(
        state.handle(&json!({"type": "control_cancel_request", "request_id": "p1"})),
        Handled::Nothing
    );
}

#[test]
fn permission_frames() {
    assert_eq!(
        permission_reply("p1", Permission::Allow, json!({"command": "ls"})),
        json!({"type": "control_response", "response": {"subtype": "success",
            "request_id": "p1", "response": {"behavior": "allow", "updatedInput": {"command": "ls"}}}})
    );
    assert_eq!(
        permission_reply("p2", Permission::Deny("不行".into()), json!({})),
        json!({"type": "control_response", "response": {"subtype": "success",
            "request_id": "p2", "response": {"behavior": "deny", "message": "不行"}}})
    );
}

// ---- 對著假的 CLI ----

/// `target/<profile>/pots-fake-claude`（cargo 在跑這個 package 的測試前會先建好所有 bin）。
/// 測試執行檔可能放在 `target/<profile>/deps` 或 `target/<profile>/build/...`，往上找。
fn fake_bin() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let mut dir = exe.parent().expect("target/<profile>").to_path_buf();
    let name = format!("pots-fake-claude{}", std::env::consts::EXE_SUFFIX);
    loop {
        let bin = dir.join(&name);
        if bin.exists() {
            return bin;
        }
        if !dir.pop() {
            panic!(
                "找不到 pots-fake-claude；請先 `cargo build -p pots-server --bin pots-fake-claude`"
            );
        }
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    work: PathBuf,
    config: PathBuf,
    log: PathBuf,
}

impl Fixture {
    fn new(script: Value) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        let config = dir.path().join("config");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(dir.path().join("script.json"), script.to_string()).unwrap();
        let log = dir.path().join("log.jsonl");
        Self {
            dir,
            work,
            config,
            log,
        }
    }

    fn run(&self, session: SessionMode) -> ClaudeRun {
        let mut command = tokio::process::Command::new(fake_bin());
        command
            .current_dir(&self.work)
            .env(
                "POTS_FAKE_CLAUDE_SCRIPT",
                self.dir.path().join("script.json"),
            )
            .env("POTS_FAKE_CLAUDE_LOG", &self.log)
            .env("CLAUDE_CONFIG_DIR", &self.config);
        ClaudeRun {
            command,
            session,
            prompt: "請完成任務".into(),
            append_system_prompt: "你是 worker".into(),
            mcp_config_path: self.dir.path().join("mcp.json").display().to_string(),
            setting_sources: String::new(),
            settings_path: None,
            model: None,
            allowed_tools: Vec::new(),
            disallowed_tools: Vec::new(),
            policy: Arc::new(|_: &str, _: &Value| Permission::Allow),
            nudge: None,
            finished: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(Notify::new()),
            grace: Duration::from_secs(2),
            hard_stop: None,
        }
    }

    /// 記錄檔裡收到的 stdin frame。
    fn stdin_frames(&self) -> Vec<Value> {
        let text = std::fs::read_to_string(&self.log).unwrap_or_default();
        text.lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|r| r["kind"] == "stdin")
            .map(|r| r["frame"].clone())
            .collect()
    }
}

fn new_session() -> SessionMode {
    SessionMode::New(uuid::Uuid::now_v7().to_string())
}

async fn drive(run: ClaudeRun) -> (ClaudeOutcome, Vec<ClaudeEvent>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let outcome = tokio::time::timeout(
        Duration::from_secs(30),
        super::run(run, tx, CancellationToken::new()),
    )
    .await
    .expect("adapter 沒有在 30 秒內結束");
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    (outcome, events)
}

/// 背景跑 adapter，回傳事件接收端與結果。
fn spawn_run(
    run: ClaudeRun,
    cancel: CancellationToken,
) -> (
    mpsc::UnboundedReceiver<ClaudeEvent>,
    JoinHandle<ClaudeOutcome>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = tokio::spawn(super::run(run, tx, cancel));
    (rx, handle)
}

async fn wait_event(
    rx: &mut mpsc::UnboundedReceiver<ClaudeEvent>,
    pred: impl Fn(&ClaudeEvent) -> bool,
) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = rx.recv().await {
            if pred(&event) {
                return;
            }
        }
        panic!("事件串結束了");
    })
    .await
    .expect("等不到事件");
}

#[tokio::test]
async fn fake_happy_path_with_native_tool() {
    let fx = Fixture::new(json!({"new": [
        {"stdout_raw": "launcher: hello (not json)"},
        {"text": "我先看看"},
        {"native": {"tool": "Bash", "input": {"command": "echo hi > out.txt && echo done"}}},
        {"expect_contains": "done"},
        {"expect_is_error": false},
        {"end_turn": "做完了"},
    ]}));
    let session = new_session();
    let SessionMode::New(id) = &session else {
        unreachable!()
    };
    let id = id.clone();
    let (outcome, events) = drive(fx.run(session)).await;

    assert_eq!(outcome.end, ClaudeEnd::TurnEnded, "{outcome:?}");
    assert_eq!(outcome.session_id.as_deref(), Some(id.as_str()));
    assert_eq!(outcome.version.as_deref(), Some("0.0.0-fake"));
    assert_eq!(outcome.last_text, "做完了");
    assert_eq!(outcome.cost_usd, Some(0.01));
    assert!(outcome.num_turns >= 1);
    assert_eq!(
        events,
        vec![
            ClaudeEvent::Started {
                session_id: id.clone(),
                version: Some("0.0.0-fake".into())
            },
            ClaudeEvent::Text("我先看看".into()),
            ClaudeEvent::ToolUse {
                name: "Bash".into(),
                summary: "echo hi > out.txt && echo done".into()
            },
            ClaudeEvent::PermissionDecided {
                tool: "Bash".into(),
                allowed: true
            },
            ClaudeEvent::Text("做完了".into()),
        ]
    );
    assert_eq!(
        std::fs::read_to_string(fx.work.join("out.txt")).unwrap(),
        "hi\n"
    );

    let frames = fx.stdin_frames();
    assert_eq!(frames[0]["request"]["subtype"], "initialize");
    assert_eq!(frames[1]["type"], "user");
    assert_eq!(frames[1]["message"]["content"], "請完成任務");
    assert_eq!(frames[2]["response"]["response"]["behavior"], "allow");

    // transcript 寫在 CLAUDE_CONFIG_DIR/projects/<cwd>/<id>.jsonl。
    let cwd = std::fs::canonicalize(&fx.work)
        .unwrap()
        .display()
        .to_string();
    let project: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let transcript = fx
        .config
        .join("projects")
        .join(project)
        .join(format!("{id}.jsonl"));
    assert!(transcript.exists(), "{}", transcript.display());
}

#[tokio::test]
async fn fake_denied_tool_and_resume() {
    let fx = Fixture::new(json!({
        "new": [
            {"native": {"tool": "Bash", "input": {"command": "touch nope.txt"}}},
            {"expect_is_error": true},
            {"expect_contains": "不准"},
            {"end_turn": "被拒絕了"},
        ],
        "resume": [{"end_turn": "接著做"}],
    }));
    let session = new_session();
    let SessionMode::New(id) = session.clone() else {
        unreachable!()
    };
    let mut run = fx.run(session);
    run.policy = Arc::new(|tool: &str, _: &Value| {
        if tool == "Bash" {
            Permission::Deny("不准".into())
        } else {
            Permission::Allow
        }
    });
    let (outcome, events) = drive(run).await;
    assert_eq!(outcome.end, ClaudeEnd::TurnEnded, "{outcome:?}");
    assert!(events.contains(&ClaudeEvent::PermissionDecided {
        tool: "Bash".into(),
        allowed: false
    }));
    assert!(!fx.work.join("nope.txt").exists());

    let (outcome, _) = drive(fx.run(SessionMode::Resume(id.clone()))).await;
    assert_eq!(outcome.end, ClaudeEnd::TurnEnded, "{outcome:?}");
    assert_eq!(outcome.last_text, "接著做");
    assert_eq!(outcome.session_id.as_deref(), Some(id.as_str()));
}

#[tokio::test]
async fn fake_allowed_tools_skip_permission() {
    let fx = Fixture::new(json!([
        {"native": {"tool": "Bash", "input": {"command": "echo ok"}}},
        {"expect_contains": "ok"},
        {"end_turn": "好"},
    ]));
    let mut run = fx.run(new_session());
    run.allowed_tools = vec!["Bash".into()];
    run.policy = Arc::new(|_: &str, _: &Value| Permission::Deny("不該被問".into()));
    let (outcome, events) = drive(run).await;
    assert_eq!(outcome.end, ClaudeEnd::TurnEnded, "{outcome:?}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ClaudeEvent::PermissionDecided { .. }))
    );
}

#[tokio::test]
async fn fake_finished_interrupts() {
    let fx = Fixture::new(json!([
        {"text": "工作中"},
        {"sleep_ms": 20_000},
        {"end_turn": "不該出現"},
    ]));
    let mut run = fx.run(new_session());
    run.grace = Duration::from_secs(5);
    let finished = run.finished.clone();
    let stop = run.stop.clone();
    let (mut rx, handle) = spawn_run(run, CancellationToken::new());
    wait_event(&mut rx, |e| matches!(e, ClaudeEvent::Text(_))).await;
    let started = StdInstant::now();
    finished.store(true, Ordering::SeqCst);
    stop.notify_waiters();
    let outcome = handle.await.unwrap();
    assert_eq!(outcome.end, ClaudeEnd::Finished);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "應該靠 interrupt 收尾，不是等 grace"
    );
    assert_eq!(outcome.last_text, "工作中");
    assert!(
        fx.stdin_frames()
            .iter()
            .any(|f| f["request"]["subtype"] == "interrupt")
    );
}

#[tokio::test]
async fn fake_finished_before_start() {
    let fx = Fixture::new(json!([{"text": "呼叫了結束工具"}, {"end_turn": "收工"}]));
    let mut run = fx.run(new_session());
    run.nudge = Some("記得呼叫 complete_task".into());
    run.finished.store(true, Ordering::SeqCst);
    let (outcome, _) = drive(run).await;
    // 一開始就是 finished：adapter 在送出任務前就收尾。
    assert_eq!(outcome.end, ClaudeEnd::Finished);
    assert!(!fx.stdin_frames().iter().any(|f| f["type"] == "user"));
}

#[tokio::test]
async fn fake_nudge_once_then_turn_ended() {
    let fx = Fixture::new(json!([
        {"end_turn": "第一輪"},
        {"text": "還是沒呼叫"},
        {"end_turn": "第二輪"},
    ]));
    let mut run = fx.run(new_session());
    run.nudge = Some("記得呼叫 complete_task".into());
    let (outcome, _) = drive(run).await;
    assert_eq!(outcome.end, ClaudeEnd::TurnEnded);
    assert_eq!(outcome.last_text, "第二輪");
    assert_eq!(outcome.num_turns, 3);
    assert_eq!(outcome.cost_usd, Some(0.02));
    let users: Vec<Value> = fx
        .stdin_frames()
        .into_iter()
        .filter(|f| f["type"] == "user")
        .collect();
    assert_eq!(users.len(), 2);
    assert_eq!(users[1]["message"]["content"], "記得呼叫 complete_task");
}

#[tokio::test]
async fn fake_cancel() {
    let fx = Fixture::new(json!([{"text": "開始"}, {"sleep_ms": 20_000}]));
    let cancel = CancellationToken::new();
    let (mut rx, handle) = spawn_run(fx.run(new_session()), cancel.clone());
    wait_event(&mut rx, |e| matches!(e, ClaudeEvent::Text(_))).await;
    let started = StdInstant::now();
    cancel.cancel();
    let outcome = handle.await.unwrap();
    assert_eq!(outcome.end, ClaudeEnd::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn fake_hang_is_killed() {
    let fx = Fixture::new(json!([{"text": "卡住"}, {"hang": true, "ignore_sigterm": true}]));
    let mut run = fx.run(new_session());
    run.grace = Duration::from_millis(300);
    let hard_stopped = Arc::new(AtomicBool::new(false));
    let flag = hard_stopped.clone();
    run.hard_stop = Some(Box::new(move || {
        Box::pin(async move { flag.store(true, Ordering::SeqCst) })
    }));
    let finished = run.finished.clone();
    let stop = run.stop.clone();
    let (mut rx, handle) = spawn_run(run, CancellationToken::new());
    wait_event(&mut rx, |e| matches!(e, ClaudeEvent::Text(_))).await;
    let started = StdInstant::now();
    finished.store(true, Ordering::SeqCst);
    stop.notify_one();
    let outcome = tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("沒有被砍掉")
        .unwrap();
    assert_eq!(outcome.end, ClaudeEnd::Finished);
    assert!(
        hard_stopped.load(Ordering::SeqCst),
        "強制終止後要呼叫 hard_stop"
    );
    // grace（中斷）＋ grace（等結束）＋ SIGTERM 之後的等待。
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[tokio::test]
async fn fake_auth_error() {
    let fx = Fixture::new(json!([{"error": "authentication_failed"}]));
    let (outcome, events) = drive(fx.run(new_session())).await;
    assert!(
        matches!(&outcome.end, ClaudeEnd::Failed(ClaudeFailure::NeedsLogin(m)) if m.contains("/login")),
        "{outcome:?}"
    );
    assert!(!events.iter().any(|e| matches!(e, ClaudeEvent::Text(_))));
}

#[tokio::test]
async fn fake_rate_limit() {
    let fx = Fixture::new(json!([
        {"rate_limit_event": "allowed_warning"},
        {"text": "做一點"},
        {"rate_limit_rejected": true},
        {"error": "rate_limit"},
    ]));
    let (outcome, _) = drive(fx.run(new_session())).await;
    assert!(
        matches!(&outcome.end, ClaudeEnd::Failed(ClaudeFailure::UsageLimit(m)) if m.contains("five_hour")),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn fake_result_error_is_cli() {
    let fx = Fixture::new(json!([{"result_error": "API Error: 500 overloaded"}]));
    let (outcome, _) = drive(fx.run(new_session())).await;
    assert_eq!(
        outcome.end,
        ClaudeEnd::Failed(ClaudeFailure::Cli("API Error: 500 overloaded".into()))
    );
}

#[tokio::test]
async fn fake_resume_missing() {
    let fx = Fixture::new(json!([{"end_turn": "不該跑到"}]));
    let (outcome, _) = drive(fx.run(SessionMode::Resume("missing-1".into()))).await;
    assert_eq!(
        outcome.end,
        ClaudeEnd::Failed(ClaudeFailure::Cli(
            "No conversation found with session ID: missing-1".into()
        ))
    );
}

#[tokio::test]
async fn fake_exit_without_result() {
    let fx = Fixture::new(json!([{"stderr": "boom: 壞掉了"}, {"exit": 2}]));
    let (outcome, _) = drive(fx.run(new_session())).await;
    match outcome.end {
        ClaudeEnd::Failed(ClaudeFailure::Exit { code, stderr }) => {
            assert_eq!(code, Some(2));
            assert!(stderr.contains("boom"), "{stderr}");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn fake_clean_exit_without_result_is_protocol() {
    let fx = Fixture::new(json!([{"text": "嗨"}, {"exit": 0}]));
    let (outcome, _) = drive(fx.run(new_session())).await;
    assert!(
        matches!(outcome.end, ClaudeEnd::Failed(ClaudeFailure::Protocol(_))),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn spawn_failure() {
    let mut run = sample_run(new_session());
    run.command = tokio::process::Command::new("/nonexistent/pots-claude");
    let (outcome, _) = drive(run).await;
    assert!(matches!(
        outcome.end,
        ClaudeEnd::Failed(ClaudeFailure::Spawn(_))
    ));
}

#[tokio::test]
async fn init_timeout_or_garbage_is_protocol() {
    // 一個只會吐雜訊然後正常結束的「CLI」。
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("cli.sh");
    let mut file = tokio::fs::File::create(&script).await.unwrap();
    file.write_all(b"#!/bin/sh\necho 'not json'\nexit 0\n")
        .await
        .unwrap();
    drop(file);
    let mut run = sample_run(new_session());
    run.command = tokio::process::Command::new("sh");
    run.command.arg(&script);
    let (outcome, _) = drive(run).await;
    assert!(
        matches!(outcome.end, ClaudeEnd::Failed(ClaudeFailure::Protocol(_))),
        "{outcome:?}"
    );
}

// ---- 假 CLI 的 MCP client ----

/// 只有一個 `echo` 工具的 MCP server；`fail: true` 時回 `is_error`。
#[derive(Clone)]
struct EchoServer;

impl rmcp::ServerHandler for EchoServer {
    fn get_info(&self) -> rmcp::model::ServerConfig {
        rmcp::model::ServerConfig::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, rmcp::ErrorData> {
        let schema = json!({"type": "object", "properties": {"text": {"type": "string"}}});
        let Value::Object(schema) = schema else {
            unreachable!()
        };
        Ok(rmcp::model::ListToolsResult::with_all_items(vec![
            rmcp::model::Tool::new("echo", "回聲", Arc::new(schema)),
        ]))
    }

    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        let args = request.arguments.unwrap_or_default();
        let text = format!(
            "{}:{}",
            request.name,
            args.get("text").and_then(Value::as_str).unwrap_or_default()
        );
        let content = vec![rmcp::model::ContentBlock::text(text)];
        let result = if args.get("fail").and_then(Value::as_bool) == Some(true) {
            rmcp::model::CallToolResult::error(content)
        } else {
            rmcp::model::CallToolResult::success(content)
        };
        Ok(rmcp::model::CallToolResponse::Complete(result))
    }
}

/// 起一個要求 `Authorization: Bearer secret` 的 MCP server，回傳 `/mcp` 的 URL。
async fn echo_mcp_server() -> String {
    use axum::response::IntoResponse;
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};

    let service = StreamableHttpService::new(
        || Ok(EchoServer),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let app = axum::Router::new()
        .nest_service("/mcp", service)
        .layer(axum::middleware::from_fn(
            |request: axum::extract::Request, next: axum::middleware::Next| async move {
                let authorized = request
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    == Some("Bearer secret");
                if authorized {
                    next.run(request).await
                } else {
                    axum::http::StatusCode::UNAUTHORIZED.into_response()
                }
            },
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("http://127.0.0.1:{}/mcp", addr.port())
}

#[tokio::test]
async fn fake_calls_mcp_tools() {
    let url = echo_mcp_server().await;
    let fx = Fixture::new(json!([
        {"mcp": {"tool": "echo", "args": {"text": "哈囉"}}},
        {"expect_contains": "echo:哈囉"},
        {"expect_is_error": false},
        {"mcp": {"tool": "echo", "args": {"text": "壞", "fail": true}}},
        {"expect_is_error": true},
        {"mcp": {"tool": "echo", "args": {"text": "再一次"}}},
        {"end_turn": "好了"},
    ]));
    std::fs::write(
        fx.dir.path().join("mcp.json"),
        json!({"mcpServers": {"pots": {
            "type": "http",
            "url": url,
            "headers": {"Authorization": "Bearer secret", "X-Pots-Run": "r1"},
        }}})
        .to_string(),
    )
    .unwrap();
    let mut run = fx.run(new_session());
    // 在 allowedTools 裡：不問權限。
    run.allowed_tools = vec!["mcp__pots__echo".into()];
    let (outcome, events) = drive(run).await;
    assert_eq!(outcome.end, ClaudeEnd::TurnEnded, "{outcome:?}");
    assert!(events.contains(&ClaudeEvent::ToolUse {
        name: "mcp__pots__echo".into(),
        summary: "平台工具 echo".into()
    }));

    let log = std::fs::read_to_string(&fx.log).unwrap();
    let tools: Vec<Value> = log
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|r| r["kind"] == "tool")
        .collect();
    assert_eq!(tools.len(), 3);
    assert_eq!(tools[0]["output"], "echo:哈囉");
    assert_eq!(tools[1]["is_error"], true);
}

#[tokio::test]
async fn fake_mcp_permission_and_bad_token() {
    let url = echo_mcp_server().await;
    let fx = Fixture::new(json!([
        {"mcp": {"tool": "echo", "args": {"text": "x"}}},
        {"expect_is_error": true},
        {"end_turn": "連不上"},
    ]));
    std::fs::write(
        fx.dir.path().join("mcp.json"),
        json!({"mcpServers": {"pots": {
            "type": "http", "url": url, "headers": {"Authorization": "Bearer wrong"},
        }}})
        .to_string(),
    )
    .unwrap();
    // 不在 allowedTools：假 CLI 會先問權限。
    let (outcome, events) = drive(fx.run(new_session())).await;
    assert_eq!(outcome.end, ClaudeEnd::TurnEnded, "{outcome:?}");
    assert!(events.contains(&ClaudeEvent::PermissionDecided {
        tool: "mcp__pots__echo".into(),
        allowed: true
    }));
}
