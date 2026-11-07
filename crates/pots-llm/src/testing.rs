//! 給測試用的假 provider、工具與 token 來源（cargo feature `test-util`）。
//!
//! ```ignore
//! use pots_llm::testing::{FnTool, ScriptedProvider, ScriptedTurn};
//!
//! let provider = ScriptedProvider::new([
//!     // 第 1 輪：模型要求呼叫工具（Item::ToolCall 的 name 不含 namespace）。
//!     ScriptedTurn::new().tool_call("call_1", "get_time", "{}"),
//!     // 第 2 輪：模型回文字，loop 以 RunOutcome::Done 結束。
//!     ScriptedTurn::reply("現在是中午"),
//! ]);
//! // ... agent::run(&RunConfig { provider: provider.clone(), .. }, ..)
//! assert_eq!(provider.requests().len(), 2);
//! ```

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::agent::{Tool, ToolError};
use crate::provider::{
    EventStream, Item, LlmError, ModelProvider, ModelRequest, StreamEvent, ToolCall, ToolSpec,
    Usage,
};
use crate::siwc::{AccessToken, TokenSource};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 依腳本回應的 provider。每次 [`ModelProvider::stream`] 取出佇列最前面的一輪，
/// 並記錄收到的 [`ModelRequest`]。腳本用完時回 [`LlmError::Other`]。
pub struct ScriptedProvider {
    name: String,
    turns: Mutex<VecDeque<ScriptedTurn>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl ScriptedProvider {
    /// 名稱預設 `scripted`。
    pub fn new(turns: impl IntoIterator<Item = ScriptedTurn>) -> Arc<Self> {
        Self::named("scripted", turns)
    }

    /// 自訂 [`ModelProvider::name`]，例如要模擬 `openai-responses` 的 [`Item::Opaque`]。
    pub fn named(
        name: impl Into<String>,
        turns: impl IntoIterator<Item = ScriptedTurn>,
    ) -> Arc<Self> {
        Arc::new(Self {
            name: name.into(),
            turns: Mutex::new(turns.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        })
    }

    /// 在腳本最後再加一輪。
    pub fn push(&self, turn: ScriptedTurn) {
        lock(&self.turns).push_back(turn);
    }

    /// 到目前為止收到的請求，依呼叫順序。
    pub fn requests(&self) -> Vec<ModelRequest> {
        lock(&self.requests).clone()
    }

    /// 還沒用掉的輪數。
    pub fn remaining(&self) -> usize {
        lock(&self.turns).len()
    }
}

#[async_trait::async_trait]
impl ModelProvider for ScriptedProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, LlmError> {
        lock(&self.requests).push(request);
        let turn = lock(&self.turns)
            .pop_front()
            .ok_or_else(|| LlmError::Other("ScriptedProvider：腳本已用完".into()))?;
        if let Some(error) = turn.reject {
            return Err(error);
        }
        let mut steps: VecDeque<Step> = turn.steps.into();
        match turn.end {
            End::Completed(usage) => steps.push_back(Step::Event(StreamEvent::Completed { usage })),
            End::Error(error) => steps.push_back(Step::Error(error)),
            End::Eof => {}
        }
        Ok(Box::pin(futures::stream::unfold(
            (steps, cancel),
            |(mut steps, cancel)| async move {
                loop {
                    if cancel.is_cancelled() && !steps.is_empty() {
                        steps.clear();
                        return Some((Err(LlmError::Cancelled), (steps, cancel)));
                    }
                    match steps.pop_front()? {
                        Step::Event(event) => return Some((Ok(event), (steps, cancel))),
                        Step::Error(error) => {
                            steps.clear();
                            return Some((Err(error), (steps, cancel)));
                        }
                        Step::Delay(duration) => {
                            tokio::select! {
                                () = tokio::time::sleep(duration) => {}
                                () = cancel.cancelled() => {}
                            }
                        }
                        Step::Hang => {
                            cancel.cancelled().await;
                            steps.push_front(Step::Hang);
                        }
                    }
                }
            },
        )))
    }
}

/// 腳本裡的一輪模型串流。預設以 [`StreamEvent::Completed`] 正常結束。
#[derive(Clone, Debug)]
pub struct ScriptedTurn {
    reject: Option<LlmError>,
    steps: Vec<Step>,
    end: End,
}

#[derive(Clone, Debug)]
enum Step {
    Event(StreamEvent),
    Error(LlmError),
    Delay(Duration),
    /// 一直等到取消。
    Hang,
}

#[derive(Clone, Debug)]
enum End {
    Completed(Usage),
    Error(LlmError),
    Eof,
}

impl Default for ScriptedTurn {
    fn default() -> Self {
        Self::new()
    }
}

impl ScriptedTurn {
    pub fn new() -> Self {
        Self {
            reject: None,
            steps: Vec::new(),
            end: End::Completed(Usage::default()),
        }
    }

    /// 只回一段文字的一輪。
    pub fn reply(text: impl Into<String>) -> Self {
        Self::new().text(text)
    }

    /// [`ModelProvider::stream`] 本身直接失敗（例如還沒開始串流就 401、額度不足）。
    pub fn reject(error: LlmError) -> Self {
        Self {
            reject: Some(error),
            ..Self::new()
        }
    }

