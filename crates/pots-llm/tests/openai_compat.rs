//! OpenAI 相容 Chat Completions provider 對假 server 的整合測試。

mod common;

use std::sync::Arc;

use common::Reply;
use common::api::FakeApi;
use futures::StreamExt;
use pots_llm::agent::{self, AllowAll, RunConfig, RunOutcome};
use pots_llm::openai_compat::{ModelInfo, OpenAiCompatProvider};
use pots_llm::reqwest::header::{HeaderMap, HeaderValue};
use pots_llm::testing::FnTool;
use pots_llm::{
    Item, LlmError, ModelProvider, ModelRequest, StreamEvent, ToolCall, ToolSpec, Usage,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const KEY: &str = "sk-test-secret";

fn provider(fake: &FakeApi, base: &str) -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(fake.url(base), KEY.into(), reqwest::Client::new())
}

fn request(text: &str) -> ModelRequest {
    ModelRequest {
        model: "gpt-test".into(),
        instructions: Some("你是 Pots".into()),
        input: vec![Item::User { text: text.into() }],
        tools: vec![],
        session_id: None,
    }
}

async fn collect(
    provider: &OpenAiCompatProvider,
    request: ModelRequest,
) -> Result<Vec<Result<StreamEvent, LlmError>>, LlmError> {
    let stream = provider.stream(request, CancellationToken::new()).await?;
    Ok(stream.collect().await)
}

fn chunks(values: &[Value], done: bool) -> String {
    let mut body: String = values
        .iter()
        .map(|value| format!("data: {value}\n\n"))
        .collect();
    if done {
        body.push_str("data: [DONE]\n\n");
    }
    body
}

fn text(delta: &str) -> Value {
    json!({"id": "chatcmpl-1", "object": "chat.completion.chunk",
           "choices": [{"index": 0, "delta": {"content": delta}, "finish_reason": null}]})
}

fn finish(reason: &str) -> Value {
    json!({"choices": [{"index": 0, "delta": {}, "finish_reason": reason}]})
}

fn usage(prompt: u64, completion: u64) -> Value {
    json!({"choices": [], "usage": {"prompt_tokens": prompt, "completion_tokens": completion, "total_tokens": prompt + completion}})
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
        Reply::Sse(chunks(&[text("好"), finish("stop"), usage(1, 1)], true)),
    ])
    .await;
    let provider = provider(&fake, "v1").with_headers(extra_headers());
    provider.list_models().await.unwrap();
    let mut request = request("嗨");
    request.session_id = Some("session-1".into());
    collect(&provider.with_session_header("x-opencode-session"), request)
        .await
        .unwrap();

    let requests = fake.requests();
    assert_eq!(requests[0].header("x-pots-test"), Some("1"));
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer sk-test-secret")
    );
    assert_eq!(requests[1].header("x-pots-test"), Some("1"));
    assert_eq!(requests[1].header("x-opencode-session"), Some("session-1"));
}

