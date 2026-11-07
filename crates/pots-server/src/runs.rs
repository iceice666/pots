//! Agent run 的排程與執行（`proposal/agent-collaboration.md#一個-loop`）。
//!
//! 每個 project 同一時間只跑一個 run。訊息先進 project 的佇列：
//!
//! - 沒有 run 在跑：立刻開一個 run 處理。
//! - `Queue`：等目前的 run 結束，下一個 run 一次處理所有排隊的訊息。
//! - `Steer`：排進佇列並取消目前的 run，取消完成後馬上用佇列開新的 run。
//! - `Stop`：取消目前的 run，清掉佇列；被清掉的訊息仍寫進 `om_raw`，下次 run 看得到，
//!   但不會自己觸發 run。
//!
//! 人類訊息在 run 開始時才寫進 `om_raw`，不在收到時寫：run 進行中插入的使用者訊息會夾在
//! 工具呼叫與結果之間，破壞歷史的配對。
//!
//! 被取消的 run：訊息標成 Done，保留已產生的文字並加註「（已停止）」等說明；
//! `agent_runs.state = cancelled`。模型錯誤：訊息標成 Failed，加上白話的錯誤說明。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pots_llm::LlmError;
use pots_llm::agent::{self, AgentEvent, Hook, RunConfig, RunOutcome, Tool, ToolError};
use pots_llm::{Item, ToolCall, ToolSpec};
use pots_proto::{
    AgentRole, AgentState, Author, ItemId, ModelAuthView, Msg, MsgStatus, ProjectId, ProviderId,
    ServerEvent, StreamItemKind, StreamItemView, StreamScope, Timestamp, ToolSummary,
};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use uuid::Uuid;

use crate::Error;
use crate::hub::{Hub, LogTxn};
use crate::memory::Memory;
use crate::providers::{ProviderRouter, Route};
use crate::store::{ItemStatus, ProjectRow, RunState, Store};

/// Agent profile。M0 只有 chat，之後改成設定檔（`profiles/*.toml`）。
pub struct AgentProfile {
    pub name: &'static str,
    pub role_prompt: &'static str,
    pub max_turns: usize,
}

pub const CHAT_PROFILE: AgentProfile = AgentProfile {
    name: "chat",
    role_prompt: "你是 Pots 的 project 對話 agent，和使用者討論這個 project 的事情。\
        預設用繁體中文回答，簡潔、直接；不確定時說不確定，不要編造。\
        需要知道 project 的狀態或現在時間時，用工具查詢，不要猜。",
    max_turns: 8,
};

/// 平台的 gate：每次工具呼叫前後把關。
///
/// M0 全部放行。之後在這裡做 profile 的工具授權、scope 檢查、預算與熔斷、
/// Suspend／Effect 類工具的處理與 trace（`proposal/agent-collaboration.md`）。
pub struct Gate {
    #[allow(dead_code)]
    profile: &'static AgentProfile,
}

#[async_trait::async_trait]
impl Hook for Gate {}