    /// 一個文字增量加上對應的 [`Item::Assistant`]。
    pub fn text(self, text: impl Into<String>) -> Self {
        let text = text.into();
        self.text_deltas([text])
    }

    /// 多個文字增量，最後送一個串起來的 [`Item::Assistant`]。
    pub fn text_deltas<S: Into<String>>(mut self, deltas: impl IntoIterator<Item = S>) -> Self {
        let mut full = String::new();
        for delta in deltas {
            let delta = delta.into();
            full.push_str(&delta);
            self.steps.push(Step::Event(StreamEvent::TextDelta(delta)));
        }
        self.item(Item::Assistant { text: full })
    }

    /// 模型要求呼叫工具。`name` 不含 namespace。
    pub fn tool_call(
        self,
        call_id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        self.item(Item::ToolCall(ToolCall {
            call_id: call_id.into(),
            name: name.into(),
            arguments: arguments.into(),
        }))
    }

    /// 送出一個完整的輸出項目。
    pub fn item(self, item: Item) -> Self {
        self.event(StreamEvent::ItemDone(item))
    }

    /// 送出任意串流事件。
    pub fn event(mut self, event: StreamEvent) -> Self {
        self.steps.push(Step::Event(event));
        self
    }

    /// 暫停一段時間再送下一個事件（取消時提早結束暫停）。
    pub fn delay(mut self, duration: Duration) -> Self {
        self.steps.push(Step::Delay(duration));
        self
    }

    /// 停在這裡直到取消，然後回 [`LlmError::Cancelled`]。
    pub fn hang(mut self) -> Self {
        self.steps.push(Step::Hang);
        self
    }

    /// 正常完成時回報的 usage。
    pub fn usage(mut self, input_tokens: u64, output_tokens: u64) -> Self {
        self.end = End::Completed(Usage {
            input_tokens,
            output_tokens,
        });
        self
    }

    /// 串流中途以錯誤結束（例如 `response.failed`）。
    pub fn fail(mut self, error: LlmError) -> Self {
        self.end = End::Error(error);
        self
    }

    /// 串流在完成事件之前直接結束（EOF）；loop 應回 [`LlmError::Interrupted`]。
    pub fn eof(mut self) -> Self {
        self.end = End::Eof;
        self
    }
}

type ToolFn = dyn Fn(&str) -> Result<String, ToolError> + Send + Sync;

/// 用閉包實作的工具，並記錄收到的參數。
pub struct FnTool {
    spec: ToolSpec,
    handler: Box<ToolFn>,
    calls: Mutex<Vec<String>>,
}

impl FnTool {
    /// 參數 schema 預設為空 object。
    pub fn new(
        name: impl Into<String>,
        handler: impl Fn(&str) -> Result<String, ToolError> + Send + Sync + 'static,
    ) -> Arc<Self> {
        let name = name.into();
        Self::with_spec(
            ToolSpec {
                description: format!("測試工具 {name}"),
                name,
                parameters: serde_json::json!({ "type": "object", "properties": {} }),
            },
            handler,
        )
    }

    pub fn with_spec(
        spec: ToolSpec,
        handler: impl Fn(&str) -> Result<String, ToolError> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            spec,
            handler: Box::new(handler),
            calls: Mutex::new(Vec::new()),
        })
    }

    /// 每次呼叫收到的 `arguments`，依呼叫順序。
    pub fn calls(&self) -> Vec<String> {
        lock(&self.calls).clone()
    }
}

#[async_trait::async_trait]
impl Tool for FnTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn call(&self, arguments: &str) -> Result<String, ToolError> {
        lock(&self.calls).push(arguments.to_owned());
        (self.handler)(arguments)
    }
}

/// 固定的 token 序列。[`TokenSource::token_rejected`] 換到下一個；用完時回
/// [`LlmError::NeedsLogin`]，模擬「強制 refresh 也救不回來」。
pub struct StaticTokenSource {
    tokens: Mutex<VecDeque<String>>,
    rejected: Mutex<usize>,
}

impl StaticTokenSource {
    pub fn new<S: Into<String>>(tokens: impl IntoIterator<Item = S>) -> Arc<Self> {
        Arc::new(Self {
            tokens: Mutex::new(tokens.into_iter().map(Into::into).collect()),
            rejected: Mutex::new(0),
        })
    }

    /// [`TokenSource::token_rejected`] 被呼叫的次數。
    pub fn rejected_count(&self) -> usize {
        *lock(&self.rejected)
    }
}

#[async_trait::async_trait]
impl TokenSource for StaticTokenSource {
    async fn access_token(&self) -> Result<AccessToken, LlmError> {
        lock(&self.tokens)
            .front()
            .map(|token| AccessToken(token.clone()))
            .ok_or(LlmError::NeedsLogin)
    }

    async fn token_rejected(&self, rejected: &AccessToken) -> Result<AccessToken, LlmError> {
        *lock(&self.rejected) += 1;
        let mut tokens = lock(&self.tokens);
        if tokens.front() == Some(&rejected.0) {
            tokens.pop_front();
        }
        tokens
            .front()
            .map(|token| AccessToken(token.clone()))
            .ok_or(LlmError::NeedsLogin)
    }
}
