//! 所有 agent profile 共用的多輪工具迴圈（`proposal/agent-collaboration.md`）。
//!
//! 一輪 = 一次模型串流。模型要求呼叫工具時，[`Hook`] 先把關，再執行工具，把結果
//! 放回歷史後開始下一輪；模型只回文字就結束。預算、授權、Suspend 類工具等「不能
//! 靠 prompt」的規則都由 [`Hook`] 決定，loop 本身不知道 profile。

use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::provider::{
    Item, LlmError, ModelProvider, ModelRequest, StreamEvent, ToolCall, ToolSpec, Usage,
};

/// 平台工具。
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;

    /// `arguments` 是模型給的 JSON 字串，工具自己解析與驗證。
    /// 回傳的字串原樣送回模型；錯誤也會轉成文字送回，讓模型自己修正。
    async fn call(&self, arguments: &str) -> Result<String, ToolError>;
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ToolError(pub String);

/// Hook 對一次工具呼叫的決定。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolDecision {
    Allow,
    /// 不執行，把理由當作工具結果送回模型。
    Deny(String),
    /// 執行後結束本輪（Suspend 類工具，例如 `wait_for`）。
    AllowThenStop,
    /// 不執行並結束本輪（Effect 類工具改成核准請求）；字串當作工具結果寫進歷史。
    Stop(String),
}

/// 平台的 gate。預設全部放行。
#[async_trait::async_trait]
pub trait Hook: Send + Sync {
    /// 每輪開始前呼叫；回傳 `false` 時乾淨結束（預算用完、steer 等）。
    async fn before_turn(&self, _turn: usize) -> bool {
        true
    }

    async fn before_tool(&self, _call: &ToolCall) -> ToolDecision {
        ToolDecision::Allow
    }

    async fn after_tool(&self, _call: &ToolCall, _output: &str) {}

    /// 一輪模型呼叫完成後、處理工具之前呼叫；`usage` 是這一輪的用量。
    /// 預算與 trace 在這裡同步記帳，下一輪的 [`Hook::before_turn`] 才看得到。
    async fn after_turn(&self, _turn: usize, _usage: Usage) {}
}

/// 不做任何限制的 hook。
pub struct AllowAll;

#[async_trait::async_trait]
impl Hook for AllowAll {}

/// loop 進行中送出的事件，給伺服器轉成 UI 串流與持久化。
#[derive(Clone, Debug, PartialEq)]
pub enum AgentEvent {
    /// 模型的文字增量。
    TextDelta(String),
    /// 新的歷史項目（模型輸出或工具結果），依歷史順序送出；伺服器據此逐筆持久化。
    Item(Item),
    ToolStarted(ToolCall),
    ToolFinished {
        call: ToolCall,
        ok: bool,
    },
    TurnCompleted {
        turn: usize,
        usage: Usage,
    },
}

