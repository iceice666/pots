use std::sync::Mutex;
use std::time::Duration;

use super::*;
use crate::testing::{FnTool, ScriptedProvider, ScriptedTurn};

/// 依工具名稱決定的 hook；`max_turns_allowed` 之後 `before_turn` 回 `false`。
#[derive(Default)]
struct TestHook {
    decisions: HashMap<String, ToolDecision>,
    stop_at_turn: Option<usize>,
    seen_turns: Mutex<Vec<usize>>,
    after: Mutex<Vec<(String, String)>>,
}

#[async_trait::async_trait]
impl Hook for TestHook {
    async fn before_turn(&self, turn: usize) -> bool {
        self.seen_turns.lock().unwrap().push(turn);
        self.stop_at_turn != Some(turn)
    }

    async fn before_tool(&self, call: &ToolCall) -> ToolDecision {
        self.decisions
            .get(&call.name)
            .cloned()
            .unwrap_or(ToolDecision::Allow)
    }

    async fn after_tool(&self, call: &ToolCall, output: &str) {
        self.after
            .lock()
            .unwrap()
            .push((call.name.clone(), output.to_owned()));
    }
}

/// 永遠不會完成的工具，用來測取消。
struct HangTool;

#[async_trait::async_trait]
impl Tool for HangTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "hang".into(),
            description: "不會結束".into(),
            parameters: serde_json::json!({ "type": "object" }),
        }
    }

    async fn call(&self, _arguments: &str) -> Result<String, ToolError> {
        futures::future::pending().await
    }
}

fn config(
    provider: Arc<ScriptedProvider>,
    tools: Vec<Arc<dyn Tool>>,
    hook: Arc<dyn Hook>,
) -> RunConfig {
    RunConfig {
        provider,
        model: "test-model".into(),
        instructions: Some("你是測試".into()),
        tools,
        hook,
        max_turns: 8,
    }
}

fn user(text: &str) -> Item {
    Item::User { text: text.into() }
}

fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
    ToolCall {
        call_id: id.into(),
        name: name.into(),
        arguments: arguments.into(),
    }
}

fn tool_out(id: &str, output: &str) -> Item {
    Item::ToolResult {
        call_id: id.into(),
        output: output.into(),
    }
}

fn get_time() -> Arc<FnTool> {
    FnTool::new("get_time", |_| Ok("12:00".into()))
}

async fn run_collect(
    config: &RunConfig,
    history: Vec<Item>,
    cancel: CancellationToken,
) -> (Result<RunResult, LlmError>, Vec<AgentEvent>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let result = run(config, history, tx, cancel).await;
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    (result, events)
}

#[tokio::test]
async fn text_only_turn_is_done() {
    let provider =
        ScriptedProvider::new([ScriptedTurn::new().text_deltas(["你", "好"]).usage(3, 2)]);
    let cfg = config(provider.clone(), vec![get_time()], Arc::new(AllowAll));
    let (result, events) = run_collect(&cfg, vec![user("嗨")], CancellationToken::new()).await;
    let result = result.unwrap();

    let reply = Item::Assistant {
        text: "你好".into(),
    };
    assert_eq!(result.outcome, RunOutcome::Done);
    assert_eq!(result.new_items, vec![reply.clone()]);
    assert_eq!(
        result.usage,
        Usage {
            input_tokens: 3,
            output_tokens: 2
        }
    );
    assert_eq!(
        events,
        vec![
            AgentEvent::TextDelta("你".into()),
            AgentEvent::TextDelta("好".into()),
            AgentEvent::Item(reply),
            AgentEvent::TurnCompleted {
                turn: 0,
                usage: result.usage
            },
        ]
    );

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model, "test-model");
    assert_eq!(requests[0].instructions.as_deref(), Some("你是測試"));
    assert_eq!(requests[0].input, vec![user("嗨")]);
    assert_eq!(requests[0].tools.len(), 1);
    assert_eq!(requests[0].tools[0].name, "get_time");
}

#[tokio::test]
async fn tool_roundtrip_feeds_result_into_next_turn() {
    let provider = ScriptedProvider::new([
        ScriptedTurn::new()
            .text("我查一下")
            .tool_call("c1", "get_time", "{}")
            .usage(10, 5),
        ScriptedTurn::reply("現在 12:00").usage(20, 3),
    ]);
    let tool = get_time();
    let hook = Arc::new(TestHook::default());
    let cfg = config(provider.clone(), vec![tool.clone()], hook.clone());
    let (result, events) = run_collect(&cfg, vec![user("幾點")], CancellationToken::new()).await;
    let result = result.unwrap();

    let tool_call = call("c1", "get_time", "{}");
    let expected = vec![
        Item::Assistant {
            text: "我查一下".into(),
        },
        Item::ToolCall(tool_call.clone()),
        tool_out("c1", "12:00"),
        Item::Assistant {
            text: "現在 12:00".into(),
        },
    ];
    assert_eq!(result.outcome, RunOutcome::Done);
    assert_eq!(result.new_items, expected);
    assert_eq!(
        result.usage,
        Usage {
            input_tokens: 30,
            output_tokens: 8
        }
    );
    assert_eq!(tool.calls(), vec!["{}".to_string()]);
    assert_eq!(
        *hook.after.lock().unwrap(),
        vec![("get_time".into(), "12:00".into())]
    );

    // 事件依歷史順序。
    let items: Vec<&Item> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Item(item) => Some(item),
            _ => None,
        })
        .collect();
    assert_eq!(items, expected.iter().collect::<Vec<_>>());
    let started = events
        .iter()
        .position(|e| *e == AgentEvent::ToolStarted(tool_call.clone()))
        .unwrap();
    let finished = events
        .iter()
        .position(|e| {
            *e == AgentEvent::ToolFinished {
                call: tool_call.clone(),
                ok: true,
            }
        })
        .unwrap();
    let result_pos = events
        .iter()
        .position(|e| *e == AgentEvent::Item(tool_out("c1", "12:00")))
        .unwrap();
    assert!(started < finished && finished < result_pos);

    // 第二輪的請求帶著完整歷史。
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let mut second_input = vec![user("幾點")];
    second_input.extend(expected[..3].iter().cloned());
    assert_eq!(requests[1].input, second_input);
}

