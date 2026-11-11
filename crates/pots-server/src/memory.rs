//! Observational Memory（`proposal/memory.md`）。
//!
//! 對話 agent 的歷史不再是完整的 `om_raw`，而是「記憶區塊 + 還沒觀察的原始尾巴」：
//!
//! - [`Memory::load`]：run 開始前組出 `agent::run` 的 `history`。記憶區塊（目前這一代的
//!   觀察與「目前在做什麼」）放最前面，後面接 `om_raw` 裡還沒觀察的項目，原樣還原。
//!   兩者在同一個讀取 transaction 裡讀，不會因為 Observer 剛好 commit 而漏掉一段。
//! - [`Memory::wake`]：每次寫進 `om_raw` 之後呼叫。每個 project 一個背景 driver（和
//!   `runs` 的 lane 同一種寫法），醒來後自己判斷有沒有超過閾值。
//!
//! 背景的 Observer：還沒觀察的原始歷史超過 [`OmConfig::observe_after`] 時，取出「未觀察、
//! 而且不在最近 [`OmConfig::keep_recent`] 則內」的一段，連同現有觀察送給 Observer 模型，
//! 把輸出寫進 `om_observations`，並把這段 `om_raw` 標成已觀察。切點只落在 run 的邊界
//! （下一則是人類訊息），或所有工具呼叫都有結果的回應之間，所以尾巴不會有懸空的
//! `ToolCall`，也不會把 reasoning 和它後面的輸出拆開。同一段只觀察一次：標記時只改
//! `observed = FALSE` 的列，筆數不符就丟掉結果（重啟後也不會重複寫入）。
//!
//! 背景的 Reflector：目前這一代的記憶超過 [`OmConfig::reflect_after`] 時，把觀察重寫成新的
//! 一代；被釘選的觀察原樣帶過去。舊世代保留作稽核。重寫期間你改了觀察就丟掉結果。
//!
//! 如果背景跟不上、未觀察的超過 [`OmConfig::block_after`]，`load` 會先同步觀察一次；
//! 失敗時照樣用完整的尾巴，不擋住對話。
//!
//! 同一個 project 的 Observer／Reflector（背景與 `load` 的同步觀察）用一把 async mutex
//! 序列化。模型由 [`ProviderRouter::resolve_memory`] 決定，和對話 agent 分開設定。
//! 失敗時記在 `om_state.last_error`（顯示在「記憶」分頁），[`OmConfig::retry_after`] 內不再自動重試。

pub(crate) mod format;
pub(crate) mod tokens;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use pots_llm::provider::Usage;
use pots_llm::{Item, LlmError, ModelRequest, StreamEvent};
use pots_proto::{MemoryView, ObservationId, ObservationView, ProjectId, ServerEvent};
use sqlx::sqlite::SqliteConnection;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::Error;
use crate::hub::Hub;
use crate::providers::{ProviderRouter, Route};
use crate::store::{
    ObservationRow, ObservationSource, ObservationState, OmStateRow, RawRow, Store,
};

/// 閾值與行為。預設值取自 Mastra 公開的 Observational Memory 預設值。
#[derive(Clone, Debug)]
pub struct OmConfig {
    /// 未觀察的原始歷史超過這麼多 token 時跑 Observer。
    pub observe_after: u64,
    /// 目前這一代的記憶超過這麼多 token 時跑 Reflector。
    pub reflect_after: u64,
    /// 未觀察的超過這麼多 token 時，`load` 先同步觀察（背景跟不上時的最後手段）。
    pub block_after: u64,
    /// 至少保留最近幾則原始訊息不觀察。
    pub keep_recent: usize,
    /// 一次送給 Observer 的原始歷史上限（token）；再多就分幾次。
    pub max_chunk: u64,
    /// 失敗後多久之內不再自動重試。
    pub retry_after: Duration,
    /// 一次 Observer／Reflector 請求的時限。
    pub request_timeout: Duration,
}

impl Default for OmConfig {
    fn default() -> Self {
        Self {
            observe_after: 30_000,
            reflect_after: 40_000,
            block_after: 60_000,
            keep_recent: 12,
            max_chunk: 60_000,
            retry_after: Duration::from_secs(60),
            request_timeout: Duration::from_secs(300),
        }
    }
}

/// 一次處理最多觀察幾段，避免異常時一直打模型。
const MAX_PASSES: usize = 16;
/// 連續失敗時冷卻時間的上限。
const MAX_BACKOFF: Duration = Duration::from_secs(30 * 60);
/// 所有 project 同時進行的 Observer／Reflector 請求上限。
const MAX_CONCURRENT_CALLS: usize = 2;
/// 格式不對時日誌只記這麼多字。
const LOG_PREVIEW_CHARS: usize = 200;
/// 你修正的觀察最多幾個字。
const MAX_EDIT_CHARS: usize = pots_proto::MAX_OBSERVATION_CHARS;

#[derive(Debug, thiserror::Error)]
enum OmError {
    #[error("{0}")]
    Llm(LlmError),
    #[error("{0}")]
    Server(Error),
    /// 模型的輸出認不得。
    #[error("模型的輸出格式不對")]
    Format,
}

impl From<Error> for OmError {
    fn from(e: Error) -> Self {
        Self::Server(e)
    }
}

impl From<LlmError> for OmError {
    fn from(e: LlmError) -> Self {
        Self::Llm(e)
    }
}

impl OmError {
    /// 給「記憶」分頁看的說明。
    fn describe(&self) -> String {
        match self {
            Self::Llm(LlmError::NotConfigured) => "還沒設定模型 provider，到「設定」選一個".into(),
            Self::Llm(e) => e.to_string(),
            Self::Server(_) => "伺服器內部錯誤".into(),
            Self::Format => "整理記憶的模型輸出格式不對".into(),
        }
    }
}

/// 哪個階段失敗：Observer 與 Reflector 各自冷卻，Reflector 失敗不影響觀察。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Observe,
    Reflect,
}

/// 失敗後的冷卻：連續失敗一次就加倍，最多 [`MAX_BACKOFF`]。
#[derive(Debug, Default)]
struct Backoff {
    failures: u32,
    until: Option<Instant>,
}

impl Backoff {
    fn active(&self) -> bool {
        self.until.is_some_and(|t| Instant::now() < t)
    }

    fn fail(&mut self, base: Duration) {
        self.failures = self.failures.saturating_add(1);
        let factor = 1u32 << (self.failures - 1).min(6);
        self.until = Some(Instant::now() + (base * factor).min(MAX_BACKOFF));
    }
}