pub struct RunConfig {
    pub provider: Arc<dyn ModelProvider>,
    pub model: String,
    pub instructions: Option<String>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub hook: Arc<dyn Hook>,
    pub max_turns: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    /// 模型回了文字、沒有再呼叫工具。
    Done,
    /// `before_turn` 回 `false`。
    Stopped,
    /// 工具的 [`ToolDecision::AllowThenStop`] 或 [`ToolDecision::Stop`]。
    Suspended,
    /// 用完 `max_turns`。
    MaxTurns,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunResult {
    pub outcome: RunOutcome,
    /// 這次 run 新增的歷史（不含傳入的 `history`）。
    pub new_items: Vec<Item>,
    pub usage: Usage,
}

/// 工具被略過時寫進歷史的結果：同一輪前面的工具已經要求結束本輪。
pub const SKIPPED_AFTER_STOP: &str = "未執行：本輪已由前一個工具結束";
/// 工具因為 run 被取消而沒有結果時寫進歷史的結果。
pub const CANCELLED_OUTPUT: &str = "未完成：執行已取消";

/// 執行一次 agent run。`history` 是完整的既有歷史（最後一筆通常是觸發的 [`Item::User`]）。
///
/// 每一輪：
///
/// 1. [`Hook::before_turn`]（`turn` 從 0 開始）回 `false` 就以 [`RunOutcome::Stopped`] 結束。
/// 2. 串流一次模型。文字增量即時以 [`AgentEvent::TextDelta`] 送出；輸出項目先暫存，
///    收到 [`StreamEvent::Completed`] 之後才依序以 [`AgentEvent::Item`] 送出並加入歷史，
///    接著送 [`AgentEvent::TurnCompleted`]。串流失敗或中斷時這一輪的項目全部丟棄，
///    歷史裡不會留下沒有結果的工具呼叫。
/// 3. 沒有工具呼叫就以 [`RunOutcome::Done`] 結束；否則依模型順序逐一問 [`Hook::before_tool`]、
///    執行工具，並把 [`Item::ToolResult`] 加入歷史。工具的錯誤轉成文字送回模型。
///    不認得的工具名稱直接回錯誤結果，不經過 hook。
/// 4. [`ToolDecision::AllowThenStop`]／[`ToolDecision::Stop`] 之後，同一輪剩下的工具呼叫
///    不執行，各自得到 [`SKIPPED_AFTER_STOP`] 結果（Responses 要求每個呼叫都有結果），
///    然後以 [`RunOutcome::Suspended`] 結束。
///
/// 跑完 `max_turns` 輪仍在呼叫工具就回 [`RunOutcome::MaxTurns`]。
///
/// 取消在每個 await 點都會生效，回 [`LlmError::Cancelled`]；已經透過 [`AgentEvent::Item`]
/// 送出的項目仍然有效。工具階段被取消時，還沒有結果的工具呼叫會先補上
/// [`CANCELLED_OUTPUT`] 結果再回傳，讓持久化的歷史保持完整。
pub async fn run(
    config: &RunConfig,
    history: Vec<Item>,
    events: mpsc::UnboundedSender<AgentEvent>,
    cancel: CancellationToken,
) -> Result<RunResult, LlmError> {
    let specs: Vec<ToolSpec> = config.tools.iter().map(|tool| tool.spec()).collect();
    let tools: HashMap<String, Arc<dyn Tool>> = specs
        .iter()
        .zip(&config.tools)
        .map(|(spec, tool)| (spec.name.clone(), Arc::clone(tool)))
        .collect();
    let mut state = RunState {
        history,
        new_items: Vec::new(),
        usage: Usage::default(),
        events,
    };

    for turn in 0..config.max_turns {
        if !cancellable(&cancel, config.hook.before_turn(turn)).await? {
            return Ok(state.finish(RunOutcome::Stopped));
        }
        let request = ModelRequest {
            model: config.model.clone(),
            instructions: config.instructions.clone(),
            input: state.history.clone(),
            tools: specs.clone(),
        };
        let (items, usage) =
            stream_turn(config.provider.as_ref(), request, &state.events, &cancel).await?;
        state.usage += usage;
        config.hook.after_turn(turn, usage).await;
        let calls: Vec<ToolCall> = items
            .iter()
            .filter_map(|item| match item {
                Item::ToolCall(call) => Some(call.clone()),
                _ => None,
            })
            .collect();
        for item in items {
            state.commit(item);
        }
        state.emit(AgentEvent::TurnCompleted { turn, usage });

        if calls.is_empty() {
            return Ok(state.finish(RunOutcome::Done));
        }
        if run_tools(config, &tools, &calls, &mut state, &cancel).await? {
            return Ok(state.finish(RunOutcome::Suspended));
        }
    }
    Ok(state.finish(RunOutcome::MaxTurns))
}

/// 一次 run 累積的狀態。
struct RunState {
    /// 送給模型的完整歷史（傳入的 + 這次新增的）。
    history: Vec<Item>,
    new_items: Vec<Item>,
    usage: Usage,
    events: mpsc::UnboundedSender<AgentEvent>,
}

impl RunState {
    /// 接收端關掉時事件直接丟掉；loop 本身不依賴有人在聽。
    fn emit(&self, event: AgentEvent) {
        let _ = self.events.send(event);
    }

    /// 把項目加入歷史並通知伺服器持久化。
    fn commit(&mut self, item: Item) {
        self.history.push(item.clone());
        self.new_items.push(item.clone());
        self.emit(AgentEvent::Item(item));
    }

    fn finish(self, outcome: RunOutcome) -> RunResult {
        RunResult {
            outcome,
            new_items: self.new_items,
            usage: self.usage,
        }
    }
}

/// 等 `future` 完成，取消時回 [`LlmError::Cancelled`]。已經取消就不開始等。
async fn cancellable<F: Future>(
    cancel: &CancellationToken,
    future: F,
) -> Result<F::Output, LlmError> {
    if cancel.is_cancelled() {
        return Err(LlmError::Cancelled);
    }
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(LlmError::Cancelled),
        output = future => Ok(output),
    }
}