#[tokio::test]
async fn streams_text_until_done() {
    let fake = FakeApi::start([Reply::Sse(chunks(
        &[
            json!({"choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}}]}),
            text("你"),
            text("好"),
            finish("stop"),
            usage(5, 2),
        ],
        true,
    ))])
    .await;
    let events = collect(&provider(&fake, "v1"), request("嗨"))
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
    let sent = &requests[0];
    assert_eq!(sent.method, "POST");
    assert_eq!(sent.path, "/v1/chat/completions");
    assert_eq!(sent.header("authorization"), Some("Bearer sk-test-secret"));
    assert_eq!(
        sent.body,
        json!({
            "model": "gpt-test",
            "messages": [
                {"role": "system", "content": "你是 Pots"},
                {"role": "user", "content": "嗨"},
            ],
            "stream": true,
            "stream_options": {"include_usage": true},
        })
    );
    assert!(sent.body.get("tools").is_none());
}

#[tokio::test]
async fn base_url_with_trailing_slash_and_nested_path() {
    let ok = || Reply::Sse(chunks(&[text("ok"), finish("stop")], true));
    let fake = FakeApi::start([ok(), ok()]).await;
    for base in ["v1/", "api/v1"] {
        let events = collect(&provider(&fake, base), request("x")).await.unwrap();
        assert!(matches!(
            events.last(),
            Some(Ok(StreamEvent::Completed { .. }))
        ));
    }
    let paths: Vec<_> = fake.requests().into_iter().map(|r| r.path).collect();
    assert_eq!(
        paths,
        vec!["/v1/chat/completions", "/api/v1/chat/completions"]
    );
}

#[tokio::test]
async fn tool_roundtrip_through_agent_loop() {
    let fake = FakeApi::start([
        Reply::Sse(chunks(
            &[
                text("我查一下"),
                json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                    {"index": 0, "id": "call_1", "type": "function", "function": {"name": "get_time", "arguments": ""}}
                ]}}]}),
                json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                    {"index": 0, "function": {"arguments": "{\"tz\":"}}
                ]}}]}),
                json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                    {"index": 0, "function": {"arguments": "\"UTC\"}"}}
                ]}}]}),
                finish("tool_calls"),
                usage(10, 4),
            ],
            true,
        )),
        Reply::Sse(chunks(&[text("12:00"), finish("stop"), usage(20, 2)], true)),
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
        provider: Arc::new(provider(&fake, "v1")),
        model: "gpt-test".into(),
        instructions: Some("你是 Pots".into()),
        tools: vec![tool.clone()],
        hook: Arc::new(AllowAll),
        max_turns: 4,
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
    assert_eq!(tool.calls(), vec!["{\"tz\":\"UTC\"}".to_string()]);
    assert_eq!(
        result.new_items[1],
        Item::ToolCall(ToolCall {
            call_id: "call_1".into(),
            name: "get_time".into(),
            arguments: "{\"tz\":\"UTC\"}".into(),
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
    assert_eq!(
        requests[0].body["tools"],
        json!([{"type": "function", "function": {
            "name": "get_time",
            "description": "現在時間",
            "parameters": {"type": "object", "properties": {"tz": {"type": "string"}}},
        }}])
    );
    assert_eq!(
        requests[1].body["messages"],
        json!([
            {"role": "system", "content": "你是 Pots"},
            {"role": "user", "content": "幾點"},
            {"role": "assistant", "content": "我查一下", "tool_calls": [
                {"id": "call_1", "type": "function", "function": {"name": "get_time", "arguments": "{\"tz\":\"UTC\"}"}},
            ]},
            {"role": "tool", "tool_call_id": "call_1", "content": "12:00"},
        ])
    );
}

#[tokio::test]
async fn close_after_finish_reason_without_done_is_complete() {
    let fake = FakeApi::start([Reply::Sse(chunks(&[text("hi"), finish("stop")], false))]).await;
    let events = collect(&provider(&fake, "v1"), request("x")).await.unwrap();
    assert_eq!(
        events.last(),
        Some(&Ok(StreamEvent::Completed {
            usage: Usage::default()
        }))
    );
}

#[tokio::test]
async fn eof_without_finish_is_interrupted() {
    let fake = FakeApi::start([Reply::Sse(chunks(&[text("一半")], false))]).await;
    let events = collect(&provider(&fake, "v1"), request("x")).await.unwrap();
    assert_eq!(events.len(), 2);
    assert!(matches!(events[1], Err(LlmError::Interrupted(_))));
}

#[tokio::test]
async fn error_object_in_stream() {
    let fake = FakeApi::start([Reply::Sse(chunks(
        &[
            text("部分"),
            json!({"error": {"message": "upstream overloaded", "code": 502}}),
        ],
        true,
    ))])
    .await;
    let events = collect(&provider(&fake, "v1"), request("x")).await.unwrap();
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[1], Err(LlmError::Unavailable(m)) if m.contains("upstream")));
}

#[tokio::test]
async fn http_errors_are_classified_without_key() {
    let fake = FakeApi::start([
        Reply::Status(
            401,
            json!({"error": {"message": format!("Incorrect API key provided: {KEY}"), "type": "invalid_request_error", "code": "invalid_api_key"}}),
        ),
        Reply::Status(
            404,
            json!({"error": {"message": "The model `nope` does not exist", "code": "model_not_found"}}),
        ),
        Reply::Status(
            429,
            json!({"error": {"message": "You exceeded your current quota", "type": "insufficient_quota"}}),
        ),
        Reply::Status(429, json!({"error": {"message": "Rate limit reached"}})),
        Reply::Status(503, json!({"error": {"message": "busy"}})),
    ])
    .await;
    let provider = provider(&fake, "v1");
    let unauthorized = collect(&provider, request("x")).await.unwrap_err();
    assert!(
        matches!(&unauthorized, LlmError::InvalidCredentials(m) if m.contains("Incorrect API key")),
        "{unauthorized:?}"
    );
    assert!(!format!("{unauthorized:?} {unauthorized}").contains(KEY));
    assert!(matches!(
        collect(&provider, request("x")).await.unwrap_err(),
        LlmError::Unsupported(m) if m.contains("does not exist")
    ));
    assert!(matches!(
        collect(&provider, request("x")).await.unwrap_err(),
        LlmError::Other(m) if m.starts_with("API 額度不足：")
    ));
    assert!(matches!(
        collect(&provider, request("x")).await.unwrap_err(),
        LlmError::Unavailable(m) if m.starts_with("速率限制或額度不足：")
    ));
    assert!(matches!(
        collect(&provider, request("x")).await.unwrap_err(),
        LlmError::Unavailable(m) if m.contains("busy")
    ));
}

#[tokio::test]
async fn connection_error_is_unavailable_without_url() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    drop(listener);
    let provider =
        OpenAiCompatProvider::new(base.parse().unwrap(), KEY.into(), reqwest::Client::new());
    let error = collect(&provider, request("x")).await.unwrap_err();
    assert!(
        matches!(&error, LlmError::Unavailable(m) if !m.contains("127.0.0.1")),
        "{error:?}"
    );
}