#[derive(Clone, Debug)]
pub struct Trigger {
    pub item: ItemId,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopReason {
    User,
    Steer,
}

#[derive(Default)]
struct Lane {
    queued: Vec<Trigger>,
    /// 被 `Stop` 清掉的訊息，等 driver 寫進 `om_raw`。
    discarded: Vec<Trigger>,
    current: Option<CancellationToken>,
    stop_reason: Option<StopReason>,
    /// 這個 project 有沒有 driver task 在跑。
    driver: bool,
}

struct Inner {
    hub: Arc<Hub>,
    store: Store,
    /// 對話 agent 的歷史（記憶區塊 + 未觀察的尾巴）；寫進 `om_raw` 後叫醒它。
    memory: Memory,
    /// 每個 run 開始時決定要用的 provider 與模型。
    router: Arc<ProviderRouter>,
    /// 模型授權控制的狀態，用來在 run 成功後校正 hub 裡的授權狀態。
    auth_status: watch::Receiver<ModelAuthView>,
    lanes: Mutex<HashMap<ProjectId, Lane>>,
    tracker: TaskTracker,
    shutdown: CancellationToken,
}

#[derive(Clone)]
pub struct Runs {
    inner: Arc<Inner>,
}

impl Runs {
    pub fn new(
        hub: Arc<Hub>,
        store: Store,
        memory: Memory,
        router: Arc<ProviderRouter>,
        auth_status: watch::Receiver<ModelAuthView>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                hub,
                store,
                memory,
                router,
                auth_status,
                lanes: Mutex::new(HashMap::new()),
                tracker: TaskTracker::new(),
                shutdown: CancellationToken::new(),
            }),
        }
    }

    /// 把一則已存好的人類訊息交給 agent。
    pub fn enqueue(&self, project: ProjectId, trigger: Trigger, steer: bool) {
        if self.inner.shutdown.is_cancelled() {
            return;
        }
        let mut lanes = self.inner.lanes.lock().unwrap();
        let lane = lanes.entry(project).or_default();
        lane.queued.push(trigger);
        if !lane.driver {
            lane.driver = true;
            let inner = self.inner.clone();
            self.inner
                .tracker
                .spawn(async move { drive(inner, project).await });
        } else if steer && let Some(cancel) = &lane.current {
            lane.stop_reason = Some(StopReason::Steer);
            cancel.cancel();
        }
    }

    /// 停止目前的 run 並清掉佇列。
    pub fn stop(&self, project: ProjectId) {
        let mut lanes = self.inner.lanes.lock().unwrap();
        let Some(lane) = lanes.get_mut(&project) else {
            return;
        };
        let queued = std::mem::take(&mut lane.queued);
        lane.discarded.extend(queued);
        if let Some(cancel) = &lane.current {
            lane.stop_reason = Some(StopReason::User);
            cancel.cancel();
        }
    }

    /// 取消所有 run 並等它們收尾（最多 `timeout`）。之後不再接受新的 run。
    pub async fn shutdown(&self, timeout: Duration) {
        self.inner.shutdown.cancel();
        self.inner.tracker.close();
        if tokio::time::timeout(timeout, self.inner.tracker.wait())
            .await
            .is_err()
        {
            tracing::warn!("agent run 沒有在時限內結束");
        }
    }
}

impl Inner {
    fn has_queued(&self, project: ProjectId) -> bool {
        self.lanes
            .lock()
            .unwrap()
            .get(&project)
            .is_some_and(|l| !l.queued.is_empty())
    }
}

/// 一個 project 的 driver：把佇列處理完才結束。
async fn drive(inner: Arc<Inner>, project: ProjectId) {
    loop {
        let (batch, discarded, cancel) = {
            let mut lanes = inner.lanes.lock().unwrap();
            let lane = lanes.entry(project).or_default();
            let discarded = std::mem::take(&mut lane.discarded);
            let batch = if inner.shutdown.is_cancelled() {
                // 排隊中的訊息留在資料庫（pending），下次啟動時寫進 om_raw。
                lane.queued.clear();
                Vec::new()
            } else {
                std::mem::take(&mut lane.queued)
            };
            lane.stop_reason = None;
            if batch.is_empty() && discarded.is_empty() {
                lane.driver = false;
                lane.current = None;
                return;
            }
            let cancel = inner.shutdown.child_token();
            lane.current = (!batch.is_empty()).then(|| cancel.clone());
            (batch, discarded, cancel)
        };

        if !discarded.is_empty()
            && let Err(e) = consume_without_run(&inner, project, &discarded).await
        {
            tracing::error!(%project, error = %e, "無法寫入被停止的訊息");
        }
        if batch.is_empty() {
            continue;
        }
        if let Err(e) = run_chat(&inner, project, batch, cancel).await {
            tracing::error!(%project, error = %e, "agent run 失敗");
        }
    }
}

