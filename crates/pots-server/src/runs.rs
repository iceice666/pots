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
//!
//! 模型在工具呼叫之間說的話（「我先查一下……」）是旁白：串流中放在 [`Msg::narration`] 讓人知道
//! 它在做什麼，結束後訊息只留最後的回答，旁白在「為什麼」面板的 trace 裡。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pots_llm::LlmError;
use pots_llm::agent::{self, AgentEvent, RunConfig, RunOutcome};
use pots_llm::{Item, ToolCall};
use pots_proto::{
    AgentRole, AgentState, Author, Card, GoalState, ItemId, ModelAuthView, ModelRole, Msg,
    MsgStatus, ProjectId, ProviderId, ServerEvent, StreamItemKind, StreamItemView, StreamScope,
    Timestamp, ToolSummary,
};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use uuid::Uuid;

use crate::Error;
use crate::hub::{Hub, LogTxn};
use crate::memory::Memory;
use crate::planner::Planner;
use crate::profiles::AgentProfile;
use crate::providers::{ProviderRouter, Route};
use crate::store::{ItemStatus, ProjectRow, RunState, Store};
use crate::tasks::{self, Principal, TaskState};
use crate::tools::{self, Scope, ToolCtx};

/// 對話 agent 的 instructions 裡列出幾張最近的里程碑卡片。
const RECENT_MILESTONES: u32 = 5;