struct Lane {
    /// 有 driver task 在跑。
    driver: bool,
    /// driver 開始處理後又被叫醒過。
    dirty: bool,
    observe: Backoff,
    reflect: Backoff,
    /// `om_state.last_error` 是哪個階段寫的；`None` 是不知道（例如重啟前寫的）。
    error_stage: Option<Stage>,
    /// `om_state.last_error` 有沒有值的快取；`None` 是還沒讀過。沒錯誤時就不用為了「清掉錯誤」
    /// 去拿 hub 的鎖。
    has_error: Option<bool>,
    /// 下次 driver 醒來時要看看該不該反思（剛啟動、剛重試，或剛觀察出新東西）。
    reflect_due: bool,
    /// 一次只讓一個 Observer／Reflector 呼叫改這個 project 的記憶；每段各自拿，
    /// `load` 的同步觀察最多等一段。
    lock: Arc<tokio::sync::Mutex<()>>,
}

impl Default for Lane {
    fn default() -> Self {
        Self {
            driver: false,
            dirty: false,
            observe: Backoff::default(),
            reflect: Backoff::default(),
            error_stage: None,
            has_error: None,
            reflect_due: true,
            lock: Arc::default(),
        }
    }
}

struct Inner {
    hub: Arc<Hub>,
    store: Store,
    router: Arc<ProviderRouter>,
    cfg: OmConfig,
    lanes: Mutex<HashMap<ProjectId, Lane>>,
    /// 同時進行的 Observer／Reflector 請求上限（所有 project 共用）。
    calls: tokio::sync::Semaphore,
    tracker: TaskTracker,
    shutdown: CancellationToken,
}

#[derive(Clone)]
pub(crate) struct Memory {
    inner: Arc<Inner>,
}

impl Memory {
    pub fn new(hub: Arc<Hub>, store: Store, router: Arc<ProviderRouter>, cfg: OmConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                hub,
                store,
                router,
                cfg,
                lanes: Mutex::new(HashMap::new()),
                calls: tokio::sync::Semaphore::new(MAX_CONCURRENT_CALLS),
                tracker: TaskTracker::new(),
                shutdown: CancellationToken::new(),
            }),
        }
    }

    /// 寫進 `om_raw` 之後呼叫：讓這個 project 的背景 Observer 看看要不要整理。
    pub fn wake(&self, project: ProjectId) {
        if self.inner.shutdown.is_cancelled() {
            return;
        }
        let mut lanes = self.inner.lanes.lock().unwrap();
        let lane = lanes.entry(project).or_default();
        lane.dirty = true;
        if !lane.driver {
            lane.driver = true;
            let inner = self.inner.clone();
            self.inner
                .tracker
                .spawn(async move { drive(inner, project).await });
        }
    }

    /// run 開始前呼叫：記憶區塊加上還沒觀察的原始尾巴，就是 `agent::run` 的 `history`
    /// （觸發訊息已經在尾巴裡）。
    ///
    /// 未觀察的超過 `block_after` 時先同步觀察；`cancel`（run 的停止／改方向）觸發時放棄
    /// 等待，照樣回傳目前的記憶與完整的尾巴，由 loop 自己以取消結束。
    pub async fn load(
        &self,
        project: ProjectId,
        cancel: &CancellationToken,
    ) -> Result<Vec<Item>, Error> {
        let inner = &self.inner;
        let unobserved = {
            let mut conn = inner.store.acquire().await?;
            Store::om_unobserved_tokens(&mut conn, project).await?
        };
        if unobserved > inner.cfg.block_after && !inner.backing_off(project, Stage::Observe) {
            tracing::info!(%project, unobserved, "背景觀察跟不上，先同步觀察");
            // 用 run 的 token：停止時只中斷等鎖與模型請求，不會丟下寫到一半的 transaction。
            inner.observe(project, inner.cfg.block_after, cancel).await;
        }
        let mut tx = inner.store.read().await?;
        let state = Store::om_state(&mut tx, project).await?;
        let observations = Store::om_observations(&mut tx, project, state.generation).await?;
        let rows = Store::om_unobserved(&mut tx, project).await?;
        drop(tx);
        let mut items = Vec::with_capacity(rows.len() + 1);
        match format::memory_block(&observations, &state.current_task) {
            Some(block) => items.push(Item::User { text: block }),
            // 已經整理過但沒有留下任何觀察：尾巴可能從回覆中間開始，補一個空的記憶區塊，
            // 讓歷史一定從使用者訊息開始。
            None if !matches!(
                rows.first(),
                None | Some(RawRow {
                    item: Item::User { .. },
                    ..
                })
            ) =>
            {
                items.push(Item::User {
                    text: format::EMPTY_MEMORY_BLOCK.into(),
                });
            }
            None => {}
        }
        items.extend(rows.into_iter().map(|r| r.item));
        Ok(items)
    }

    /// 你改了 provider 或記憶用的模型：清掉冷卻，讓每個 project 再試一次。
    pub async fn retry_all(&self) {
        for lane in self.inner.lanes.lock().unwrap().values_mut() {
            lane.observe = Backoff::default();
            lane.reflect = Backoff::default();
            lane.reflect_due = true;
        }
        match self.inner.store.list_projects().await {
            Ok(projects) => {
                for p in projects {
                    self.wake(p.id);
                }
            }
            Err(e) => tracing::error!(error = %e, "無法列出 project"),
        }
    }

    /// 送一次目前的記憶給客戶端（例如指令被拒絕時，讓客戶端丟掉它先改的內容）。
    pub async fn resync(&self, project: ProjectId) {
        let result: Result<(), Error> = async {
            let mut txn = self.inner.hub.begin().await?;
            let memory = view(txn.conn(), project).await?;
            txn.push(|seq| ServerEvent::Memory {
                seq,
                project,
                memory,
            })
            .await?;
            txn.commit().await
        }
        .await;
        if let Err(e) = result {
            tracing::error!(%project, error = %e, "無法重送記憶");
        }
    }

    /// 停止背景的 Observer／Reflector，最多等 `timeout`。
    pub async fn shutdown(&self, timeout: Duration) {
        self.inner.shutdown.cancel();
        self.inner.tracker.close();
        if tokio::time::timeout(timeout, self.inner.tracker.wait())
            .await
            .is_err()
        {
            tracing::warn!("整理記憶的工作沒有在時限內結束");
        }
    }

    // -----------------------------------------------------------------------
    // 你在「記憶」分頁的修改

    /// `PinObservation`。
    pub async fn pin(
        &self,
        project: ProjectId,
        id: ObservationId,
        pinned: bool,
    ) -> Result<(), String> {
        self.modify(project, id, |conn, row| {
            Box::pin(async move {
                if row.pinned != pinned {
                    Store::set_observation_pinned(conn, row.id, pinned).await?;
                }
                Ok(row.pinned != pinned)
            })
        })
        .await
    }

    /// `EditObservation`：舊列標成 superseded，新增一列取代它（同一個位置、釘選狀態不變）。
    pub async fn edit(
        &self,
        project: ProjectId,
        id: ObservationId,
        text: String,
    ) -> Result<(), String> {
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            return Err("觀察不能是空的".into());
        }
        if text.chars().count() > MAX_EDIT_CHARS {
            return Err(format!("一條觀察最多 {MAX_EDIT_CHARS} 個字"));
        }
        self.modify(project, id, move |conn, row| {
            Box::pin(async move {
                if row.text == text {
                    return Ok(false);
                }
                Store::set_observation_state(conn, row.id, ObservationState::Superseded).await?;
                let new = ObservationRow {
                    id: ObservationId::new(),
                    text,
                    source: ObservationSource::Human,
                    state: ObservationState::Active,
                    replaces: Some(row.id),
                    ..row
                };
                Store::insert_observation(conn, project, &new, None).await?;
                Ok(true)
            })
        })
        .await
    }

    /// `DeleteObservation`：標成 deleted，保留作稽核。
    pub async fn delete(&self, project: ProjectId, id: ObservationId) -> Result<(), String> {
        self.modify(project, id, |conn, row| {
            Box::pin(async move {
                Store::set_observation_state(conn, row.id, ObservationState::Deleted).await?;
                Ok(true)
            })
        })
        .await
    }

    /// 改目前這一代的一條有效觀察。`change` 回 `true` 表示內容有變：記憶的 revision 加一、
    /// 送出 `Memory` 事件。
    async fn modify<F>(
        &self,
        project: ProjectId,
        id: ObservationId,
        change: F,
    ) -> Result<(), String>
    where
        F: for<'c> FnOnce(
            &'c mut SqliteConnection,
            ObservationRow,
        ) -> futures::future::BoxFuture<'c, Result<bool, Error>>,
    {
        let result: Result<Result<(), String>, Error> = async {
            let mut txn = self.inner.hub.begin().await?;
            let mut state = Store::om_state(txn.conn(), project).await?;
            let row = Store::om_observation(txn.conn(), project, id).await?;
            let Some(row) = row.filter(|r| {
                r.state == ObservationState::Active && r.generation == state.generation
            }) else {
                return Ok(Err(
                    "找不到這條觀察，可能剛被整理過，請重新整理後再試".into()
                ));
            };
            if !change(txn.conn(), row).await? {
                return Ok(Ok(()));
            }
            state.revision += 1;
            Store::save_om_state(txn.conn(), project, &state).await?;
            let memory = view(txn.conn(), project).await?;
            txn.push(|seq| ServerEvent::Memory {
                seq,
                project,
                memory,
            })
            .await?;
            txn.commit().await?;
            Ok(Ok(()))
        }
        .await;
        result.map_err(|e| {
            tracing::error!(%project, error = %e, "無法修改觀察");
            "伺服器內部錯誤".to_string()
        })?
    }
}