/// 不跑 agent，只把訊息寫進原始歷史。
async fn consume_without_run(
    inner: &Inner,
    project: ProjectId,
    triggers: &[Trigger],
) -> Result<(), Error> {
    let mut txn = inner.hub.begin().await?;
    for t in triggers {
        consume(&mut txn, project, t.item, &t.text, None).await?;
    }
    txn.commit().await?;
    inner.memory.wake(project);
    Ok(())
}

/// 人類訊息進入原始歷史：寫 `om_raw`、標成已處理，並記一筆 `ItemUpdated`，
/// 讓客戶端清掉「排隊中」的標示。
pub(crate) async fn consume(
    txn: &mut LogTxn<'_>,
    project: ProjectId,
    item: ItemId,
    text: &str,
    run_id: Option<Uuid>,
) -> Result<(), Error> {
    Store::append_om_raw(
        txn.conn(),
        project,
        run_id,
        &Item::User {
            text: text.to_string(),
        },
    )
    .await?;
    Store::mark_consumed(txn.conn(), item, run_id).await?;
    if let Some(view) = Store::item(txn.conn(), item).await? {
        txn.push(|seq| ServerEvent::ItemUpdated {
            seq,
            scope: StreamScope::Project(project),
            item: view,
        })
        .await?;
    }
    Ok(())
}

/// 串流中訊息的工具摘要。
#[derive(Default)]
struct ToolLog {
    calls: Vec<(String, String, Option<bool>)>,
}

impl ToolLog {
    fn started(&mut self, call: &ToolCall) {
        self.calls
            .push((call.call_id.clone(), call.name.clone(), None));
    }

    fn finished(&mut self, call: &ToolCall, ok: bool) {
        if let Some(c) = self.calls.iter_mut().find(|c| c.0 == call.call_id) {
            c.2 = Some(ok);
        }
    }

    fn summary(&self) -> Option<ToolSummary> {
        if self.calls.is_empty() {
            return None;
        }
        Some(ToolSummary {
            text: format!("呼叫了 {} 個工具", self.calls.len()),
            details: self
                .calls
                .iter()
                .map(|(_, name, ok)| match ok {
                    None => format!("{name}：執行中"),
                    Some(true) => format!("{name}：完成"),
                    Some(false) => format!("{name}：失敗"),
                })
                .collect(),
        })
    }
}

struct Reply {
    project: ProjectId,
    id: ItemId,
    at: Timestamp,
    author: Author,
    text: String,
    tools: ToolLog,
    /// 上一輪有文字輸出，下一段文字前要空一行。
    needs_break: bool,
}

impl Reply {
    fn view(&self, status: MsgStatus) -> StreamItemView {
        StreamItemView {
            id: self.id,
            at: self.at,
            kind: StreamItemKind::Agent(Msg {
                author: self.author.clone(),
                text: self.text.clone(),
                status,
                tools: self.tools.summary(),
                cards: Vec::new(),
                steer: false,
            }),
        }
    }

    fn scope(&self) -> StreamScope {
        StreamScope::Project(self.project)
    }

    fn append_note(&mut self, note: &str) {
        if !self.text.is_empty() {
            self.text.push_str("\n\n");
        }
        self.text.push_str(note);
    }
}

