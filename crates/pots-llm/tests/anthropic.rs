//! Anthropic Messages provider 對假 server 的整合測試。

mod common;

use std::sync::Arc;

use common::api::FakeApi;
use common::{Reply, sse};
use futures::StreamExt;
use pots_llm::agent::{self, AllowAll, RunConfig, RunOutcome};
use pots_llm::anthropic::{AnthropicProvider, ModelInfo, PROVIDER_NAME};
use pots_llm::reqwest::header::{HeaderMap, HeaderValue};
use pots_llm::testing::FnTool;
use pots_llm::{
    Item, LlmError, ModelProvider, ModelRequest, Reasoning, StreamEvent, ToolCall, ToolSpec, Usage,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const KEY: &str = "sk-ant-test-secret";

fn provider(fake: &FakeApi, base: &str) -> AnthropicProvider {
    AnthropicProvider::new(fake.url(base), KEY.into(), reqwest::Client::new())
}

fn request(text: &str) -> ModelRequest {
    ModelRequest {
        reasoning: None,
        model: "claude-test".into(),
        instructions: Some("你是 Pots".into()),
        input: vec![Item::User { text: text.into() }],
        tools: vec![],
        session_id: None,
    }
}

async fn collect(
    provider: &AnthropicProvider,
    request: ModelRequest,
) -> Result<Vec<Result<StreamEvent, LlmError>>, LlmError> {
    let stream = provider.stream(request, CancellationToken::new()).await?;
    Ok(stream.collect().await)
}

fn message_start(input_tokens: u64) -> Value {
    json!({"type": "message_start", "message": {
        "id": "msg_1", "type": "message", "role": "assistant", "content": [],
        "usage": {"input_tokens": input_tokens, "output_tokens": 1}
    }})
}

/// 一個完整的文字 block。
fn text_block(index: u64, deltas: &[&str]) -> Vec<Value> {
    let mut events = vec![
        json!({"type": "content_block_start", "index": index, "content_block": {"type": "text", "text": ""}}),
    ];
    events.extend(deltas.iter().map(|delta| {
        json!({"type": "content_block_delta", "index": index, "delta": {"type": "text_delta", "text": delta}})
    }));
    events.push(json!({"type": "content_block_stop", "index": index}));
    events
}

fn message_end(stop_reason: &str, output_tokens: u64) -> [Value; 2] {
    [
        json!({"type": "message_delta", "delta": {"stop_reason": stop_reason, "stop_sequence": null}, "usage": {"output_tokens": output_tokens}}),
        json!({"type": "message_stop"}),
    ]
}

fn stream(parts: Vec<Vec<Value>>) -> String {
    sse(&parts.concat())
}

fn extra_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("x-pots-test", HeaderValue::from_static("1"));
    headers
}

#[tokio::test]
async fn sends_extra_headers_on_models_and_stream() {
    let fake = FakeApi::start([
        Reply::Status(200, json!({"data": []})),
        Reply::Sse(stream(vec![
            vec![message_start(1)],
            text_block(0, &["好"]),
            message_end("end_turn", 1).to_vec(),
        ])),
    ])
    .await;
    let provider = provider(&fake, "").with_headers(extra_headers());
    provider.list_models().await.unwrap();
    let mut request = request("嗨");
    request.session_id = Some("session-1".into());
    collect(&provider.with_session_header("x-opencode-session"), request)
        .await
        .unwrap();

    let requests = fake.requests();
    assert_eq!(requests[0].header("x-pots-test"), Some("1"));
    assert_eq!(requests[0].header("x-api-key"), Some(KEY));
    assert_eq!(requests[1].header("x-pots-test"), Some("1"));
    assert_eq!(requests[1].header("x-opencode-session"), Some("session-1"));
}

#[tokio::test]
async fn streams_text_until_message_stop() {
    let fake = FakeApi::start([Reply::Sse(stream(vec![
        vec![message_start(5), json!({"type": "ping"})],
        text_block(0, &["你", "好"]),
        message_end("end_turn", 2).to_vec(),
    ]))])
    .await;
    let events = collect(&provider(&fake, ""), request("嗨")).await.unwrap();
    assert_eq!(
        events,
        vec![
            Ok(StreamEvent::TextDelta("你".into())),
            Ok(StreamEvent::TextDelta("好".into())),
            Ok(StreamEvent::ItemDone(Item::Assistant {
                text: "你好".into()
            })),
            Ok(StreamEvent::Completed {
                usage: Usage {
                    input_tokens: 5,
                    output_tokens: 2
                }
            }),
        ]
    );
    let requests = fake.requests();
    let sent = &requests[0];
    assert_eq!(sent.method, "POST");
    assert_eq!(sent.path, "/v1/messages");
    assert_eq!(sent.header("x-api-key"), Some(KEY));
    assert_eq!(sent.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(sent.header("content-type"), Some("application/json"));
    assert_eq!(sent.header("authorization"), None);
    assert_eq!(
        sent.body,
        json!({
            "model": "claude-test",
            "max_tokens": 8192,
            "system": "你是 Pots",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "嗨"}]}],
            "stream": true,
        })
    );
}