/// 一個 project 的背景 driver：被叫醒過就再處理一次，直到沒有新的事。
async fn drive(inner: Arc<Inner>, project: ProjectId) {
    loop {
        {
            let mut lanes = inner.lanes.lock().unwrap();
            let lane = lanes.entry(project).or_default();
            if !lane.dirty || inner.shutdown.is_cancelled() {
                lane.driver = false;
                return;
            }
            lane.dirty = false;
        }
        let cancel = inner.shutdown.child_token();
        let committed = inner
            .observe(project, inner.cfg.observe_after, &cancel)
            .await;
        let due = {
            let mut lanes = inner.lanes.lock().unwrap();
            let lane = lanes.entry(project).or_default();
            std::mem::take(&mut lane.reflect_due) || committed
        };
        if due && !cancel.is_cancelled() && !inner.backing_off(project, Stage::Reflect) {
            inner.reflect(project, &cancel).await;
        }
    }
}

impl Inner {
    fn lane_lock(&self, project: ProjectId) -> Arc<tokio::sync::Mutex<()>> {
        self.lanes
            .lock()
            .unwrap()
            .entry(project)
            .or_default()
            .lock
            .clone()
    }

    fn backing_off(&self, project: ProjectId, stage: Stage) -> bool {
        self.lanes
            .lock()
            .unwrap()
            .get(&project)
            .is_some_and(|l| match stage {
                Stage::Observe => l.observe.active(),
                Stage::Reflect => l.reflect.active(),
            })
    }

    /// 觀察到未觀察的不超過 `limit`（或沒有可以切的地方）。每一段各自拿這個 project 的鎖。
    /// 錯誤記下來、開始冷卻，不往外傳。回傳有沒有寫進新的觀察。
    ///
    /// `cancel` 只會中斷等鎖與模型請求（`complete`）；寫資料庫的部分一定跑完，這個 future
    /// 也不能被外面丟掉，否則可能停在 commit 中間，讓 hub 的事件序號和資料庫對不上。
    async fn observe(&self, project: ProjectId, limit: u64, cancel: &CancellationToken) -> bool {
        if self.backing_off(project, Stage::Observe) {
            return false;
        }
        let lock = self.lane_lock(project);
        let mut committed = false;
        for _ in 0..MAX_PASSES {
            let result = {
                let _guard = tokio::select! {
                    () = cancel.cancelled() => return committed,
                    guard = lock.lock() => guard,
                };
                self.observe_once(project, limit, cancel).await
            };
            match result {
                Ok(true) => committed = true,
                Ok(false) => break,
                Err(e) => {
                    self.failed(project, Stage::Observe, &e).await;
                    return committed;
                }
            }
        }
        self.succeeded(project, Stage::Observe).await;
        committed
    }

    /// 記憶太大時反思一次。錯誤記下來、開始冷卻。
    async fn reflect(&self, project: ProjectId, cancel: &CancellationToken) {
        let lock = self.lane_lock(project);
        let result = {
            let _guard = tokio::select! {
                () = cancel.cancelled() => return,
                guard = lock.lock() => guard,
            };
            self.reflect_once(project, cancel).await
        };
        match result {
            Ok(_) => self.succeeded(project, Stage::Reflect).await,
            Err(e) => self.failed(project, Stage::Reflect, &e).await,
        }
    }

    async fn failed(&self, project: ProjectId, stage: Stage, error: &OmError) {
        if matches!(error, OmError::Llm(LlmError::Cancelled)) {
            return;
        }
        tracing::warn!(%project, ?stage, error = %error, "整理記憶失敗");
        {
            let mut lanes = self.lanes.lock().unwrap();
            let lane = lanes.entry(project).or_default();
            match stage {
                Stage::Observe => lane.observe.fail(self.cfg.retry_after),
                Stage::Reflect => lane.reflect.fail(self.cfg.retry_after),
            }
            lane.error_stage = Some(stage);
        }
        if let Err(e) = self.set_error(project, Some(&error.describe())).await {
            tracing::error!(%project, error = %e, "無法記錄整理記憶的錯誤");
        }
    }

    /// 記一筆 Observer／Reflector 的用量。沒有 run id 的整理工作也寫進 `agent_runs`，
    /// 成本報表才區分得出 OM 開銷與主要工作。
    async fn record_om_usage(
        &self,
        project: ProjectId,
        agent: &str,
        route: &Route,
        usage: Usage,
        started: Instant,
    ) {
        let started_at = pots_proto::Timestamp::now().0 - started.elapsed().as_secs() as i64;
        let result: Result<(), Error> = async {
            let mut tx = self.store.begin().await?;
            Store::record_om_run(
                &mut tx,
                project,
                agent,
                route,
                usage,
                pots_proto::Timestamp(started_at),
                pots_proto::Timestamp::now(),
            )
            .await?;
            tx.commit().await.map_err(Error::from)
        }
        .await;
        if let Err(e) = result {
            tracing::warn!(%project, error = %e, "無法記錄整理記憶的用量");
        }
    }