/// 串流一輪，回傳依序完成的輸出項目與 usage。只有收到 `Completed` 才算成功。
async fn stream_turn(
    provider: &dyn ModelProvider,
    request: ModelRequest,
    events: &mpsc::UnboundedSender<AgentEvent>,
    cancel: &CancellationToken,
) -> Result<(Vec<Item>, Usage), LlmError> {
    let mut stream = cancellable(cancel, provider.stream(request, cancel.clone())).await??;
    let mut items = Vec::new();
    loop {
        match cancellable(cancel, stream.next()).await? {
            Some(Ok(StreamEvent::TextDelta(delta))) => {
                let _ = events.send(AgentEvent::TextDelta(delta));
            }
            Some(Ok(StreamEvent::ItemDone(item))) => items.push(item),
            Some(Ok(StreamEvent::Completed { usage })) => return Ok((items, usage)),
            Some(Err(error)) => return Err(error),
            None => {
                return Err(LlmError::Interrupted("串流在完成事件之前結束".into()));
            }
        }
    }
}

/// 依序處理一輪的工具呼叫。回傳 `true` 表示要結束本輪（Suspended）。
async fn run_tools(
    config: &RunConfig,
    tools: &HashMap<String, Arc<dyn Tool>>,
    calls: &[ToolCall],
    state: &mut RunState,
    cancel: &CancellationToken,
) -> Result<bool, LlmError> {
    let mut stop = false;
    for (index, call) in calls.iter().enumerate() {
        if stop {
            state.commit(tool_result(call, SKIPPED_AFTER_STOP));
            continue;
        }
        let outcome = handle_call(config, tools, call, state, cancel).await;
        match outcome {
            Ok((output, suspend)) => {
                state.commit(tool_result(call, &output));
                stop = suspend;
            }
            Err(Cancelled { output }) => {
                // 取消：補齊還沒有結果的呼叫，歷史才不會留下懸空的工具呼叫。
                // 工具已經跑完時用真正的結果。
                state.commit(tool_result(
                    call,
                    output.as_deref().unwrap_or(CANCELLED_OUTPUT),
                ));
                for pending in &calls[index + 1..] {
                    state.commit(tool_result(pending, CANCELLED_OUTPUT));
                }
                return Err(LlmError::Cancelled);
            }
        }
    }
    Ok(stop)
}

/// [`handle_call`] 被取消。`output` 是工具已經跑完、但之後的 hook 被取消時的結果。
struct Cancelled {
    output: Option<String>,
}

impl From<LlmError> for Cancelled {
    fn from(_: LlmError) -> Self {
        Self { output: None }
    }
}

/// 處理單一工具呼叫，回傳（要送回模型的結果，是否結束本輪）。只會因取消而失敗。
async fn handle_call(
    config: &RunConfig,
    tools: &HashMap<String, Arc<dyn Tool>>,
    call: &ToolCall,
    state: &RunState,
    cancel: &CancellationToken,
) -> Result<(String, bool), Cancelled> {
    let Some(tool) = tools.get(&call.name) else {
        return Ok((format!("錯誤：沒有名為 `{}` 的工具", call.name), false));
    };
    let suspend = match cancellable(cancel, config.hook.before_tool(call)).await? {
        ToolDecision::Allow => false,
        ToolDecision::AllowThenStop => true,
        ToolDecision::Deny(reason) => return Ok((reason, false)),
        ToolDecision::Stop(output) => return Ok((output, true)),
    };

    state.emit(AgentEvent::ToolStarted(call.clone()));
    let result = match cancellable(cancel, tool.call(&call.arguments)).await {
        Ok(result) => result,
        Err(cancelled) => {
            state.emit(AgentEvent::ToolFinished {
                call: call.clone(),
                ok: false,
            });
            return Err(cancelled.into());
        }
    };
    let ok = result.is_ok();
    let output = result.unwrap_or_else(|error| format!("錯誤：{error}"));
    state.emit(AgentEvent::ToolFinished {
        call: call.clone(),
        ok,
    });
    if cancellable(cancel, config.hook.after_tool(call, &output))
        .await
        .is_err()
    {
        return Err(Cancelled {
            output: Some(output),
        });
    }
    Ok((output, suspend))
}

#[cfg(test)]
mod tests;

fn tool_result(call: &ToolCall, output: &str) -> Item {
    Item::ToolResult {
        call_id: call.call_id.clone(),
        output: output.to_owned(),
    }
}