#[tokio::test]
async fn base_url_with_or_without_v1() {
    let ok = || {
        Reply::Sse(stream(vec![
            vec![message_start(1)],
            text_block(0, &["ok"]),
            message_end("end_turn", 1).to_vec(),
        ]))
    };
    let fake = FakeApi::start([ok(), ok(), ok()]).await;
    for base in ["", "v1", "v1/"] {
        let events = collect(&provider(&fake, base), request("x")).await.unwrap();
        assert!(matches!(
            events.last(),
            Some(Ok(StreamEvent::Completed { .. }))
        ));
    }
    let paths: Vec<_> = fake.requests().into_iter().map(|r| r.path).collect();
    assert_eq!(paths, vec!["/v1/messages"; 3]);
}

#[tokio::test]
async fn tool_roundtrip_through_agent_loop() {
    let fake = FakeApi::start([
        Reply::Sse(stream(vec![
            vec![message_start(10)],
            text_block(0, &["我查一下"]),
            vec![
                json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "get_time", "input": {}}}),
                json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": ""}}),
                json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"tz\": \"UTC\"}"}}),
                json!({"type": "content_block_stop", "index": 1}),
            ],
            message_end("tool_use", 4).to_vec(),
        ])),
        Reply::Sse(stream(vec![
            vec![message_start(20)],
            text_block(0, &["12:00"]),
            message_end("end_turn", 2).to_vec(),
        ])),
    ])
    .await;
    let tool = FnTool::with_spec(
        ToolSpec {
            name: "get_time".into(),
            description: "現在時間".into(),
            parameters: json!({"type": "object", "properties": {"tz": {"type": "string"}}}),
        },
        |_| Ok("12:00".into()),
    );
    let config = RunConfig {
        reasoning: None,
        provider: Arc::new(provider(&fake, "").with_max_tokens(1024)),
        model: "claude-test".into(),
        instructions: Some("你是 Pots".into()),
        tools: vec![tool.clone()],
        hook: Arc::new(AllowAll),
        max_turns: Some(4),
        session_id: None,
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let result = agent::run(
        &config,
        vec![Item::User {
            text: "幾點".into(),
        }],
        tx,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, RunOutcome::Done);
    assert_eq!(tool.calls(), vec!["{\"tz\": \"UTC\"}".to_string()]);
    assert_eq!(
        result.new_items[1],
        Item::ToolCall(ToolCall {
            call_id: "toolu_1".into(),
            name: "get_time".into(),
            arguments: "{\"tz\": \"UTC\"}".into(),
        })
    );
    assert_eq!(
        result.usage,
        Usage {
            input_tokens: 30,
            output_tokens: 6
        }
    );

    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body["max_tokens"], 1024);
    assert_eq!(
        requests[0].body["tools"],
        json!([{
            "name": "get_time",
            "description": "現在時間",
            "input_schema": {"type": "object", "properties": {"tz": {"type": "string"}}},
        }])
    );
    assert_eq!(requests[1].body["system"], "你是 Pots");
    assert_eq!(
        requests[1].body["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": "幾點"}]},
            {"role": "assistant", "content": [
                {"type": "text", "text": "我查一下"},
                {"type": "tool_use", "id": "toolu_1", "name": "get_time", "input": {"tz": "UTC"}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "12:00"},
            ]},
        ])
    );
}

#[tokio::test]
async fn tool_results_and_new_user_text_share_one_message() {
    let fake = FakeApi::start([Reply::Sse(stream(vec![
        vec![message_start(1)],
        text_block(0, &["ok"]),
        message_end("end_turn", 1).to_vec(),
    ]))])
    .await;
    let mut request = request("幾點");
    request.input.extend([
        Item::ToolCall(ToolCall {
            call_id: "toolu_1".into(),
            name: "get_time".into(),
            arguments: "{}".into(),
        }),
        Item::ToolResult {
            call_id: "toolu_1".into(),
            output: "12:00".into(),
        },
        Item::User {
            text: "那台北呢".into(),
        },
    ]);
    collect(&provider(&fake, ""), request).await.unwrap();
    let messages = fake.requests()[0].body["messages"].clone();
    assert_eq!(
        messages,
        json!([
            {"role": "user", "content": [{"type": "text", "text": "幾點"}]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "get_time", "input": {}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "12:00"},
                {"type": "text", "text": "那台北呢"},
            ]},
        ])
    );
}