    /// 這個階段成功（含沒事可做）：結束冷卻；錯誤是這個階段（或不知道是誰）留下的就清掉。
    async fn succeeded(&self, project: ProjectId, stage: Stage) {
        let clear = {
            let mut lanes = self.lanes.lock().unwrap();
            let lane = lanes.entry(project).or_default();
            match stage {
                Stage::Observe => lane.observe = Backoff::default(),
                Stage::Reflect => lane.reflect = Backoff::default(),
            }
            let clear = lane.error_stage.is_none_or(|s| s == stage);
            if clear {
                lane.error_stage = None;
            }
            clear
        };
        if clear && let Err(e) = self.set_error(project, None).await {
            tracing::error!(%project, error = %e, "無法清掉整理記憶的錯誤");
        }
    }

    /// 觀察一段。回 `true` 表示有進展（或別處已經觀察過這段），可以再看一次。
    async fn observe_once(
        &self,
        project: ProjectId,
        limit: u64,
        cancel: &CancellationToken,
    ) -> Result<bool, OmError> {
        // 先只算總數：每次寫進 om_raw 都會叫醒，大多數時候還沒到閾值。
        let started = Instant::now();
        let total = {
            let mut conn = self.store.acquire().await?;
            Store::om_unobserved_tokens(&mut conn, project).await?
        };
        if total <= limit {
            return Ok(false);
        }
        let (rows, state, observations) = {
            let mut tx = self.store.read().await?;
            let rows = Store::om_unobserved(&mut tx, project).await?;
            let state = Store::om_state(&mut tx, project).await?;
            let observations = Store::om_observations(&mut tx, project, state.generation).await?;
            (rows, state, observations)
        };
        let total: u64 = rows.iter().map(|r| r.tokens).sum();
        if total <= limit {
            return Ok(false);
        }
        let Some(cut) = cut_point(&rows, self.cfg.keep_recent, self.cfg.max_chunk) else {
            return Ok(false);
        };
        let chunk = &rows[..cut];
        let route = self.router.resolve_memory()?;
        let input = format::observer_input(&observations, &state.current_task, chunk);
        let (output, usage) = self
            .complete(
                &route,
                format::OBSERVER_INSTRUCTIONS,
                input,
                format!("memory:{project}"),
                cancel,
            )
            .await?;
        self.record_om_usage(project, "observer", &route, usage, started)
            .await;
        let parsed = format::parse(&output);
        if parsed.is_empty() {
            tracing::warn!(%project, len = output.len(), "Observer 的輸出格式不對");
            tracing::debug!(%project, preview = %preview(&output), "Observer 的輸出");
            return Err(OmError::Format);
        }

        let (first, last) = (chunk[0].seq, chunk[cut - 1].seq);
        let mut txn = self.hub.begin().await?;
        let marked = Store::om_mark_observed(txn.conn(), project, first, last).await?;
        if marked != cut as u64 {
            // 這段已經有人觀察過（例如 `load` 的同步觀察）：放棄這次的結果。
            tracing::debug!(%project, first, last, marked, "這段已經觀察過");
            return Ok(true);
        }
        let mut state = Store::om_state(txn.conn(), project).await?;
        let mut seq = Store::om_next_seq(txn.conn(), project, state.generation).await?;
        for o in parsed.observations {
            let row = ObservationRow {
                id: ObservationId::new(),
                generation: state.generation,
                seq,
                date: o.date,
                priority: o.priority,
                text: o.text,
                pinned: false,
                source: ObservationSource::Observer,
                state: ObservationState::Active,
                replaces: None,
            };
            Store::insert_observation(txn.conn(), project, &row, Some(last)).await?;
            seq += 1;
        }
        if let Some(task) = parsed.current_task {
            state.current_task = format::clip(&task);
        }
        state.revision += 1;
        Store::save_om_state(txn.conn(), project, &state).await?;
        let memory = view(txn.conn(), project).await?;
        txn.push(|seq| ServerEvent::Memory {
            seq,
            project,
            memory,
        })
        .await?;
        txn.commit().await?;
        tracing::debug!(%project, first, last, "觀察了一段原始歷史");
        Ok(true)
    }

    /// 記憶太大時重寫成新的一代。回 `true` 表示產生了新的一代。
    async fn reflect_once(
        &self,
        project: ProjectId,
        cancel: &CancellationToken,
    ) -> Result<bool, OmError> {
        let started = Instant::now();
        let (state, observations) = {
            let mut tx = self.store.read().await?;
            let state = Store::om_state(&mut tx, project).await?;
            let observations = Store::om_observations(&mut tx, project, state.generation).await?;
            (state, observations)
        };
        let tokens = memory_tokens(&observations, &state.current_task);
        // 反思後還是很大時，要再多出一段才重來，不要每次觀察後都重寫。全部都釘選時沒得重寫。
        let floor = state.reflected_tokens + self.cfg.reflect_after / 4;
        let pinned = observations.iter().filter(|o| o.pinned).count();
        if tokens <= self.cfg.reflect_after || tokens <= floor || pinned == observations.len() {
            return Ok(false);
        }
        let route = self.router.resolve_memory()?;
        let input = format::reflector_input(&observations, &state.current_task);
        let (output, usage) = self
            .complete(
                &route,
                format::REFLECTOR_INSTRUCTIONS,
                input,
                format!("memory:{project}"),
                cancel,
            )
            .await?;
        self.record_om_usage(project, "reflector", &route, usage, started)
            .await;
        let parsed = format::parse(&output);
        // 釘選的由系統帶過去，Reflector 只重寫沒釘選的。重寫後只剩原本不到 1/20（含一條都不留，
        // 只有沒釘選的本來就很少時例外）多半是輸出被截斷或偷懶，當作格式不對，不要清掉記憶。
        let unpinned: u64 = observations
            .iter()
            .filter(|o| !o.pinned)
            .map(|o| line_tokens(&o.date, o.priority, &o.text))
            .sum();
        let rewritten: u64 = parsed
            .observations
            .iter()
            .map(|o| line_tokens(&o.date, o.priority, &o.text))
            .sum();
        let small = unpinned <= self.cfg.reflect_after / 4;
        if parsed.is_empty() || (!small && rewritten.saturating_mul(20) < unpinned) {
            tracing::warn!(
                %project,
                len = output.len(),
                unpinned,
                rewritten,
                "Reflector 的輸出格式不對或丟掉太多"
            );
            tracing::debug!(%project, preview = %preview(&output), "Reflector 的輸出");
            return Err(OmError::Format);
        }

        let mut txn = self.hub.begin().await?;
        let mut current = Store::om_state(txn.conn(), project).await?;
        if current.revision != state.revision || current.generation != state.generation {
            // 重寫期間觀察變了（你修正了，或又有新的觀察）：下次再反思。
            tracing::debug!(%project, "反思期間記憶有變，丟掉這次的結果");
            return Ok(false);
        }
        let generation = state.generation + 1;
        // 釘選的原樣帶過去，和 Reflector 的輸出一起依日期排序（同一天釘選的在前）。
        let mut next: Vec<ObservationRow> = observations
            .iter()
            .filter(|o| o.pinned)
            .map(|o| ObservationRow {
                id: ObservationId::new(),
                generation,
                replaces: None,
                ..o.clone()
            })
            .collect();
        next.extend(parsed.observations.into_iter().map(|o| ObservationRow {
            id: ObservationId::new(),
            generation,
            seq: 0,
            date: o.date,
            priority: o.priority,
            text: o.text,
            pinned: false,
            source: ObservationSource::Reflector,
            state: ObservationState::Active,
            replaces: None,
        }));
        next.sort_by(|a, b| a.date.cmp(&b.date));
        for (i, row) in next.iter_mut().enumerate() {
            row.seq = i as i64 + 1;
            Store::insert_observation(txn.conn(), project, row, None).await?;
        }
        if let Some(task) = parsed.current_task {
            current.current_task = format::clip(&task);
        }
        current.generation = generation;
        current.revision += 1;
        current.reflected_tokens = memory_tokens(&next, &current.current_task);
        Store::save_om_state(txn.conn(), project, &current).await?;
        let memory = view(txn.conn(), project).await?;
        txn.push(|seq| ServerEvent::Memory {
            seq,
            project,
            memory,
        })
        .await?;
        txn.commit().await?;
        tracing::info!(%project, generation, before = tokens, after = current.reflected_tokens, "記憶重寫成新的一代");
        Ok(true)
    }