/// chat profile 的 run：歷史是 Observational Memory 的記憶區塊加上未觀察的原始尾巴。
async fn run_chat(
    inner: &Arc<Inner>,
    project: ProjectId,
    batch: Vec<Trigger>,
    cancel: CancellationToken,
) -> Result<(), Error> {
    let profile = &CHAT_PROFILE;
    let Some(row) = inner.store.project(project).await? else {
        return Err(Error::Corrupt(format!("找不到 project {project}")));
    };
    let run_id = Uuid::now_v7();
    let mut reply = Reply {
        project,
        id: ItemId::new(),
        at: Timestamp::now(),
        author: Author::Agent {
            label: row.agent_name.clone(),
            role: AgentRole::Project,
        },
        text: String::new(),
        tools: ToolLog::default(),
        needs_break: false,
    };

    // 開始：run 列、觸發訊息進歷史、空白的串流訊息、狀態「思考中」。
    {
        let mut txn = inner.hub.begin().await?;
        let trigger = serde_json::json!({
            "kind": "messages",
            "items": batch.iter().map(|t| t.item.to_string()).collect::<Vec<_>>(),
        });
        Store::insert_run(txn.conn(), run_id, project, profile.name, &trigger).await?;
        for t in &batch {
            consume(&mut txn, project, t.item, &t.text, Some(run_id)).await?;
        }
        let view = reply.view(MsgStatus::Streaming);
        Store::insert_item(
            txn.conn(),
            project,
            &view,
            ItemStatus::Streaming,
            Some(run_id),
        )
        .await?;
        let scope = reply.scope();
        txn.push(|seq| ServerEvent::Item {
            seq,
            scope,
            item: view,
        })
        .await?;
        txn.push(|seq| ServerEvent::AgentState {
            seq,
            project,
            state: AgentState::Thinking,
        })
        .await?;
        txn.commit().await?;
        inner.memory.wake(project);
    }

    // 整個 run 用同一個 provider 與模型，中途切換只影響下一個 run。
    let route = inner.router.resolve();
    let pinned = route.as_ref().ok().map(|r| r.id);
    let result = match route {
        Ok(route) => drive_loop(inner, &row, run_id, &mut reply, route, cancel).await,
        Err(e) => Err(RunError::Llm(e)),
    };

    let (status, item_status, run_state, error) = match &result {
        Ok(outcome) => {
            if *outcome == RunOutcome::MaxTurns {
                reply.append_note("（已達到單次回覆的工具輪數上限）");
            }
            (MsgStatus::Done, ItemStatus::Done, RunState::Done, None)
        }
        Err(RunError::Llm(LlmError::Cancelled)) => {
            let note = if inner.shutdown.is_cancelled() {
                "（服務已關閉，回覆中斷）"
            } else {
                let reason = inner
                    .lanes
                    .lock()
                    .unwrap()
                    .get(&project)
                    .and_then(|l| l.stop_reason);
                match reason {
                    Some(StopReason::Steer) => "（已依新訊息改方向）",
                    _ => "（已停止）",
                }
            };
            reply.append_note(note);
            (MsgStatus::Done, ItemStatus::Done, RunState::Cancelled, None)
        }
        Err(RunError::Llm(e)) => {
            reply.append_note(&explain(e));
            (
                MsgStatus::Failed,
                ItemStatus::Failed,
                RunState::Failed,
                Some(e.to_string()),
            )
        }
        Err(RunError::Server(e)) => {
            reply.append_note("（伺服器內部錯誤，回覆未完成）");
            (
                MsgStatus::Failed,
                ItemStatus::Failed,
                RunState::Failed,
                Some(e.to_string()),
            )
        }
    };

    let finished = finish(
        inner,
        &reply,
        run_id,
        pinned,
        &result,
        (status, item_status, run_state, error.as_deref()),
    )
    .await;
    if let Err(e) = finished {
        tracing::error!(%project, error = %e, "無法記錄 run 的結束，改用最少的收尾");
        abandon(inner, &mut reply).await;
    }
    Ok(())
}