#[tokio::test]
async fn eof_before_message_stop_is_interrupted() {
    let fake = FakeApi::start([Reply::Sse(stream(vec![
        vec![message_start(1)],
        text_block(0, &["一半"]),
    ]))])
    .await;
    let events = collect(&provider(&fake, ""), request("x")).await.unwrap();
    assert!(matches!(events.last(), Some(Err(LlmError::Interrupted(_)))));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Ok(StreamEvent::Completed { .. })))
    );
}

#[tokio::test]
async fn error_event_in_stream() {
    let fake = FakeApi::start([Reply::Sse(sse(&[
        message_start(1),
        json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
    ]))])
    .await;
    let events = collect(&provider(&fake, ""), request("x")).await.unwrap();
    assert!(matches!(&events[..], [Err(LlmError::Unavailable(m))] if m.contains("Overloaded")));
}

#[tokio::test]
async fn http_errors_are_classified_without_key() {
    let error = |kind: &str, message: &str| json!({"type": "error", "error": {"type": kind, "message": message}});
    let fake = FakeApi::start([
        Reply::Status(401, error("authentication_error", "invalid x-api-key")),
        Reply::Status(404, error("not_found_error", "model: claude-nope")),
        Reply::Status(
            429,
            error(
                "rate_limit_error",
                "Number of requests has exceeded your rate limit",
            ),
        ),
        Reply::Status(529, error("overloaded_error", "Overloaded")),
        Reply::Status(
            400,
            error("invalid_request_error", "messages: roles must alternate"),
        ),
    ])
    .await;
    let provider = provider(&fake, "");
    let unauthorized = collect(&provider, request("x")).await.unwrap_err();
    assert!(
        matches!(&unauthorized, LlmError::InvalidCredentials(m) if m.contains("invalid x-api-key")),
        "{unauthorized:?}"
    );
    assert!(!format!("{unauthorized:?}").contains(KEY));
    assert!(!format!("{provider:?}").contains(KEY));
    assert!(matches!(
        collect(&provider, request("x")).await.unwrap_err(),
        LlmError::Unsupported(m) if m.contains("claude-nope")
    ));
    assert!(matches!(
        collect(&provider, request("x")).await.unwrap_err(),
        LlmError::Unavailable(m) if m.starts_with("速率限制或額度不足：")
    ));
    assert!(matches!(
        collect(&provider, request("x")).await.unwrap_err(),
        LlmError::Unavailable(m) if m.contains("Overloaded")
    ));
    assert!(matches!(
        collect(&provider, request("x")).await.unwrap_err(),
        LlmError::Unsupported(m) if m.contains("alternate")
    ));
}

#[tokio::test]
async fn cancel_stops_stream() {
    let fake = FakeApi::start([]).await;
    let provider = provider(&fake, "");
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = provider.stream(request("x"), cancel).await;
    assert!(matches!(result, Err(LlmError::Cancelled)));
}

#[tokio::test]
async fn list_models_reads_ids_and_display_names_across_pages() {
    let fake = FakeApi::start([
        Reply::Status(
            200,
            json!({"data": [
                {"type": "model", "id": "claude-b", "display_name": "Claude B", "created_at": "2025-01-01T00:00:00Z"},
            ], "has_more": true, "first_id": "claude-b", "last_id": "claude-b"}),
        ),
        Reply::Status(
            200,
            json!({"data": [{"type": "model", "id": "claude-a", "display_name": "Claude A"}],
                   "has_more": false, "first_id": "claude-a", "last_id": "claude-a"}),
        ),
    ])
    .await;
    let models = provider(&fake, "v1").list_models().await.unwrap();
    assert_eq!(
        models,
        vec![
            ModelInfo {
                slug: "claude-b".into(),
                display_name: "Claude B".into()
            },
            ModelInfo {
                slug: "claude-a".into(),
                display_name: "Claude A".into()
            },
        ]
    );
    let requests = fake.requests();
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/v1/models?limit=1000");
    assert_eq!(requests[1].path, "/v1/models?limit=1000&after_id=claude-b");
    assert_eq!(requests[0].header("x-api-key"), Some(KEY));
    assert_eq!(requests[0].header("anthropic-version"), Some("2023-06-01"));
}