    /// 記下（或清掉）整理失敗的原因；和現在一樣就不寫、不送事件。沒有錯誤可清時不拿 hub 的鎖。
    async fn set_error(&self, project: ProjectId, message: Option<&str>) -> Result<(), Error> {
        if message.is_none() {
            let cached = self
                .lanes
                .lock()
                .unwrap()
                .get(&project)
                .and_then(|l| l.has_error);
            let has_error = match cached {
                Some(has) => has,
                None => {
                    let mut conn = self.store.acquire().await?;
                    Store::om_state(&mut conn, project)
                        .await?
                        .last_error
                        .is_some()
                }
            };
            if !has_error {
                self.cache_error(project, false);
                return Ok(());
            }
        }
        let mut txn = self.hub.begin().await?;
        let mut state = Store::om_state(txn.conn(), project).await?;
        if state.last_error.as_deref() == message {
            drop(txn);
            self.cache_error(project, message.is_some());
            return Ok(());
        }
        state.last_error = message.map(str::to_string);
        Store::save_om_state(txn.conn(), project, &state).await?;
        let memory = view(txn.conn(), project).await?;
        txn.push(|seq| ServerEvent::Memory {
            seq,
            project,
            memory,
        })
        .await?;
        txn.commit().await?;
        self.cache_error(project, message.is_some());
        Ok(())
    }

    fn cache_error(&self, project: ProjectId, has_error: bool) {
        self.lanes
            .lock()
            .unwrap()
            .entry(project)
            .or_default()
            .has_error = Some(has_error);
    }

    /// 送出一次不帶工具的請求，回傳模型的文字。只有收到 `Completed` 才算成功。
    async fn complete(
        &self,
        route: &Route,
        instructions: &str,
        input: String,
        session_id: String,
        cancel: &CancellationToken,
    ) -> Result<(String, Usage), OmError> {
        let request = ModelRequest {
            reasoning: None,
            model: route.model.clone(),
            instructions: Some(instructions.to_string()),
            input: vec![Item::User { text: input }],
            tools: Vec::new(),
            session_id: Some(session_id),
        };
        // 排隊等名額的時間不算進請求的時限。
        let _permit = tokio::select! {
            () = cancel.cancelled() => return Err(OmError::Llm(LlmError::Cancelled)),
            permit = self.calls.acquire() => {
                permit.map_err(|_| OmError::Llm(LlmError::Cancelled))?
            }
        };
        let run = async {
            let mut stream = route.provider.stream(request, cancel.clone()).await?;
            let mut deltas = String::new();
            let mut items = String::new();
            while let Some(event) = stream.next().await {
                match event? {
                    StreamEvent::TextDelta(t) => deltas.push_str(&t),
                    StreamEvent::ItemDone(Item::Assistant { text }) => {
                        if !items.is_empty() {
                            items.push('\n');
                        }
                        items.push_str(&text);
                    }
                    StreamEvent::ItemDone(_) => {}
                    StreamEvent::Completed { usage } => {
                        return Ok((if items.is_empty() { deltas } else { items }, usage));
                    }
                }
            }
            Err(LlmError::Interrupted("串流在完成前結束".into()))
        };
        tokio::select! {
            () = cancel.cancelled() => Err(OmError::Llm(LlmError::Cancelled)),
            r = tokio::time::timeout(self.cfg.request_timeout, run) => match r {
                Ok(r) => r.map_err(OmError::Llm),
                Err(_) => Err(OmError::Llm(LlmError::Unavailable("整理記憶的請求逾時".into()))),
            },
        }
    }
}