/// 記錄 run 結束：訊息最後內容、`agent_runs`、授權狀態校正、`AgentState { Idle }`。
/// `pinned` 是這個 run 用的 provider；授權狀態的校正只在用 ChatGPT 時做。
async fn finish(
    inner: &Inner,
    reply: &Reply,
    run_id: Uuid,
    pinned: Option<ProviderId>,
    result: &Result<RunOutcome, RunError>,
    (status, item_status, run_state, error): (MsgStatus, ItemStatus, RunState, Option<&str>),
) -> Result<(), Error> {
    let project = reply.project;
    let mut txn = inner.hub.begin().await?;
    let view = reply.view(status);
    Store::update_item(txn.conn(), &view, item_status).await?;
    Store::finish_run(txn.conn(), run_id, run_state, error).await?;
    let scope = reply.scope();
    txn.push(|seq| ServerEvent::ItemDone {
        seq,
        scope,
        item: view,
    })
    .await?;
    let chatgpt = pinned == Some(ProviderId::CHATGPT);
    if chatgpt
        && matches!(result, Err(RunError::Llm(LlmError::NeedsLogin)))
        && !matches!(
            txn.state().model_auth,
            ModelAuthView::SignedOut | ModelAuthView::Pending { .. } | ModelAuthView::Error { .. }
        )
    {
        // provider 說要登入但授權狀態還是「已登入」：讓客戶端看到需要重新登入。
        // 登入流程進行中（Pending）時不蓋掉，網址還要留給客戶端。
        txn.push(|seq| ServerEvent::ModelAuth {
            seq,
            state: ModelAuthView::Error {
                message: "ChatGPT 授權已失效，請重新登入".into(),
            },
        })
        .await?;
    }
    if chatgpt && result.is_ok() {
        // 用 ChatGPT 推論成功代表授權可用：之前因 NeedsLogin 標成錯誤的話，改回授權控制的實際狀態。
        let current = inner.auth_status.borrow().clone();
        if txn.state().model_auth != current {
            txn.push(|seq| ServerEvent::ModelAuth {
                seq,
                state: current,
            })
            .await?;
        }
    }
    txn.push(|seq| ServerEvent::AgentState {
        seq,
        project,
        state: AgentState::Idle,
    })
    .await?;
    txn.commit().await
}

/// `finish` 失敗時的收尾：再試一次只記訊息失敗與閒置；還是不行就只改記憶體狀態。
async fn abandon(inner: &Inner, reply: &mut Reply) {
    let project = reply.project;
    reply.append_note("（伺服器內部錯誤，回覆未完成）");
    let view = reply.view(MsgStatus::Failed);
    let logged: Result<(), Error> = async {
        let mut txn = inner.hub.begin().await?;
        Store::update_item(txn.conn(), &view, ItemStatus::Failed).await?;
        let scope = reply.scope();
        txn.push(|seq| ServerEvent::ItemDone {
            seq,
            scope,
            item: view,
        })
        .await?;
        txn.push(|seq| ServerEvent::AgentState {
            seq,
            project,
            state: AgentState::Idle,
        })
        .await?;
        txn.commit().await
    }
    .await;
    if let Err(e) = logged {
        tracing::error!(%project, error = %e, "無法記錄 run 失敗，只清除記憶體裡的串流狀態");
        inner.hub.abandon(project, reply.id).await;
    }
}

enum RunError {
    Llm(LlmError),
    Server(Error),
}

/// 呼叫 agent loop，把事件轉成串流、工具摘要與 `om_raw`。
async fn drive_loop(
    inner: &Arc<Inner>,
    project: &ProjectRow,
    run_id: Uuid,
    reply: &mut Reply,
    route: Route,
    cancel: CancellationToken,
) -> Result<RunOutcome, RunError> {
    let history = inner
        .memory
        .load(project.id, &cancel)
        .await
        .map_err(RunError::Server)?;
    let history = close_dangling_calls(history);

    let mut instructions = CHAT_PROFILE.role_prompt.to_string();
    instructions.push_str(&format!("\n\n這個 project 叫「{}」。", project.name));
    if !project.instructions.trim().is_empty() {
        instructions.push_str("\n\n# Project 指示\n\n");
        instructions.push_str(project.instructions.trim());
    }
    let config = RunConfig {
        provider: route.provider,
        model: route.model,
        instructions: Some(instructions),
        tools: vec![Arc::new(GetStatus {
            inner: inner.clone(),
            project: project.id,
        })],
        hook: Arc::new(Gate {
            profile: &CHAT_PROFILE,
        }),
        max_turns: CHAT_PROFILE.max_turns,
    };

    let (tx, mut rx) = mpsc::unbounded_channel();
    let run = agent::run(&config, history, tx, cancel);
    let consume = async {
        // 寫入失敗時記下第一個錯誤，但仍把事件收完，讓 loop 正常結束。
        let mut failure = None;
        while let Some(ev) = rx.recv().await {
            if let Err(e) = on_event(inner, run_id, reply, ev).await
                && failure.is_none()
            {
                failure = Some(e);
            }
        }
        failure
    };
    let (result, failure) = tokio::join!(run, consume);
    if let Some(e) = failure {
        return Err(RunError::Server(e));
    }
    result.map(|r| r.outcome).map_err(RunError::Llm)
}