#[tokio::test]
async fn deny_sends_reason_without_running_tool() {
    let provider = ScriptedProvider::new([
        ScriptedTurn::new().tool_call("c1", "get_time", "{}"),
        ScriptedTurn::reply("好"),
    ]);
    let tool = get_time();
    let hook = Arc::new(TestHook {
        decisions: HashMap::from([("get_time".into(), ToolDecision::Deny("不准".into()))]),
        ..TestHook::default()
    });
    let cfg = config(provider, vec![tool.clone()], hook);
    let (result, events) = run_collect(&cfg, vec![user("x")], CancellationToken::new()).await;
    let result = result.unwrap();
    assert_eq!(result.outcome, RunOutcome::Done);
    assert_eq!(result.new_items[1], tool_out("c1", "不准"));
    assert!(tool.calls().is_empty());
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolStarted(_)))
    );
}

#[tokio::test]
async fn stop_suspends_and_skips_remaining_calls() {
    let provider = ScriptedProvider::new([ScriptedTurn::new()
        .tool_call("c1", "approve_me", "{}")
        .tool_call("c2", "get_time", "{}")]);
    let tool = get_time();
    let approve = FnTool::new("approve_me", |_| Ok("ran".into()));
    let hook = Arc::new(TestHook {
        decisions: HashMap::from([("approve_me".into(), ToolDecision::Stop("等待核准".into()))]),
        ..TestHook::default()
    });
    let cfg = config(provider.clone(), vec![tool.clone(), approve.clone()], hook);
    let (result, _) = run_collect(&cfg, vec![user("x")], CancellationToken::new()).await;
    let result = result.unwrap();
    assert_eq!(result.outcome, RunOutcome::Suspended);
    assert_eq!(
        result.new_items[2..],
        [
            tool_out("c1", "等待核准"),
            tool_out("c2", SKIPPED_AFTER_STOP)
        ]
    );
    assert!(approve.calls().is_empty());
    assert!(tool.calls().is_empty());
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn allow_then_stop_runs_tool_then_suspends() {
    let provider = ScriptedProvider::new([ScriptedTurn::new().tool_call("c1", "wait_for", "{}")]);
    let wait = FnTool::new("wait_for", |_| Ok("已掛起".into()));
    let hook = Arc::new(TestHook {
        decisions: HashMap::from([("wait_for".into(), ToolDecision::AllowThenStop)]),
        ..TestHook::default()
    });
    let cfg = config(provider.clone(), vec![wait.clone()], hook);
    let (result, _) = run_collect(&cfg, vec![user("x")], CancellationToken::new()).await;
    let result = result.unwrap();
    assert_eq!(result.outcome, RunOutcome::Suspended);
    assert_eq!(result.new_items[1], tool_out("c1", "已掛起"));
    assert_eq!(wait.calls().len(), 1);
    assert_eq!(provider.remaining(), 0);
}

#[tokio::test]
async fn before_turn_false_stops_cleanly() {
    let provider = ScriptedProvider::new([
        ScriptedTurn::new().tool_call("c1", "get_time", "{}"),
        ScriptedTurn::reply("不該送出"),
    ]);
    let hook = Arc::new(TestHook {
        stop_at_turn: Some(1),
        ..TestHook::default()
    });
    let cfg = config(provider.clone(), vec![get_time()], hook.clone());
    let (result, _) = run_collect(&cfg, vec![user("x")], CancellationToken::new()).await;
    let result = result.unwrap();
    assert_eq!(result.outcome, RunOutcome::Stopped);
    assert_eq!(result.new_items.len(), 2);
    assert_eq!(*hook.seen_turns.lock().unwrap(), vec![0, 1]);
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn max_turns_is_reported() {
    let provider = ScriptedProvider::new([
        ScriptedTurn::new().tool_call("c1", "get_time", "{}"),
        ScriptedTurn::new().tool_call("c2", "get_time", "{}"),
    ]);
    let mut cfg = config(provider.clone(), vec![get_time()], Arc::new(AllowAll));
    cfg.max_turns = 2;
    let (result, _) = run_collect(&cfg, vec![user("x")], CancellationToken::new()).await;
    let result = result.unwrap();
    assert_eq!(result.outcome, RunOutcome::MaxTurns);
    assert_eq!(result.new_items.len(), 4);
    assert_eq!(result.new_items[3], tool_out("c2", "12:00"));

    cfg.max_turns = 0;
    let (result, _) = run_collect(&cfg, vec![user("x")], CancellationToken::new()).await;
    assert_eq!(result.unwrap().outcome, RunOutcome::MaxTurns);
}

#[tokio::test]
async fn unknown_tool_and_tool_error_go_back_to_model() {
    let provider = ScriptedProvider::new([
        ScriptedTurn::new()
            .tool_call("c1", "nope", "{}")
            .tool_call("c2", "broken", "{}"),
        ScriptedTurn::reply("我修正"),
    ]);
    let broken = FnTool::new("broken", |_| Err(ToolError("參數錯誤".into())));
    let cfg = config(provider, vec![broken], Arc::new(AllowAll));
    let (result, events) = run_collect(&cfg, vec![user("x")], CancellationToken::new()).await;
    let result = result.unwrap();
    assert_eq!(result.outcome, RunOutcome::Done);
    assert_eq!(
        result.new_items[2],
        tool_out("c1", "錯誤：沒有名為 `nope` 的工具")
    );
    assert_eq!(result.new_items[3], tool_out("c2", "錯誤：參數錯誤"));
    assert!(events.contains(&AgentEvent::ToolFinished {
        call: call("c2", "broken", "{}"),
        ok: false,
    }));
}

#[tokio::test]
async fn failed_turn_discards_its_items() {
    let provider = ScriptedProvider::new([ScriptedTurn::new()
        .text("一半")
        .tool_call("c1", "get_time", "{}")
        .fail(LlmError::UsageLimit)]);
    let cfg = config(provider, vec![get_time()], Arc::new(AllowAll));
    let (result, events) = run_collect(&cfg, vec![user("x")], CancellationToken::new()).await;
    assert_eq!(result.unwrap_err(), LlmError::UsageLimit);
    assert_eq!(events, vec![AgentEvent::TextDelta("一半".into())]);
}

#[tokio::test]
async fn eof_without_completed_is_interrupted() {
    let provider = ScriptedProvider::new([ScriptedTurn::new().text("一半").eof()]);
    let cfg = config(provider, vec![], Arc::new(AllowAll));
    let (result, _) = run_collect(&cfg, vec![user("x")], CancellationToken::new()).await;
    assert!(matches!(result, Err(LlmError::Interrupted(_))));
}

#[tokio::test]
async fn rejected_stream_error_passes_through() {
    let provider = ScriptedProvider::new([ScriptedTurn::reject(LlmError::NeedsLogin)]);
    let cfg = config(provider, vec![], Arc::new(AllowAll));
    let (result, events) = run_collect(&cfg, vec![user("x")], CancellationToken::new()).await;
    assert_eq!(result.unwrap_err(), LlmError::NeedsLogin);
    assert!(events.is_empty());
}

#[tokio::test]
async fn cancel_before_start_sends_nothing() {
    let provider = ScriptedProvider::new([ScriptedTurn::reply("x")]);
    let cfg = config(provider.clone(), vec![], Arc::new(AllowAll));
    let cancel = CancellationToken::new();
    cancel.cancel();
    let (result, _) = run_collect(&cfg, vec![user("x")], cancel).await;
    assert_eq!(result.unwrap_err(), LlmError::Cancelled);
    assert!(provider.requests().is_empty());
}

#[tokio::test]
async fn cancel_during_stream() {
    let provider = ScriptedProvider::new([ScriptedTurn::new().text("開始").hang()]);
    let cfg = config(provider, vec![], Arc::new(AllowAll));
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        trigger.cancel();
    });
    let (result, events) = run_collect(&cfg, vec![user("x")], cancel).await;
    assert_eq!(result.unwrap_err(), LlmError::Cancelled);
    assert_eq!(events, vec![AgentEvent::TextDelta("開始".into())]);
}

#[tokio::test]
async fn cancel_during_tool_fills_pending_results() {
    let provider = ScriptedProvider::new([ScriptedTurn::new()
        .tool_call("c1", "hang", "{}")
        .tool_call("c2", "get_time", "{}")]);
    let cfg = config(
        provider,
        vec![Arc::new(HangTool), get_time()],
        Arc::new(AllowAll),
    );
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        trigger.cancel();
    });
    let (result, events) = run_collect(&cfg, vec![user("x")], cancel).await;
    assert_eq!(result.unwrap_err(), LlmError::Cancelled);
    let items: Vec<Item> = events
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::Item(item) => Some(item),
            _ => None,
        })
        .collect();
    assert_eq!(
        items,
        vec![
            Item::ToolCall(call("c1", "hang", "{}")),
            Item::ToolCall(call("c2", "get_time", "{}")),
            tool_out("c1", CANCELLED_OUTPUT),
            tool_out("c2", CANCELLED_OUTPUT),
        ]
    );
}