fn ok_stream() -> Reply {
    Reply::Sse(stream(vec![
        vec![message_start(1)],
        text_block(0, &["ok"]),
        message_end("end_turn", 1).to_vec(),
    ]))
}

fn invalid_request(message: &str) -> Reply {
    Reply::Status(
        400,
        json!({"type": "error", "error": {"type": "invalid_request_error", "message": message}}),
    )
}

fn with_reasoning(model: &str, reasoning: Reasoning) -> ModelRequest {
    let mut request = request("嗨");
    request.model = model.into();
    request.reasoning = Some(reasoning);
    request
}

#[tokio::test]
async fn adaptive_rejected_falls_back_to_budget_per_model() {
    let fake = FakeApi::start([
        invalid_request("adaptive thinking is not supported on this model"),
        ok_stream(),
        ok_stream(),
        ok_stream(),
    ])
    .await;
    let provider = provider(&fake, "");
    for request in [
        with_reasoning("claude-old", Reasoning::High),
        with_reasoning("claude-old", Reasoning::Low),
        with_reasoning("claude-new", Reasoning::Medium),
    ] {
        let events = collect(&provider, request).await.unwrap();
        assert!(matches!(
            events.last(),
            Some(Ok(StreamEvent::Completed { .. }))
        ));
    }

    let requests = fake.requests();
    assert_eq!(requests.len(), 4);
    let first = &requests[0].body;
    assert_eq!(first["thinking"], json!({"type": "adaptive"}));
    assert_eq!(first["output_config"], json!({"effort": "high"}));
    assert_eq!(first["max_tokens"], 8192);
    // 重送：enabled + budget，不送 output_config，max_tokens 放得下 budget；歷史不變。
    let retry = &requests[1].body;
    assert_eq!(
        retry["thinking"],
        json!({"type": "enabled", "budget_tokens": 16384})
    );
    assert!(retry.get("output_config").is_none());
    assert_eq!(retry["max_tokens"], 16384 + 8192);
    assert_eq!(retry["messages"], first["messages"]);
    // 同一個模型之後直接用 budget。
    assert_eq!(
        requests[2].body["thinking"],
        json!({"type": "enabled", "budget_tokens": 2048})
    );
    // 其他模型不受影響。
    assert_eq!(requests[3].body["model"], "claude-new");
    assert_eq!(requests[3].body["thinking"], json!({"type": "adaptive"}));
    assert_eq!(
        requests[3].body["output_config"],
        json!({"effort": "medium"})
    );
}

#[tokio::test]
async fn disabled_rejected_omits_thinking() {
    let fake = FakeApi::start([
        invalid_request(
            "\"thinking.type.disabled\" is not supported for this model. Use \"thinking.type.adaptive\" and \"output_config.effort\" to control thinking behavior.",
        ),
        ok_stream(),
        ok_stream(),
        ok_stream(),
    ])
    .await;
    let provider = provider(&fake, "");
    collect(
        &provider,
        with_reasoning("claude-always-on", Reasoning::Off),
    )
    .await
    .unwrap();
    collect(
        &provider,
        with_reasoning("claude-always-on", Reasoning::Off),
    )
    .await
    .unwrap();
    // 記住的只有 disabled 的後備；其他強度照常送 adaptive。
    collect(
        &provider,
        with_reasoning("claude-always-on", Reasoning::Low),
    )
    .await
    .unwrap();

    let requests = fake.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[0].body["thinking"], json!({"type": "disabled"}));
    for sent in &requests[1..3] {
        assert!(sent.body.get("thinking").is_none(), "{}", sent.body);
        assert!(sent.body.get("output_config").is_none());
    }
    assert_eq!(requests[3].body["thinking"], json!({"type": "adaptive"}));
}

#[tokio::test]
async fn failed_fallback_is_not_remembered() {
    let rate_limited = Reply::Status(
        429,
        json!({"type": "error", "error": {"type": "rate_limit_error", "message": "slow down"}}),
    );
    let fake = FakeApi::start([
        invalid_request("adaptive thinking is not supported on this model"),
        rate_limited,
        ok_stream(),
        // 不相關的 400 不重送。
        invalid_request("messages: roles must alternate"),
    ])
    .await;
    let provider = provider(&fake, "");
    let error = collect(&provider, with_reasoning("claude-test", Reasoning::Low))
        .await
        .unwrap_err();
    assert!(matches!(error, LlmError::Unavailable(_)), "{error:?}");
    collect(&provider, with_reasoning("claude-test", Reasoning::Low))
        .await
        .unwrap();
    let error = collect(&provider, with_reasoning("claude-x", Reasoning::Low))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, LlmError::Unsupported(m) if m.contains("alternate")),
        "{error:?}"
    );

    let requests = fake.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(
        requests[1].body["thinking"],
        json!({"type": "enabled", "budget_tokens": 2048})
    );
    // 重送失敗沒有記住：下一次還是先試 adaptive。
    assert_eq!(requests[2].body["thinking"], json!({"type": "adaptive"}));
}

