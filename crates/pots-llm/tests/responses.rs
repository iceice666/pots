//! Responses provider 對假 SSE server 的整合測試。

mod common;

use std::sync::Arc;

use common::{FakeResponses, Reply, completed, item_done, message, sse, text_delta};
use futures::StreamExt;
use pots_llm::agent::{self, AgentEvent, AllowAll, RunConfig, RunOutcome};
use pots_llm::responses::{ModelInfo, ResponsesProvider};
use pots_llm::testing::{FnTool, StaticTokenSource};
use pots_llm::{
    Item, LlmError, ModelProvider, ModelRequest, StreamEvent, ToolCall, ToolSpec, Usage,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn provider(fake: &FakeResponses, tokens: Arc<StaticTokenSource>) -> ResponsesProvider {
    ResponsesProvider::new(fake.base.clone(), tokens, reqwest::Client::new())
}

fn request(text: &str) -> ModelRequest {
    ModelRequest {
        model: "gpt-test".into(),
        instructions: Some("你是 Pots".into()),
        input: vec![Item::User { text: text.into() }],
        tools: vec![],
    }
}

async fn collect(
    provider: &ResponsesProvider,
    request: ModelRequest,
) -> Result<Vec<Result<StreamEvent, LlmError>>, LlmError> {
    let stream = provider.stream(request, CancellationToken::new()).await?;
    Ok(stream.collect().await)
}

/// SIWC 不接受的請求欄位。
const FORBIDDEN_KEYS: &[&str] = &[
    "previous_response_id",
    "temperature",
    "top_p",
    "max_output_tokens",
    "max_tool_calls",
    "metadata",
    "background",
    "conversation",
    "prompt",
    "prompt_cache_retention",
    "safety_identifier",
    "top_logprobs",
    "truncation",
    "user",
    "moderation",
    "service_tier",
];

fn assert_siwc_body(body: &Value) {
    assert_eq!(body["store"], json!(false));
    assert_eq!(body["stream"], json!(true));
    let object = body.as_object().unwrap();
    for key in FORBIDDEN_KEYS {
        assert!(!object.contains_key(*key), "不該送 {key}");
    }
    for item in body["input"].as_array().unwrap() {
        assert_ne!(item["role"], "system", "不該有 system item");
    }
    for tool in body["tools"].as_array().into_iter().flatten() {
        assert_eq!(tool["type"], "namespace", "工具要放在 namespace 裡");
    }
}

#[tokio::test]
async fn streams_text_until_completed() {
    let fake = FakeResponses::start([Reply::Sse(sse(&[
        json!({"type": "response.created", "response": {"id": "resp_1"}}),
        text_delta("你"),
        text_delta("好"),
        message("你好"),
        completed(5, 2),
    ]))])
    .await;
    let tokens = StaticTokenSource::new(["t1"]);
    let events = collect(&provider(&fake, tokens), request("嗨"))
        .await
        .unwrap();
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
    assert_eq!(requests[0].authorization.as_deref(), Some("Bearer t1"));
    let body = &requests[0].body;
    assert_siwc_body(body);
    assert_eq!(body["model"], "gpt-test");
    assert_eq!(body["instructions"], "你是 Pots");
    assert_eq!(body["input"][0]["role"], "user");
}

#[tokio::test]
async fn namespaced_tool_roundtrip_through_agent_loop() {
    let reasoning =
        json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "enc"});
    let fake = FakeResponses::start([
        Reply::Sse(sse(&[
            item_done(reasoning.clone()),
            item_done(json!({
                "type": "function_call", "id": "fc_1", "namespace": "pots",
                "name": "get_time", "call_id": "call_1", "arguments": "{\"tz\":\"UTC\"}"
            })),
            completed(10, 4),
        ])),
        Reply::Sse(sse(&[
            text_delta("12:00"),
            message("12:00"),
            completed(20, 2),
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
    let provider = Arc::new(provider(&fake, StaticTokenSource::new(["t1"])));
    let config = RunConfig {
        provider,
        model: "gpt-test".into(),
        instructions: Some("你是 Pots".into()),
        tools: vec![tool.clone()],
        hook: Arc::new(AllowAll),
        max_turns: 4,
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
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
    assert_eq!(tool.calls(), vec!["{\"tz\":\"UTC\"}".to_string()]);
    assert_eq!(
        result.new_items[1],
        Item::ToolCall(ToolCall {
            call_id: "call_1".into(),
            name: "get_time".into(),
            arguments: "{\"tz\":\"UTC\"}".into(),
        })
    );
    assert_eq!(result.usage.input_tokens, 30);
    let mut saw_delta = false;
    while let Ok(event) = rx.try_recv() {
        saw_delta |= event == AgentEvent::TextDelta("12:00".into());
    }
    assert!(saw_delta);

    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    let first = &requests[0].body;
    assert_siwc_body(first);
    assert_eq!(first["tools"][0]["name"], "pots");
    assert_eq!(first["tools"][0]["tools"][0]["name"], "get_time");
    assert_eq!(first["include"], json!(["reasoning.encrypted_content"]));

    let second = &requests[1].body;
    assert_siwc_body(second);
    let input = second["input"].as_array().unwrap();
    assert_eq!(input.len(), 4);
    // reasoning 原樣回送（拿掉 id）。
    assert_eq!(
        input[1],
        json!({"type": "reasoning", "summary": [], "encrypted_content": "enc"})
    );
    assert_eq!(
        input[2],
        json!({"type": "function_call", "namespace": "pots", "name": "get_time",
               "call_id": "call_1", "arguments": "{\"tz\":\"UTC\"}"})
    );
    assert_eq!(
        input[3],
        json!({"type": "function_call_output", "call_id": "call_1", "output": "12:00"})
    );
}

#[tokio::test]
async fn late_usage_limit_failure() {
    let fake = FakeResponses::start([Reply::Sse(sse(&[
        text_delta("部分"),
        json!({"type": "response.failed", "response": {"error": {
            "code": "subscription_sharing_usage_limit_exceeded", "message": "limit"
        }}}),
        completed(1, 1),
    ]))])
    .await;
    let events = collect(
        &provider(&fake, StaticTokenSource::new(["t1"])),
        request("x"),
    )
    .await
    .unwrap();
    assert_eq!(
        events,
        vec![
            Ok(StreamEvent::TextDelta("部分".into())),
            Err(LlmError::UsageLimit)
        ]
    );
}

#[tokio::test]
async fn eof_without_completed_is_interrupted() {
    let fake = FakeResponses::start([Reply::Sse(sse(&[text_delta("一半")]))]).await;
    let events = collect(
        &provider(&fake, StaticTokenSource::new(["t1"])),
        request("x"),
    )
    .await
    .unwrap();
    assert_eq!(events.len(), 2);
    assert!(matches!(events[1], Err(LlmError::Interrupted(_))));
}

#[tokio::test]
async fn unauthorized_refreshes_once_then_retries() {
    let fake = FakeResponses::start([
        Reply::Status(
            401,
            json!({"error": {"code": "invalid_token", "message": "expired"}}),
        ),
        Reply::Sse(sse(&[message("ok"), completed(1, 1)])),
    ])
    .await;
    let tokens = StaticTokenSource::new(["t1", "t2"]);
    let events = collect(&provider(&fake, tokens.clone()), request("x"))
        .await
        .unwrap();
    assert!(matches!(
        events.last(),
        Some(Ok(StreamEvent::Completed { .. }))
    ));
    assert_eq!(tokens.rejected_count(), 1);
    let auth: Vec<_> = fake
        .requests()
        .into_iter()
        .map(|r| r.authorization.unwrap())
        .collect();
    assert_eq!(auth, vec!["Bearer t1", "Bearer t2"]);
}

#[tokio::test]
async fn second_unauthorized_needs_login() {
    let fake = FakeResponses::start([
        Reply::Status(401, json!({"detail": "nope"})),
        Reply::Status(
            401,
            json!({"error": {"code": "subscription_sharing_invalid_user"}}),
        ),
    ])
    .await;
    let tokens = StaticTokenSource::new(["t1", "t2"]);
    let error = collect(&provider(&fake, tokens.clone()), request("x"))
        .await
        .unwrap_err();
    assert_eq!(error, LlmError::NeedsLogin);
    assert_eq!(tokens.rejected_count(), 1);
    assert_eq!(fake.requests().len(), 2);
}

#[tokio::test]
async fn admission_errors_are_classified() {
    let fake = FakeResponses::start([
        Reply::Status(429, json!({"error": {"code": "subscription_sharing_usage_limit_exceeded"}})),
        Reply::Status(403, json!({"error": {"code": "subscription_sharing_user_not_eligible", "message": "no"}})),
        Reply::Status(503, json!({"detail": "routing unavailable"})),
        Reply::Status(400, json!({"error": {"code": "subscription_sharing_unsupported_capability", "param": "tools"}})),
    ])
    .await;
    let provider = provider(&fake, StaticTokenSource::new(["t1"]));
    assert_eq!(
        collect(&provider, request("x")).await.unwrap_err(),
        LlmError::UsageLimit
    );
    let not_entitled = collect(&provider, request("x")).await.unwrap_err();
    assert!(matches!(not_entitled, LlmError::NotEntitled(m) if m.contains("req_test")));
    assert!(matches!(
        collect(&provider, request("x")).await.unwrap_err(),
        LlmError::Unavailable(_)
    ));
    assert!(matches!(
        collect(&provider, request("x")).await.unwrap_err(),
        LlmError::Unsupported(m) if m.contains("tools")
    ));
}

#[tokio::test]
async fn cancel_stops_stream() {
    let fake = FakeResponses::start([]).await;
    let provider = provider(&fake, StaticTokenSource::new(["t1"]));
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = provider.stream(request("x"), cancel).await;
    assert!(matches!(result, Err(LlmError::Cancelled)));
}

#[tokio::test]
async fn list_models_filters_visibility_in_order() {
    let fake = FakeResponses::start([]).await;
    fake.set_models(json!({"models": [
        {"slug": "gpt-b", "display_name": "GPT B", "visibility": "list"},
        {"slug": "gpt-hidden", "display_name": "Hidden", "visibility": "hide"},
        {"slug": "gpt-a", "display_name": "GPT A", "visibility": "list"},
    ]}));
    let models = provider(&fake, StaticTokenSource::new(["t1"]))
        .list_models()
        .await
        .unwrap();
    assert_eq!(
        models,
        vec![
            ModelInfo {
                slug: "gpt-b".into(),
                display_name: "GPT B".into()
            },
            ModelInfo {
                slug: "gpt-a".into(),
                display_name: "GPT A".into()
            },
        ]
    );
    assert_eq!(
        fake.requests()[0].authorization.as_deref(),
        Some("Bearer t1")
    );
}