/// 對話 agent 的輸出使用者看得到：請它呼叫工具前先說一句要做什麼（旁白，見模組說明）。
/// 寫在程式裡而不是 profile，覆寫 profile 時也會有。
const NARRATION_PROMPT: &str = "\n\n# 回覆的進行方式\n\n\
呼叫工具前，先用一句話告訴使用者你要做什麼、為什麼（例如「我先查一下目前的 Goal 狀態。」）；\
看到結果、改變做法時也一樣。這些話只在執行中顯示，完成後收進「為什麼這樣回答」，\
所以最後不再呼叫工具時的回覆要完整，不要依賴前面說過的話，也不要重複過程。";

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
    /// chat profile。
    profile: AgentProfile,
    /// 對話 agent 的工具會建立 Goal、叫醒 Planner。
    planner: Planner,
    /// 對話 agent 的歷史（記憶區塊 + 未觀察的尾巴）；寫進 `om_raw` 後叫醒它。
    memory: Memory,
    /// 每個 run 開始時決定要用的 provider 與模型。
    router: Arc<ProviderRouter>,
    /// 模型授權控制的狀態，用來在 run 成功後校正 hub 裡的授權狀態。
    auth_status: watch::Receiver<ModelAuthView>,
    clock: crate::config::Clock,
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
        profile: AgentProfile,
        planner: Planner,
    ) -> Self {
        let clock = planner.clock().clone();
        Self {
            inner: Arc::new(Inner {
                hub,
                store,
                profile,
                planner,
                memory,
                router,
                auth_status,
                clock,
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

/// 不是人類訊息的觸發（排程、listener 的通知）：在對話流貼一則只給人看的事件（`display`），
/// 狀態是 pending，給 agent 的文字（`text`）存在 `stream_items.trigger_text`。commit 之後用它的
/// ID 加上同一段文字呼叫 [`Runs::enqueue`]；run 開始時那段文字才寫進 `om_raw`（和人類訊息
/// 一樣）。服務在那之前關閉或當掉的話，下次啟動時 `recover` 照樣寫進 `om_raw`。
pub(crate) async fn post_system_trigger(
    txn: &mut LogTxn<'_>,
    project: ProjectId,
    display: &str,
    text: &str,
) -> Result<ItemId, Error> {
    let item = StreamItemView {
        id: ItemId::new(),
        at: Timestamp::now(),
        kind: StreamItemKind::Event {
            text: tasks::one_line(display, 300),
        },
    };
    let id = item.id;
    Store::insert_item(txn.conn(), project, &item, ItemStatus::Pending, None).await?;
    sqlx::query("UPDATE stream_items SET trigger_text = ? WHERE id = ?")
        .bind(text)
        .bind(id.to_string())
        .execute(txn.conn())
        .await?;
    txn.push(|seq| ServerEvent::Item {
        seq,
        scope: crate::store::stream_scope(project),
        item,
    })
    .await?;
    Ok(id)
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
            scope: crate::store::stream_scope(project),
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
    /// 附在這則回覆下面的卡片（例如 `create_goal` 的 Goal 卡片）。
    cards: Vec<Card>,
    /// 工具寫進來、還沒搬到 `cards` 的卡片。
    pending_cards: Arc<Mutex<Vec<Card>>>,
    /// 上一個輸出項目有文字，下一段文字前要空一行。
    needs_break: bool,
    /// 已結束、接著工具呼叫的那幾輪說的話（旁白）。`text` 只留這一輪的文字。
    narration: Vec<String>,
    /// 這一輪有工具呼叫：輪末把 `text` 搬進 `narration`。
    turn_calls: bool,
}

impl Reply {
    fn view(&self, status: MsgStatus) -> StreamItemView {
        StreamItemView {
            id: self.id,
            at: self.at,
            kind: StreamItemKind::Agent(Msg {
                author: self.author.clone(),
                text: self.text.clone(),
                // 旁白只在串流中顯示；結束後在「為什麼」面板的 trace 裡。
                narration: if status == MsgStatus::Streaming {
                    self.narration.clone()
                } else {
                    Vec::new()
                },
                status,
                tools: self.tools.summary(),
                cards: self.cards.clone(),
                steer: false,
            }),
        }
    }

    fn scope(&self) -> StreamScope {
        crate::store::stream_scope(self.project)
    }

    /// 一輪結束：這一輪接著工具呼叫的話搬進旁白。回傳是否有搬。
    fn end_turn(&mut self) -> bool {
        let calls = std::mem::take(&mut self.turn_calls);
        if !calls || self.text.trim().is_empty() {
            return false;
        }
        let text = std::mem::take(&mut self.text);
        self.narration.push(text.trim().to_string());
        self.needs_break = false;
        true
    }

    /// run 結束、加註說明之前：最後一輪沒有文字（例如回答寫在結束 run 的工具呼叫旁、
    /// 用完輪數、停止）時，把最後一段旁白當成回答，訊息才不會是空的。
    fn settle(&mut self) {
        if self.text.trim().is_empty()
            && let Some(last) = self.narration.pop()
        {
            self.text = last;
        }
    }

    fn append_note(&mut self, note: &str) {
        if !self.text.is_empty() {
            self.text.push_str("\n\n");
        }
        self.text.push_str(note);
    }
}

/// chat profile 的 run：歷史是 Observational Memory 的記憶區塊加上未觀察的原始尾巴。
#[tracing::instrument(
    name = "agent.run",
    skip_all,
    fields(agent = "chat", project = %project, run = tracing::field::Empty, model = tracing::field::Empty)
)]
async fn run_chat(
    inner: &Arc<Inner>,
    project: ProjectId,
    batch: Vec<Trigger>,
    cancel: CancellationToken,
) -> Result<(), Error> {
    let profile = &inner.profile;
    let Some(row) = inner.store.project(project).await? else {
        return Err(Error::Corrupt(format!("找不到 project {project}")));
    };
    let run_id = Uuid::now_v7();
    tracing::Span::current().record("run", tracing::field::display(run_id));
    let mut reply = Reply {
        project,
        id: ItemId::new(),
        at: Timestamp::now(),
        author: Author::Agent {
            label: row.agent_name.clone(),
            role: if project == crate::store::OVERSIGHT_PROJECT_ID {
                AgentRole::Overseer
            } else {
                AgentRole::Project
            },
        },
        text: String::new(),
        tools: ToolLog::default(),
        cards: Vec::new(),
        pending_cards: Arc::new(Mutex::new(Vec::new())),
        needs_break: false,
        narration: Vec::new(),
        turn_calls: false,
    };

    // 開始：run 列、觸發訊息進歷史、空白的串流訊息、狀態「思考中」。
    {
        let mut txn = inner.hub.begin().await?;
        let trigger = serde_json::json!({
            "kind": "messages",
            "items": batch.iter().map(|t| t.item.to_string()).collect::<Vec<_>>(),
            "text": batch.first().map(|t| tasks::truncate(&t.text, 300)).unwrap_or_default(),
        });
        Store::insert_run(txn.conn(), run_id, project, &profile.name, &trigger).await?;
        let revision = Store::om_state(txn.conn(), project).await?.revision;
        Store::set_run_memory_revision(txn.conn(), run_id, revision).await?;
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

    // 整個 run 用同一個 provider 與模型，中途切換只影響下一個 run。project 的 Runs 用 chat
    // profile（對話 agent），總管的 Runs 用 global profile（總管）；profile 名稱固定，
    // 不會不認得，保險起見退回對話 agent。
    let role = ModelRole::from_profile(&profile.name).unwrap_or(ModelRole::Chat);
    let route = inner.router.resolve_role(role);
    let pinned = route.as_ref().ok().map(|r| r.id);
    let result = match route {
        Ok(route) => drive_loop(inner, &row, run_id, &mut reply, route, cancel).await,
        Err(e) => Err(RunError::Llm(e)),
    };

    reply.settle();
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
    tracing::Span::current().record("model", tracing::field::display(&route.model));

    let profile = &inner.profile;
    let (head, tail) = {
        let mut conn = inner.store.acquire().await.map_err(RunError::Server)?;
        crate::user_md::compose(&mut conn, project.id)
            .await
            .map_err(RunError::Server)?
    };
    let mut instructions = format!("{head}{}{NARRATION_PROMPT}", profile.role_prompt());
    if project.id == crate::store::OVERSIGHT_PROJECT_ID {
        instructions.push_str(&format!("\n\n這個 project 叫「{}」。", project.name));
    } else {
        instructions.push_str(&format!(
            "\n\n這個 project 叫「{}」，你是它的負責人，名字是「{}」。",
            project.name, project.agent_name
        ));
    }
    if !project.instructions.trim().is_empty() {
        instructions.push_str("\n\n# Project 指示\n\n");
        instructions.push_str(project.instructions.trim());
    }
    instructions.push_str(&format!(
        "\n\n# 工作資料夾\n\n{}",
        crate::workdir::describe(project.workdir.as_ref())
    ));
    instructions.push_str(&tail);
    let overseer = project.id == crate::store::OVERSIGHT_PROJECT_ID;
    if overseer {
        let mut overview = String::new();
        let mut conn = inner.store.acquire().await.map_err(RunError::Server)?;
        for p in inner
            .store
            .list_projects()
            .await
            .map_err(RunError::Server)?
        {
            let goals = tasks::project_goals(&mut conn, p.id)
                .await
                .map_err(RunError::Server)?;
            let mut active: Vec<String> = Vec::new();
            for g in goals
                .iter()
                .filter(|g| matches!(g.state, GoalState::Active))
            {
                let tasks = tasks::tasks(&mut conn, g.id)
                    .await
                    .map_err(RunError::Server)?;
                let needs: usize = tasks
                    .iter()
                    .filter(|t| matches!(t.state, TaskState::NeedsHuman { .. }))
                    .count();
                active.push(format!(
                    "  - 「{}」（{} 個任務{}）{}",
                    g.title,
                    tasks.len(),
                    if needs > 0 {
                        format!("，{needs} 個需要你處理")
                    } else {
                        String::new()
                    },
                    g.planner_note
                ));
            }
            if active.is_empty() {
                overview.push_str(&format!("- {}：沒有進行中的 Goal\n", p.name));
            } else {
                overview.push_str(&format!("- {}：\n{}\n", p.name, active.join("\n")));
            }
        }
        instructions.push_str("\n\n# 目前的 project\n\n");
        instructions.push_str(&overview);
        instructions.push_str(
            "派工時用 create_goal 指定 project；核准留在各 project 的 Inbox，你不在這裡代為決定。",
        );
    } else {
        let goals = goal_overview(inner, project.id)
            .await
            .map_err(RunError::Server)?;
        if !goals.is_empty() {
            instructions.push_str("\n\n");
            instructions.push_str(&goals);
        }
    }
    let queued = {
        let inner = inner.clone();
        let project = project.id;
        Arc::new(move || inner.has_queued(project)) as Arc<dyn Fn() -> bool + Send + Sync>
    };
    let budget = crate::budget::for_run(
        &inner.store,
        &inner.hub,
        &inner.clock,
        run_id,
        project.id,
        None,
        None,
        route.meta("builtin"),
    )
    .await
    .map_err(RunError::Server)?;
    let ctx = Arc::new(ToolCtx {
        hub: inner.hub.clone(),
        store: inner.store.clone(),
        planner: inner.planner.clone(),
        project: project.id,
        scope: if overseer {
            Scope::Global
        } else {
            Scope::Project
        },
        actor: Principal::agent(&profile.name),
        reader: format!("chat:{}", project.id),
        artifact_kinds: profile.artifact_kinds.clone(),
        run_id,
        cards: reply.pending_cards.clone(),
        finished: Default::default(),
        budget,
        milestones: Default::default(),
        goals_created: Default::default(),
        queued: Some(queued),
        clock: inner.clock.clone(),
        worker: inner.planner.worker().clone(),
        work: None,
        code: None,
        stop: Arc::new(tokio::sync::Notify::new()),
        cancel: cancel.clone(),
    });
    let gate = ctx.gate();
    let config = RunConfig {
        reasoning: route.reasoning,
        provider: route.provider,
        model: route.model,
        instructions: Some(instructions),
        tools: tools::build(profile, ctx),
        hook: Arc::new(gate),
        max_turns: profile.max_turns,
        session_id: Some(format!("project:{}", project.id)),
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
            match item {
                Item::Assistant { .. } => reply.needs_break = true,
                Item::ToolCall(_) => reply.turn_calls = true,
                _ => {}
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
            let cards = std::mem::take(&mut *reply.pending_cards.lock().unwrap());
            reply.cards.extend(cards);
            log_update(inner, reply).await?;
        }
        AgentEvent::TurnCompleted { turn, usage } => {
            // 接著要跑工具：這一輪的話變成旁白，內文清空等下一輪。
            if reply.end_turn() {
                log_update(inner, reply).await?;
            }
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

/// 對話 agent 的 instructions 裡的 Goal 概況：進行中的 Goal 與最近的里程碑。
/// 里程碑卡片不寫進 `om_raw`（寫成使用者訊息會讓 Observer 誤認成使用者說的話），
/// 改從這裡讓對話 agent 在下一輪看到並轉述。沒有內容時是空字串。
async fn goal_overview(inner: &Inner, project: ProjectId) -> Result<String, Error> {
    let mut tx = inner.store.read().await?;
    let goals = tasks::goals(&mut tx, project).await?;
    let mut out = String::new();
    let active: Vec<_> = goals.iter().filter(|g| g.state.is_active()).collect();
    if !active.is_empty() {
        out.push_str("# 進行中的 Goal（細節與最新狀態用 get_status）\n\n");
        for g in active {
            let tasks = tasks::tasks(&mut tx, g.id).await?;
            let open = tasks.iter().filter(|t| t.state.is_open()).count();
            out.push_str(&format!(
                "- 「{}」（goal_id：{}）：{} 個任務，{} 個未結束；Planner：{}\n",
                tasks::one_line(&g.title, 120),
                g.id,
                tasks.len(),
                open,
                tasks::one_line(&g.planner_note, 120)
            ));
        }
    }
    let drafts: Vec<_> = goals.iter().filter(|g| g.state.is_draft()).collect();
    if !drafts.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(
            "# 草稿 Goal（等使用者看過 Brief 按「開始」；要改 Brief 用 steer_goal 帶 edits）\n\n",
        );
        for g in drafts {
            out.push_str(&format!(
                "- 「{}」（goal_id：{}）\n",
                tasks::one_line(&g.title, 120),
                g.id
            ));
        }
    }
    let mut milestones = Vec::new();
    for item in Store::recent_cards(&mut tx, project, RECENT_MILESTONES * 4).await? {
        if let StreamItemKind::Card(Card::Milestone { goal, text }) = item.kind {
            let title = goals
                .iter()
                .find(|g| g.id == goal)
                .map_or("（已刪除）", |g| g.title.as_str());
            let state = goals
                .iter()
                .find(|g| g.id == goal)
                .map(|g| &g.state)
                .filter(|s| !matches!(s, GoalState::Active))
                .map(|s| format!("（{}）", s.label()))
                .unwrap_or_default();
            milestones.push(format!(
                "- {} 「{}」{state}：{}",
                format_utc(item.at),
                tasks::one_line(title, 120),
                tasks::one_line(&text, 300)
            ));
            if milestones.len() as u32 >= RECENT_MILESTONES {
                break;
            }
        }
    }
    if !milestones.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("# Planner 最近的里程碑（新的在前；使用者在對話流裡也看得到）\n\n");
        out.push_str(&milestones.join("\n"));
        out.push('\n');
    }
    // 核准卡片不進記憶，只以一句結果讓對話 agent 知道（proposal/memory.md）。
    let approvals = crate::approvals::recent_outcomes(&mut tx, project, 5).await?;
    if !approvals.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("# 最近的核准（新的在前；使用者在對話流與 Inbox 決定）\n\n");
        out.push_str(&approvals.join("\n"));
        out.push('\n');
    }
    Ok(out)
}

/// 模型錯誤的白話說明，顯示在訊息裡。
pub(crate) fn explain(e: &LlmError) -> String {
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