/// 日誌只記開頭一小段，不記完整的模型輸出（可能含對話內容）。
fn preview(text: &str) -> String {
    match text.char_indices().nth(LOG_PREVIEW_CHARS) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

/// 一條觀察（一行）的估計 token 數。
fn line_tokens(date: &str, priority: pots_proto::Priority, text: &str) -> u64 {
    tokens::estimate(&format::observation_line(date, priority, text))
}

/// 記憶區塊的估計 token 數。
fn memory_tokens(observations: &[ObservationRow], current_task: &str) -> u64 {
    format::memory_block(observations, current_task).map_or(0, |b| tokens::estimate(&b))
}

/// 這次要觀察幾則：回傳切點 `i`，觀察 `rows[..i]`、`rows[i..]` 留在尾巴。
///
/// 至少留 `keep_recent` 則。切點要落在邊界：下一則是人類訊息（新的 run；之前沒有結果的
/// 工具呼叫已經不可能有結果了），或上一則是工具結果、所有工具呼叫都有結果、下一則不是
/// 工具結果（同一個 run 裡兩次回應之間）。在 `max_chunk` 內取最遠的邊界；第一個邊界就
/// 超過時仍取它。沒有邊界時回 `None`。
fn cut_point(rows: &[RawRow], keep_recent: usize, max_chunk: u64) -> Option<usize> {
    if rows.len() <= keep_recent {
        return None;
    }
    let limit = rows.len() - keep_recent;
    let mut open = std::collections::HashSet::new();
    let mut tokens = 0u64;
    let mut best = None;
    for i in 1..=limit {
        let prev = &rows[i - 1];
        tokens += prev.tokens;
        match &prev.item {
            Item::ToolCall(call) => {
                open.insert(call.call_id.as_str());
            }
            Item::ToolResult { call_id, .. } => {
                open.remove(call_id.as_str());
            }
            _ => {}
        }
        let Some(next) = rows.get(i) else { break };
        let boundary = match &next.item {
            Item::User { .. } => {
                open.clear();
                true
            }
            Item::ToolResult { .. } => false,
            _ => matches!(prev.item, Item::ToolResult { .. }) && open.is_empty(),
        };
        if !boundary {
            continue;
        }
        if tokens > max_chunk && best.is_some() {
            break;
        }
        best = Some(i);
        if tokens >= max_chunk {
            break;
        }
    }
    best
}

/// 一個 project 目前的記憶，給快照與 `Memory` 事件。
pub(crate) async fn view(
    conn: &mut SqliteConnection,
    project: ProjectId,
) -> Result<MemoryView, Error> {
    let state = Store::om_state(conn, project).await?;
    let observations = Store::om_observations(conn, project, state.generation).await?;
    Ok(memory_view(&state, &observations))
}

fn memory_view(state: &OmStateRow, observations: &[ObservationRow]) -> MemoryView {
    MemoryView {
        generation: state.generation.max(0) as u32,
        current_task: state.current_task.clone(),
        tokens: memory_tokens(observations, &state.current_task),
        observations: observations
            .iter()
            .map(|o| ObservationView {
                id: o.id,
                date: o.date.clone(),
                priority: o.priority,
                text: o.text.clone(),
                pinned: o.pinned,
                edited: o.source == ObservationSource::Human,
            })
            .collect(),
        error: state.last_error.clone(),
    }
}

#[cfg(test)]
mod tests {
    use pots_llm::ToolCall;
    use pots_proto::Timestamp;

    use super::*;

    fn row(seq: i64, item: Item, tokens: u64) -> RawRow {
        RawRow {
            seq,
            item,
            tokens,
            created_at: Timestamp(0),
        }
    }

    fn user(seq: i64) -> RawRow {
        row(seq, Item::User { text: "u".into() }, 10)
    }

    fn assistant(seq: i64) -> RawRow {
        row(seq, Item::Assistant { text: "a".into() }, 10)
    }

    fn call(seq: i64, id: &str) -> RawRow {
        row(
            seq,
            Item::ToolCall(ToolCall {
                call_id: id.into(),
                name: "t".into(),
                arguments: "{}".into(),
            }),
            10,
        )
    }

    fn result(seq: i64, id: &str) -> RawRow {
        row(
            seq,
            Item::ToolResult {
                call_id: id.into(),
                output: "ok".into(),
            },
            10,
        )
    }

    fn opaque(seq: i64) -> RawRow {
        row(
            seq,
            Item::Opaque {
                provider: "p".into(),
                value: serde_json::json!({}),
            },
            10,
        )
    }

    #[test]
    fn cut_keeps_recent_and_respects_boundaries() {
        // u a | u a u a
        let rows = [
            user(1),
            assistant(2),
            user(3),
            assistant(4),
            user(5),
            assistant(6),
        ];
        assert_eq!(cut_point(&rows, 2, 1000), Some(4));
        assert_eq!(cut_point(&rows, 6, 1000), None);
        // 至少一則以上的上限：第一個邊界超過上限仍取它，之後停下。
        assert_eq!(cut_point(&rows, 0, 5), Some(2));
        assert_eq!(cut_point(&rows, 0, 30), Some(2));
        assert_eq!(cut_point(&rows, 0, 40), Some(4));
    }

    #[test]
    fn cut_never_splits_tool_calls_or_reasoning() {
        // u o c1 c2 r1 r2 o a u a
        let rows = [
            user(1),
            opaque(2),
            call(3, "1"),
            call(4, "2"),
            result(5, "1"),
            result(6, "2"),
            opaque(7),
            assistant(8),
            user(9),
            assistant(10),
        ];
        // 只能在 r2 之後（index 6）或下一個人類訊息之前（index 8）切。
        for keep in 0..rows.len() {
            if let Some(i) = cut_point(&rows, keep, 1000) {
                assert!(i == 6 || i == 8, "keep={keep} 切在 {i}");
                assert!(rows.len() - i >= keep);
            }
        }
        assert_eq!(cut_point(&rows, 4, 1000), Some(6));
        assert_eq!(cut_point(&rows, 3, 1000), Some(6));
        assert_eq!(cut_point(&rows, 2, 1000), Some(8));
        // 中斷的 run 留下沒有結果的呼叫：下一則人類訊息仍是邊界。
        let rows = [user(1), call(2, "x"), user(3), assistant(4)];
        assert_eq!(cut_point(&rows, 1, 1000), Some(2));
        assert_eq!(cut_point(&rows, 0, 1000), Some(2));
    }
}

/// 直接驅動 [`Memory`]（不經 WebSocket），檢查資料庫裡的觀察、世代與錯誤。
#[cfg(test)]
mod db_tests {
    use pots_llm::testing::{FnProvider, ScriptedTurn};
    use pots_proto::{InstanceId, InstanceKind, ModelAuthView, ProviderId, Timestamp};

    use super::*;
    use crate::hub::InstanceInfo;
    use crate::providers::ProviderRegistry;
    use crate::store::ProjectRow;

    struct Fixture {
        _dir: tempfile::TempDir,
        memory: Memory,
        store: Store,
        project: ProjectId,
        provider: Arc<FnProvider>,
    }

    /// 假的 Observer／Reflector：Observer 把含「決定」的人類訊息各記成一條觀察；
    /// Reflector 回一條合併後的觀察。對話請求回「收到」。
    fn fake_model(request: &ModelRequest) -> ScriptedTurn {
        let instructions = request.instructions.as_deref().unwrap_or("");
        let input = match request.input.first() {
            Some(Item::User { text }) => text.clone(),
            _ => String::new(),
        };
        if instructions == format::OBSERVER_INSTRUCTIONS {
            let mut out = String::new();
            for line in input.lines() {
                if let Some((_, text)) = line.split_once("] 使用者：")
                    && text.contains("決定")
                {
                    out.push_str(&format!("- 2026-11-07 · 高 · {text}\n"));
                }
            }
            out.push_str("目前在做什麼：測試記憶");
            ScriptedTurn::reply(out)
        } else if instructions == format::REFLECTOR_INSTRUCTIONS {
            ScriptedTurn::reply("- 2026-11-08 · 中 · 合併後的觀察\n目前在做什麼：反思之後")
        } else {
            ScriptedTurn::reply("收到")
        }
    }

    async fn fixture(
        cfg: OmConfig,
        reply: impl Fn(&ModelRequest) -> ScriptedTurn + Send + Sync + 'static,
    ) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_sqlite(&dir.path().join("pots.db"))
            .await
            .unwrap();
        let project = ProjectId::new();
        let mut tx = store.begin().await.unwrap();
        Store::insert_project(
            &mut tx,
            &ProjectRow {
                id: project,
                name: "記憶".into(),
                instructions: String::new(),
                agent_name: "Pots".into(),
                workdir: None,
                created_at: Timestamp::now(),
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let loaded = ProviderRegistry::load(
            &store,
            dir.path(),
            Arc::new(crate::NoApiProviders),
            Some(ProviderId::CHATGPT),
        )
        .await
        .unwrap();
        let registry = Arc::new(loaded.registry);
        let hub = Arc::new(
            Hub::new(
                store.clone(),
                InstanceInfo {
                    id: InstanceId::new(),
                    name: "測試".into(),
                    kind: InstanceKind::Desktop,
                },
                ModelAuthView::SignedIn { account: None },
                registry.view(),
            )
            .await
            .unwrap(),
        );
        let provider = FnProvider::new("fn", reply);
        let router = Arc::new(ProviderRouter::new(
            registry,
            provider.clone(),
            "memory-model".into(),
        ));
        let memory = Memory::new(hub, store.clone(), router, cfg);
        Fixture {
            _dir: dir,
            memory,
            store,
            project,
            provider,
        }
    }

    impl Fixture {
        async fn append(&self, items: impl IntoIterator<Item = Item>) {
            let mut tx = self.store.begin().await.unwrap();
            for item in items {
                Store::append_om_raw(&mut tx, self.project, None, &item)
                    .await
                    .unwrap();
            }
            tx.commit().await.unwrap();
        }

        /// `n` 輪「人類訊息 + agent 回覆」。
        async fn rounds(&self, range: std::ops::Range<usize>) {
            for i in range {
                self.append([
                    Item::User {
                        text: format!("決定第 {i} 件事：{}", "很".repeat(20)),
                    },
                    Item::Assistant { text: "好".into() },
                ])
                .await;
            }
        }

        async fn view(&self) -> MemoryView {
            let mut conn = self.store.acquire().await.unwrap();
            view(&mut conn, self.project).await.unwrap()
        }

        fn requests(&self, instructions: &str) -> usize {
            self.provider
                .requests()
                .iter()
                .filter(|r| r.instructions.as_deref() == Some(instructions))
                .count()
        }

        async fn observe_all(&self) {
            let cancel = CancellationToken::new();
            let inner = &self.memory.inner;
            while inner
                .observe_once(self.project, inner.cfg.observe_after, &cancel)
                .await
                .unwrap()
            {}
        }

        /// 等背景 driver 結束。
        async fn settle(&self) {
            for _ in 0..200 {
                let busy = self
                    .memory
                    .inner
                    .lanes
                    .lock()
                    .unwrap()
                    .get(&self.project)
                    .is_some_and(|l| l.driver);
                if !busy {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("背景整理沒有結束");
        }
    }

    fn small() -> OmConfig {
        OmConfig {
            observe_after: 50,
            reflect_after: 60,
            block_after: 10_000,
            keep_recent: 2,
            max_chunk: 10_000,
            ..OmConfig::default()
        }
    }

    #[tokio::test]
    async fn observes_edits_and_reflects_keeping_pinned() {
        let f = fixture(small(), fake_model).await;
        f.rounds(0..6).await;
        f.observe_all().await;
        assert_eq!(f.requests(format::OBSERVER_INSTRUCTIONS), 1);
        let view = f.view().await;
        // 最後一輪（2 則）留在尾巴。
        assert_eq!(view.observations.len(), 5);
        assert_eq!(view.current_task, "測試記憶");
        assert_eq!(view.generation, 0);
        let ids: Vec<_> = view.observations.iter().map(|o| o.id).collect();

        f.memory.pin(f.project, ids[0], true).await.unwrap();
        f.memory
            .edit(f.project, ids[1], "  改過的  決定 ".into())
            .await
            .unwrap();
        f.memory.delete(f.project, ids[2]).await.unwrap();
        assert!(f.memory.delete(f.project, ids[2]).await.is_err());
        assert!(f.memory.edit(f.project, ids[3], " ".into()).await.is_err());
        let view = f.view().await;
        assert_eq!(view.observations.len(), 4);
        assert!(view.observations[0].pinned);
        assert_eq!(view.observations[1].text, "改過的 決定");
        assert!(view.observations[1].edited);
        assert!(!view.observations.iter().any(|o| o.id == ids[2]));

        let history = f
            .memory
            .load(f.project, &CancellationToken::new())
            .await
            .unwrap();
        let Item::User { text: block } = &history[0] else {
            panic!("第一筆應該是記憶區塊");
        };
        assert!(block.contains("改過的 決定"));
        assert!(!block.contains("決定第 2 件事"));
        assert!(!block.contains("決定第 1 件事"));
        assert_eq!(
            &history[1..],
            &[
                Item::User {
                    text: format!("決定第 5 件事：{}", "很".repeat(20)),
                },
                Item::Assistant { text: "好".into() },
            ]
        );

        let cancel = CancellationToken::new();
        assert!(
            f.memory
                .inner
                .reflect_once(f.project, &cancel)
                .await
                .unwrap()
        );
        let reflector = f
            .provider
            .requests()
            .into_iter()
            .find(|r| r.instructions.as_deref() == Some(format::REFLECTOR_INSTRUCTIONS))
            .unwrap();
        let Item::User { text } = &reflector.input[0] else {
            panic!()
        };
        assert!(text.contains("決定第 0 件事"));
        assert!(text.contains("（釘選）"));
        let view = f.view().await;
        assert_eq!(view.generation, 1);
        assert_eq!(view.current_task, "反思之後");
        let texts: Vec<_> = view.observations.iter().map(|o| o.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                format!("決定第 0 件事：{}", "很".repeat(20)).as_str(),
                "合併後的觀察"
            ]
        );
        assert!(view.observations[0].pinned);
        // 舊的一代保留：5 條原始觀察 + 1 條修正後的。
        let old = f.store.om_generation_rows(f.project, 0).await.unwrap();
        assert_eq!(old.len(), 6);
        assert_eq!(
            old.iter()
                .filter(|o| o.state == ObservationState::Superseded)
                .count(),
            1
        );
        assert_eq!(
            old.iter()
                .filter(|o| o.state == ObservationState::Deleted)
                .count(),
            1
        );
        // 舊一代的觀察不能再改。
        assert!(f.memory.pin(f.project, ids[3], true).await.is_err());
        // 剛反思過：沒有多出一段之前不再反思。
        assert!(
            !f.memory
                .inner
                .reflect_once(f.project, &cancel)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn load_observes_synchronously_when_far_behind() {
        let cfg = OmConfig {
            block_after: 100,
            ..small()
        };
        let f = fixture(cfg, fake_model).await;
        f.rounds(0..6).await;
        // 沒有叫醒背景：只有 load 的同步觀察。
        let history = f
            .memory
            .load(f.project, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(f.requests(format::OBSERVER_INSTRUCTIONS), 1);
        assert!(matches!(&history[0], Item::User { text } if text.starts_with("<memory>")));
        assert_eq!(history.len(), 1 + 2);
        // 同步觀察只做到不超過 block_after，不反思。
        assert_eq!(f.requests(format::REFLECTOR_INSTRUCTIONS), 0);
        // 同一段只觀察一次。
        f.observe_all().await;
        assert_eq!(f.requests(format::OBSERVER_INSTRUCTIONS), 1);
    }

    #[tokio::test]
    async fn failures_are_shown_and_back_off() {
        let cfg = OmConfig {
            block_after: 100,
            ..small()
        };
        let f = fixture(cfg, |request| {
            if request.instructions.as_deref() == Some(format::OBSERVER_INSTRUCTIONS) {
                ScriptedTurn::reject(LlmError::Unavailable("維修中".into()))
            } else {
                ScriptedTurn::reply("收到")
            }
        })
        .await;
        f.rounds(0..6).await;
        f.memory.wake(f.project);
        f.settle().await;
        assert_eq!(f.requests(format::OBSERVER_INSTRUCTIONS), 1);
        let view = f.view().await;
        assert!(view.observations.is_empty());
        assert_eq!(view.error.as_deref(), Some("模型暫時無法使用：維修中"));

        // 冷卻中：叫醒或 load 都不再打模型，對話照樣拿到完整的尾巴。
        f.memory.wake(f.project);
        f.settle().await;
        let history = f
            .memory
            .load(f.project, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(f.requests(format::OBSERVER_INSTRUCTIONS), 1);
        assert_eq!(history.len(), 12);
    }

    #[tokio::test]
    async fn stopping_a_run_does_not_wait_for_the_observer() {
        let cfg = OmConfig {
            block_after: 100,
            ..small()
        };
        let f = fixture(cfg, |request| {
            if request.instructions.as_deref() == Some(format::OBSERVER_INSTRUCTIONS) {
                ScriptedTurn::new().hang()
            } else {
                ScriptedTurn::reply("收到")
            }
        })
        .await;
        f.rounds(0..6).await;
        let cancel = CancellationToken::new();
        let stopper = {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                cancel.cancel();
            })
        };
        let history =
            tokio::time::timeout(Duration::from_secs(2), f.memory.load(f.project, &cancel))
                .await
                .expect("停止後 load 應該馬上回來")
                .unwrap();
        stopper.await.unwrap();
        assert_eq!(f.requests(format::OBSERVER_INSTRUCTIONS), 1);
        assert_eq!(history.len(), 12);
        // 被放棄的那段沒有標成已觀察，也沒有記成錯誤。
        assert!(f.view().await.error.is_none());
        let mut conn = f.store.acquire().await.unwrap();
        assert_eq!(
            Store::om_unobserved(&mut conn, f.project)
                .await
                .unwrap()
                .len(),
            12
        );
    }

    #[tokio::test]
    async fn reflector_may_keep_only_pinned_and_its_failure_does_not_stop_observing() {
        let f = fixture(small(), |request| {
            if request.instructions.as_deref() == Some(format::REFLECTOR_INSTRUCTIONS) {
                ScriptedTurn::reply("目前在做什麼：只剩釘選的")
            } else {
                fake_model(request)
            }
        })
        .await;
        f.rounds(0..6).await;
        f.observe_all().await;
        let ids: Vec<_> = f.view().await.observations.iter().map(|o| o.id).collect();
        let cancel = CancellationToken::new();
        // 還有不少沒釘選的：什麼都不留就是格式不對，記憶不動。
        f.memory.pin(f.project, ids[0], true).await.unwrap();
        assert!(matches!(
            f.memory.inner.reflect_once(f.project, &cancel).await,
            Err(OmError::Format)
        ));
        assert_eq!(f.view().await.generation, 0);
        // 沒釘選的只剩一點：可以一條都不留，新的一代只有釘選的。
        for id in &ids[1..4] {
            f.memory.pin(f.project, *id, true).await.unwrap();
        }
        f.memory.edit(f.project, ids[4], "x".into()).await.unwrap();
        assert!(
            f.memory
                .inner
                .reflect_once(f.project, &cancel)
                .await
                .unwrap()
        );
        let view = f.view().await;
        assert_eq!(view.generation, 1);
        assert_eq!(view.observations.len(), 4);
        assert!(view.observations.iter().all(|o| o.pinned));
        assert_eq!(view.current_task, "只剩釘選的");
        // 全部都釘選：不再反思。
        assert!(
            !f.memory
                .inner
                .reflect_once(f.project, &cancel)
                .await
                .unwrap()
        );

        // 沒有釘選、Reflector 又什麼都沒留：格式不對，但只冷卻 Reflector。
        let g = fixture(small(), |request| {
            if request.instructions.as_deref() == Some(format::REFLECTOR_INSTRUCTIONS) {
                ScriptedTurn::reply("目前在做什麼：清空")
            } else {
                fake_model(request)
            }
        })
        .await;
        g.rounds(0..6).await;
        g.memory.wake(g.project);
        g.settle().await;
        assert_eq!(g.requests(format::REFLECTOR_INSTRUCTIONS), 1);
        let view = g.view().await;
        assert_eq!(view.generation, 0);
        assert_eq!(view.observations.len(), 5);
        assert!(view.error.is_some());
        assert!(g.memory.inner.backing_off(g.project, Stage::Reflect));
        assert!(!g.memory.inner.backing_off(g.project, Stage::Observe));
        // Observer 照常：新的對話還是會被整理，但 Reflector 的錯誤不會被它清掉。
        g.rounds(6..9).await;
        g.memory.wake(g.project);
        g.settle().await;
        assert!(g.requests(format::OBSERVER_INSTRUCTIONS) >= 2);
        assert_eq!(g.requests(format::REFLECTOR_INSTRUCTIONS), 1);
        assert!(g.view().await.observations.len() > 5);
        assert!(g.view().await.error.is_some());
        // 改了模型：冷卻清掉、馬上再試。
        g.memory.retry_all().await;
        g.settle().await;
        assert_eq!(g.requests(format::REFLECTOR_INSTRUCTIONS), 2);
    }

    #[tokio::test]
    async fn bad_output_is_an_error_and_nothing_is_marked() {
        let f = fixture(small(), |_| ScriptedTurn::reply("我不知道要寫什麼")).await;
        f.rounds(0..6).await;
        let cancel = CancellationToken::new();
        let inner = &f.memory.inner;
        let r = inner
            .observe_once(f.project, inner.cfg.observe_after, &cancel)
            .await;
        assert!(matches!(r, Err(OmError::Format)));
        let mut conn = f.store.acquire().await.unwrap();
        assert_eq!(
            Store::om_unobserved(&mut conn, f.project)
                .await
                .unwrap()
                .len(),
            12
        );
    }
}