async fn on_event(
    inner: &Inner,
    run_id: Uuid,
    reply: &mut Reply,
    ev: AgentEvent,
) -> Result<(), Error> {
    match ev {
        AgentEvent::TextDelta(text) => {
            if text.is_empty() {
                return Ok(());
            }
            let text = if reply.needs_break && !reply.text.is_empty() {
                format!("\n\n{text}")
            } else {
                text
            };
            reply.needs_break = false;
            reply.text.push_str(&text);
            inner.hub.delta(reply.id, &text);
        }
        AgentEvent::Item(item) => {
            if matches!(item, Item::Assistant { .. }) {
                reply.needs_break = true;
            }
            let mut tx = inner.store.begin().await?;
            Store::append_om_raw(&mut tx, reply.project, Some(run_id), &item).await?;
            tx.commit().await?;
            inner.memory.wake(reply.project);
        }
        AgentEvent::ToolStarted(call) => {
            reply.tools.started(&call);
            log_update(inner, reply).await?;
        }
        AgentEvent::ToolFinished { call, ok } => {
            reply.tools.finished(&call, ok);
            log_update(inner, reply).await?;
        }
        AgentEvent::TurnCompleted { turn, usage } => {
            tracing::debug!(
                turn,
                input = usage.input_tokens,
                output = usage.output_tokens,
                "模型一輪結束"
            );
        }
    }
    Ok(())
}

async fn log_update(inner: &Inner, reply: &Reply) -> Result<(), Error> {
    let mut txn = inner.hub.begin().await?;
    let view = reply.view(MsgStatus::Streaming);
    Store::update_item(txn.conn(), &view, ItemStatus::Streaming).await?;
    let scope = reply.scope();
    txn.push(|seq| ServerEvent::ItemUpdated {
        seq,
        scope,
        item: view,
    })
    .await?;
    txn.commit().await
}