#[tokio::test]
async fn cancel_stops_stream() {
    let fake = FakeApi::start([Reply::Sse(chunks(&[text("x")], false))]).await;
    let provider = provider(&fake, "v1");
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = provider.stream(request("x"), cancel).await;
    assert!(matches!(result, Err(LlmError::Cancelled)));
}

#[tokio::test]
async fn list_models_reads_data_ids() {
    let fake = FakeApi::start([Reply::Status(
        200,
        json!({"object": "list", "data": [
            {"id": "gpt-b", "object": "model", "owned_by": "openai"},
            {"id": "gpt-a", "object": "model", "owned_by": "openai"},
        ]}),
    )])
    .await;
    let models = provider(&fake, "v1/").list_models().await.unwrap();
    assert_eq!(
        models,
        vec![
            ModelInfo {
                slug: "gpt-b".into(),
                display_name: "gpt-b".into()
            },
            ModelInfo {
                slug: "gpt-a".into(),
                display_name: "gpt-a".into()
            },
        ]
    );
    let requests = fake.requests();
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/v1/models");
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer sk-test-secret")
    );
}

#[tokio::test]
async fn empty_key_sends_no_authorization() {
    let fake = FakeApi::start([Reply::Status(200, json!({"data": []}))]).await;
    let provider = OpenAiCompatProvider::new(fake.url("v1"), String::new(), reqwest::Client::new());
    assert!(provider.list_models().await.unwrap().is_empty());
    assert_eq!(fake.requests()[0].header("authorization"), None);
}

#[tokio::test]
async fn truncated_tool_call_is_not_executed() {
    let fake = FakeApi::start([Reply::Sse(chunks(
        &[
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "call_1", "type": "function", "function": {"name": "get_time", "arguments": "{\"tz\":"}}
            ]}}]}),
            finish("length"),
        ],
        true,
    ))])
    .await;
    let tool = FnTool::new("get_time", |_| Ok("12:00".into()));
    let config = RunConfig {
        provider: Arc::new(provider(&fake, "v1")),
        model: "gpt-test".into(),
        instructions: None,
        tools: vec![tool.clone()],
        hook: Arc::new(AllowAll),
        max_turns: 4,
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

/// 帶工具的請求被 Chat Completions 以「工具 + reasoning」拒絕。
fn tools_with_reasoning_rejected(model: &str) -> Reply {
    Reply::Status(
        400,
        json!({"error": {
            "message": format!(
                "Function tools with reasoning_effort are not supported for {model} in /v1/chat/completions. \
                 To use function tools, use /v1/responses or set reasoning_effort to 'none'."
            ),
            "type": "invalid_request_error",
            "param": "reasoning_effort",
            "code": "invalid_request_error",
        }}),
    )
}

fn request_with_tool(text: &str) -> ModelRequest {
    let mut request = request(text);
    request.tools = vec![ToolSpec {
        name: "get_time".into(),
        description: "現在時間".into(),
        parameters: json!({"type": "object", "properties": {}}),
    }];
    request
}

fn ok_stream() -> Reply {
    Reply::Sse(chunks(&[text("ok"), finish("stop"), usage(1, 1)], true))
}

#[tokio::test]
async fn tools_retry_with_reasoning_disabled() {
    let fake = FakeApi::start([
        tools_with_reasoning_rejected("gpt-6-luna"),
        ok_stream(),
        ok_stream(),
    ])
    .await;
    let provider = provider(&fake, "v1");
    let events = collect(&provider, request_with_tool("嗨")).await.unwrap();
    assert!(matches!(
        events.last(),
        Some(Ok(StreamEvent::Completed { .. }))
    ));
    // 同一個 provider 的下一輪直接關掉推理，不再先失敗一次。
    collect(&provider, request_with_tool("再一輪"))
        .await
        .unwrap();

    let requests = fake.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[0].body.get("reasoning_effort").is_none());
    for downgraded in &requests[1..] {
        assert_eq!(downgraded.body["reasoning_effort"], json!("none"));
        // 重送要保留工具與歷史。
        assert_eq!(downgraded.body["tools"], requests[0].body["tools"]);
    }
    assert_eq!(requests[1].body["messages"], requests[0].body["messages"]);
    assert_ne!(requests[2].body["messages"], requests[1].body["messages"]);
}

#[tokio::test]
async fn models_rejecting_none_point_at_responses_api() {
    let fake = FakeApi::start([
        tools_with_reasoning_rejected("gpt-6.1-sol"),
        Reply::Status(
            400,
            json!({"error": {
                "message": "Unsupported value: 'reasoning_effort' does not support 'none' with this model.",
                "type": "invalid_request_error",
                "param": "reasoning_effort",
            }}),
        ),
    ])
    .await;
    let error = collect(&provider(&fake, "v1"), request_with_tool("嗨"))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, LlmError::Unsupported(m) if m.contains("Responses API")),
        "{error:?}"
    );
    assert_eq!(fake.requests().len(), 2);
}