#[tokio::test]
async fn thinking_blocks_roundtrip_through_agent_loop() {
    let redacted = json!({"type": "redacted_thinking", "data": "EmwKAhgBEgy3va3pzix"});
    let fake = FakeApi::start([
        Reply::Sse(stream(vec![
            vec![
                message_start(10),
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "要查"}}),
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "時間"}}),
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "EqQBCgIYAh"}}),
                json!({"type": "content_block_stop", "index": 0}),
                json!({"type": "content_block_start", "index": 1, "content_block": redacted}),
                json!({"type": "content_block_stop", "index": 1}),
            ],
            text_block(2, &["我查一下"]),
            vec![
                json!({"type": "content_block_start", "index": 3, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "get_time", "input": {}}}),
                json!({"type": "content_block_delta", "index": 3, "delta": {"type": "input_json_delta", "partial_json": "{}"}}),
                json!({"type": "content_block_stop", "index": 3}),
            ],
            message_end("tool_use", 4).to_vec(),
        ])),
        Reply::Sse(stream(vec![
            vec![message_start(20)],
            text_block(0, &["12:00"]),
            message_end("end_turn", 2).to_vec(),
        ])),
    ])
    .await;
    let tool = FnTool::new("get_time", |_| Ok("12:00".into()));
    let config = RunConfig {
        reasoning: Some(Reasoning::Medium),
        provider: Arc::new(provider(&fake, "")),
        model: "claude-test".into(),
        instructions: None,
        tools: vec![tool.clone()],
        hook: Arc::new(AllowAll),
        max_turns: Some(4),
        session_id: None,
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let result = agent::run(&config, request("幾點").input, tx, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.outcome, RunOutcome::Done);
    let thinking = json!({"type": "thinking", "thinking": "要查時間", "signature": "EqQBCgIYAh"});
    let opaque = |value: &Value| Item::Opaque {
        provider: PROVIDER_NAME.into(),
        value: value.clone(),
    };
    assert_eq!(result.new_items[0], opaque(&thinking));
    assert_eq!(result.new_items[1], opaque(&redacted));

    // thinking 內容不會變成文字 delta。
    while let Ok(event) = rx.try_recv() {
        let debug = format!("{event:?}");
        assert!(
            !(debug.contains("TextDelta") && debug.contains("要查")),
            "{debug}"
        );
    }

    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    for sent in &requests {
        assert_eq!(sent.body["thinking"], json!({"type": "adaptive"}));
        assert_eq!(sent.body["output_config"], json!({"effort": "medium"}));
    }
    // 第二輪把 thinking block 原樣、依序放回 assistant message。
    assert_eq!(
        requests[1].body["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": "幾點"}]},
            {"role": "assistant", "content": [
                thinking,
                redacted,
                {"type": "text", "text": "我查一下"},
                {"type": "tool_use", "id": "toolu_1", "name": "get_time", "input": {}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "12:00"},
            ]},
        ])
    );
}

#[tokio::test]
async fn truncated_tool_call_is_not_executed() {
    let fake = FakeApi::start([Reply::Sse(stream(vec![
        vec![
            message_start(1),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "get_time", "input": {}}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"tz\": \"U"}}),
            json!({"type": "content_block_stop", "index": 0}),
        ],
        message_end("max_tokens", 8192).to_vec(),
    ]))])
    .await;
    let tool = FnTool::new("get_time", |_| Ok("12:00".into()));
    let config = RunConfig {
        reasoning: None,
        provider: Arc::new(provider(&fake, "")),
        model: "claude-test".into(),
        instructions: None,
        tools: vec![tool.clone()],
        hook: Arc::new(AllowAll),
        max_turns: Some(4),
        session_id: None,
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let error = agent::run(&config, request("幾點").input, tx, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(
        error,
        LlmError::Interrupted("輸出被截斷（達到 max tokens）".into())
    );
    assert!(tool.calls().is_empty());
}