/// 異常結束可能留下沒有結果的工具呼叫；補上結果，否則 Responses 會拒絕整個請求。
/// 只改送給模型的副本，不改資料庫。
fn close_dangling_calls(history: Vec<Item>) -> Vec<Item> {
    let answered: HashSet<String> = history
        .iter()
        .filter_map(|i| match i {
            Item::ToolResult { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();
    let mut out = Vec::with_capacity(history.len());
    for item in history {
        let dangling = match &item {
            Item::ToolCall(call) if !answered.contains(&call.call_id) => Some(call.call_id.clone()),
            _ => None,
        };
        out.push(item);
        if let Some(call_id) = dangling {
            out.push(Item::ToolResult {
                call_id,
                output: agent::CANCELLED_OUTPUT.into(),
            });
        }
    }
    out
}

/// 模型錯誤的白話說明，顯示在訊息裡。
fn explain(e: &LlmError) -> String {
    match e {
        LlmError::NeedsLogin => {
            "（需要先登入 ChatGPT 才能回覆。請在 Inbox 按「開始登入」，完成後再傳一次訊息。）"
                .into()
        }
        LlmError::NotEntitled(m) => format!("（這個 ChatGPT 帳號不能用來推論：{m}）"),
        LlmError::UsageLimit => "（ChatGPT plan 的額度已用完，請稍後再試。）".into(),
        LlmError::Unavailable(m) => format!("（模型暫時無法使用，請稍後再試：{m}）"),
        LlmError::Unsupported(m) => format!("（模型不支援這個請求：{m}）"),
        LlmError::Interrupted(m) => format!("（回覆串流中斷，請再試一次：{m}）"),
        LlmError::Cancelled => "（已停止）".into(),
        LlmError::NotConfigured => {
            "（還沒設定模型 provider。請到左下角「設定」新增或選擇 provider，再傳一次訊息。）"
                .into()
        }
        LlmError::InvalidCredentials(m) => {
            format!("（模型 provider 的 API key 無效或沒有權限，請到「設定」檢查：{m}）")
        }
        LlmError::Other(m) => format!("（回覆失敗：{m}）"),
    }
}

// ---------------------------------------------------------------------------
// 唯讀工具

/// `get_status`：project 的基本狀態。M2 之後加上任務樹。
struct GetStatus {
    inner: Arc<Inner>,
    project: ProjectId,
}

#[async_trait::async_trait]
impl Tool for GetStatus {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "get_status".into(),
            description: "查詢這個 project 的狀態：名稱、建立時間、訊息數、現在時間（UTC），\
                以及是否有使用者訊息在排隊。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
        }
    }

    async fn call(&self, _arguments: &str) -> Result<String, ToolError> {
        let store = &self.inner.store;
        let project = store
            .project(self.project)
            .await
            .map_err(|e| ToolError(format!("讀取 project 失敗：{e}")))?
            .ok_or_else(|| ToolError("找不到這個 project".into()))?;
        let messages = store
            .count_messages(self.project)
            .await
            .map_err(|e| ToolError(format!("讀取訊息數失敗：{e}")))?;
        let status = serde_json::json!({
            "project": project.name,
            "agent": project.agent_name,
            "created_at": format_utc(project.created_at),
            "messages": messages,
            "now": format_utc(Timestamp::now()),
            "queued": self.inner.has_queued(self.project),
        });
        Ok(status.to_string())
    }
}

/// Unix 秒轉成 `2026-11-07T12:00:00Z`。
pub fn format_utc(t: Timestamp) -> String {
    let days = t.0.div_euclid(86_400);
    let secs = t.0.rem_euclid(86_400);
    // Howard Hinnant 的 civil_from_days。
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_format() {
        assert_eq!(format_utc(Timestamp(0)), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(Timestamp(1_793_968_496)), "2026-11-06T12:34:56Z");
        assert_eq!(format_utc(Timestamp(951_782_400)), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn dangling_calls_get_results() {
        let call = |id: &str| {
            Item::ToolCall(ToolCall {
                call_id: id.into(),
                name: "get_status".into(),
                arguments: "{}".into(),
            })
        };
        let history = vec![
            Item::User { text: "hi".into() },
            call("a"),
            Item::ToolResult {
                call_id: "a".into(),
                output: "ok".into(),
            },
            call("b"),
        ];
        let fixed = close_dangling_calls(history);
        assert_eq!(fixed.len(), 5);
        assert_eq!(
            fixed[4],
            Item::ToolResult {
                call_id: "b".into(),
                output: agent::CANCELLED_OUTPUT.into(),
            }
        );
    }

    #[test]
    fn tool_summary() {
        let mut log = ToolLog::default();
        assert!(log.summary().is_none());
        let call = ToolCall {
            call_id: "c1".into(),
            name: "get_status".into(),
            arguments: "{}".into(),
        };
        log.started(&call);
        assert_eq!(log.summary().unwrap().details, ["get_status：執行中"]);
        log.finished(&call, true);
        let s = log.summary().unwrap();
        assert_eq!(s.text, "呼叫了 1 個工具");
        assert_eq!(s.details, ["get_status：完成"]);
    }
}
