//! Watcher：外部事件、等待與自動化（`proposal/events-and-automation.md`）。
//!
//! - **接收**：connector 只負責「收進來、驗證、正規化」（[`SourceConnector`]）；M3 只有
//!   GitHub（[`github`]）。桌面版以輪詢為主，也接受驗簽過的 webhook。
//! - **去重**：`external_events.dedupe_key` 有 unique 索引，webhook 與輪詢收到同一個事件只留一筆。
//! - **路由**（[`Watcher::ingest`]）：先比對訂閱這個資源、處於 `Waiting` 的任務（[`event::wakes`]），
//!   再比對訂閱它的 Goal（listener 開的 Goal：同一個資源回到同一個 Goal），都沒有才比對 listener；
//!   全部沒對上就只留在活動紀錄（`external_events`），不廣播給任何 agent。
//! - **sweeper**（每 `sweep_interval`）：`deadline` 到了的等待以「逾時」喚醒、`At` 與 `Subtasks`
//!   的等待、到期的排程、到期的暫停。
//! - **喚醒**：任務回到 `Planned`，並排一個 `worker` 的 `agent_runs`（觸發寫明在等什麼、結果是
//!   什麼），commit 後叫醒 worker。
//!
//! 排程與 listener 的時間都用 [`Clock`]，測試可以撥時間。

pub mod event;
pub mod github;

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use pots_proto::{
    Card, GithubDraft, GithubView, ItemId, ListenerActionSpec, ProjectId, ServerEvent,
    SourceHealth, SourceView, Timestamp, TrustLevel,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::sqlite::SqliteConnection;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use uuid::Uuid;

pub use event::WaitCond;
use event::{ExternalEvent, wakes};
use github::{Github, GithubConfig};

use crate::Error;
use crate::automation::{self, ListenerDbState, ListenerRow, ScheduleOwner, TriggerRow};
use crate::config::{Clock, WatcherConfig};
use crate::hub::{Hub, LogTxn};
use crate::planner::{self, Planner, PlannerTrigger};
use crate::runs::{self, Runs, Trigger};
use crate::store::{Store, parse_id, to_json};
use crate::tasks::{self, Cause, Principal, TaskRow, TaskState};

/// 連續幾次輪詢失敗才把來源標成異常（token 失效、額度用完則馬上標）。
const FAILURES_BEFORE_ERROR: u32 = 3;
/// 等「下一個」事件（review、留言）時，比開始等待早這麼多秒以內的事件也算（時鐘誤差）。
const WAIT_SLACK_SECS: i64 = 60;
/// listener 開的 Goal 每小時最多因為後續事件叫醒 Planner 幾次。
const MAX_GOAL_EVENTS_PER_HOUR: u32 = 10;
/// 活動紀錄與 webhook 投遞紀錄保留多久。
const EVENT_RETENTION_SECS: i64 = 30 * 86_400;
const DELIVERY_RETENTION_SECS: i64 = 7 * 86_400;

/// connector 支援的接收方式，依偏好排序。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ingest {
    /// webhook
    Push,
    /// 定期拉取
    Poll,
    /// 長連線（IMAP IDLE，之後的郵件 connector）
    Stream,
}

/// 外部來源的介面（`proposal/events-and-automation.md#connector-介面`）。去重、比對、喚醒與
/// listener 共用 [`Watcher`] 的管線，新增來源不必改其他地方。
pub trait SourceConnector: Send + Sync {
    /// `sources.kind`，例如 `github`。
    fn kind(&self) -> &'static str;
    fn ingest(&self) -> &'static [Ingest];
    /// 這個來源的預設信任等級（個別事件可能更低）。
    fn default_trust(&self) -> TrustLevel;
}

impl SourceConnector for Github {
    fn kind(&self) -> &'static str {
        "github"
    }

    fn ingest(&self) -> &'static [Ingest] {
        &[Ingest::Push, Ingest::Poll]
    }

    fn default_trust(&self) -> TrustLevel {
        TrustLevel::Members
    }
}

/// `sources.health` 的內容。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Health {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    since: Option<Timestamp>,
    #[serde(default)]
    last_ok: Option<Timestamp>,
    #[serde(default)]
    failures: u32,
}

/// 事件送去了哪裡（`external_events.matched`）。
#[derive(Debug, Default, Serialize)]
struct Matched {
    tasks: Vec<String>,
    goals: Vec<String>,
    listeners: Vec<String>,
}

impl Matched {
    fn is_empty(&self) -> bool {
        self.tasks.is_empty() && self.goals.is_empty() && self.listeners.is_empty()
    }
}

/// 一輪輪詢收集到的東西。
struct Fetched {
    generation: u64,
    repos: Vec<String>,
    cursor: github::Cursor,
    events: Vec<ExternalEvent>,
    failure: Option<github::GhError>,
}

/// commit 之後要做的事。
#[derive(Default)]
struct After {
    kick_planner: bool,
    /// 結束了的 Goal（快速修改的任務結束）：中斷它其他正在跑的 run。
    closed: Option<pots_proto::GoalId>,
    /// 送給對話 agent 的系統觸發：(project, 對話流裡的項目, 給 agent 的文字)。
    chat: Vec<(ProjectId, ItemId, String)>,
}

struct Inner {
    hub: Arc<Hub>,
    store: Store,
    planner: Planner,
    runs: Runs,
    github: Github,
    config: WatcherConfig,
    poll_now: Notify,
    sweep_now: Notify,
    tracker: TaskTracker,
    shutdown: CancellationToken,
}

#[derive(Clone)]
pub struct Watcher {
    inner: Arc<Inner>,
}

impl Watcher {
    pub fn new(
        hub: Arc<Hub>,
        store: Store,
        planner: Planner,
        runs: Runs,
        data_dir: &std::path::Path,
        config: WatcherConfig,
    ) -> Self {
        let github = Github::load(data_dir, config.github_api.clone());
        Self {
            inner: Arc::new(Inner {
                hub,
                store,
                planner,
                runs,
                github,
                config,
                poll_now: Notify::new(),
                sweep_now: Notify::new(),
                tracker: TaskTracker::new(),
                shutdown: CancellationToken::new(),
            }),
        }
    }

    pub fn clock(&self) -> &Clock {
        &self.inner.config.clock
    }

    pub fn github(&self) -> &Github {
        &self.inner.github
    }

    /// 建立來源列並記一筆目前的 GitHub 設定（啟動時呼叫）。
    pub async fn init(&self) -> Result<(), Error> {
        let mut txn = self.inner.hub.begin().await?;
        ensure_github_source(txn.conn(), &self.inner.github, self.now()).await?;
        let github = self.github_view(txn.conn()).await?;
        txn.push(|seq| ServerEvent::Github { seq, github }).await?;
        txn.commit().await
    }

    pub async fn github_view(&self, conn: &mut SqliteConnection) -> Result<GithubView, Error> {
        Ok(self.inner.github.view(&github_config(conn).await?))
    }

    fn now(&self) -> Timestamp {
        self.inner.config.clock.now()
    }

    /// 開始輪詢與 sweeper。啟動時馬上各跑一次（補上停機期間漏掉的事件與到期的排程）。
    pub fn start(&self) {
        let w = self.clone();
        self.inner.tracker.spawn(async move { w.poll_loop().await });
        let w = self.clone();
        self.inner
            .tracker
            .spawn(async move { w.sweep_loop().await });
    }

    /// 馬上輪詢一次（例如剛設定 token 或 repo）。
    pub fn poll_now(&self) {
        self.inner.poll_now.notify_one();
    }

    /// 馬上掃一次（例如改了排程時間）。
    pub fn sweep_now(&self) {
        self.inner.sweep_now.notify_one();
    }

    pub async fn shutdown(&self, timeout: Duration) {
        self.inner.shutdown.cancel();
        self.inner.tracker.close();
        if tokio::time::timeout(timeout, self.inner.tracker.wait())
            .await
            .is_err()
        {
            tracing::warn!("watcher 沒有在時限內結束");
        }
    }

    async fn poll_loop(&self) {
        loop {
            if let Err(e) = self.poll_once().await {
                tracing::error!(error = %e, "輪詢外部來源失敗");
            }
            tokio::select! {
                () = self.inner.shutdown.cancelled() => return,
                () = self.inner.poll_now.notified() => {}
                () = tokio::time::sleep(self.inner.config.poll_interval) => {}
            }
        }
    }

    async fn sweep_loop(&self) {
        loop {
            if let Err(e) = self.sweep_once().await {
                tracing::error!(error = %e, "sweeper 失敗");
            }
            tokio::select! {
                () = self.inner.shutdown.cancelled() => return,
                () = self.inner.sweep_now.notified() => {}
                () = tokio::time::sleep(self.inner.config.sweep_interval) => {}
            }
        }
    }

    // -----------------------------------------------------------------------
    // 輪詢

    /// 一輪輪詢。打 API 的部分在關閉時直接放棄；寫入（事件處理、進度與健康狀態）不中斷，
    /// 免得 `LogTxn` 停在一半（記憶體裡的序號會落後資料庫）。
    async fn poll_once(&self) -> Result<(), Error> {
        let fetched = tokio::select! {
            () = self.inner.shutdown.cancelled() => return Ok(()),
            f = self.fetch() => f?,
        };
        let Some(Fetched {
            generation,
            repos,
            mut cursor,
            events,
            failure,
        }) = fetched
        else {
            return Ok(());
        };
        let inner = &self.inner;
        // 先處理事件再存進度：中途當掉或失敗時，下一輪會再拿到（處理過的靠去重略過）。
        let ingested = self.ingest(events).await;
        if inner.github.generation() != generation {
            return Ok(());
        }
        let mut txn = inner.hub.begin().await?;
        if ingested {
            // 輪詢期間被移除的 repo 不要寫回進度（之後再加回來時從當時開始）。
            let watched = automation::all_repos(txn.conn()).await?;
            cursor.retain(|r, _| watched.contains(r) && repos.contains(r));
            set_github_cursor(txn.conn(), &cursor).await?;
        }
        self.record_health(&mut txn, failure.map(|e| (e.to_string(), e.is_fatal())))
            .await?;
        txn.commit().await
    }

    /// 打 GitHub API 收集這一輪的事件；沒有 token 時是 `None`。
    async fn fetch(&self) -> Result<Option<Fetched>, Error> {
        let inner = &self.inner;
        if !inner.github.has_token() {
            return Ok(None);
        }
        // 這一輪開始後設定改了（例如換了 token）：結果不算數。
        let generation = inner.github.generation();
        let (config, mut cursor, repos, ci_refs, waits) = {
            let mut conn = inner.store.acquire().await?;
            let config = github_config(&mut conn).await?;
            let cursor = github_cursor(&mut conn).await?;
            let repos = automation::all_repos(&mut conn).await?;
            let ci_refs = ci_branches(&mut conn).await?;
            let waits = waiting_resources(&mut conn).await?;
            (config, cursor, repos, ci_refs, waits)
        };
        // 沒有要看的東西：不打 API，也不把來源標成正常（還沒驗證過 token）。
        if repos.is_empty() && waits.is_empty() {
            return Ok(None);
        }
        let gh = &inner.github;
        let html = gh.html_base_for(&config);
        let mut events = Vec::new();
        let mut failure: Option<github::GhError> = None;
        let note = |e: github::GhError, failure: &mut Option<github::GhError>| {
            tracing::warn!(error = %e, "GitHub 輪詢失敗");
            if failure.as_ref().is_none_or(|f| !f.is_fatal()) {
                *failure = Some(e);
            }
        };
        'repos: for repo in &repos {
            let c = cursor.entry(repo.clone()).or_default();
            match gh.poll_repo(&config, repo, c).await {
                Ok(evs) => events.extend(evs),
                // 打錯的 repo 不算來源異常（token 沒問題），只記在日誌。
                Err(github::GhError::NotFound(_)) => {
                    tracing::warn!(%repo, "GitHub 找不到這個 repo（或 token 沒有權限）");
                    continue;
                }
                Err(e) => {
                    let fatal = e.is_fatal();
                    note(e, &mut failure);
                    if fatal {
                        break 'repos;
                    }
                    continue;
                }
            }
            // listener 要看的分支的 CI。第一次看到某個分支時只記下目前的 head（還在跑時記成
            // 「?」，之後第一次結束照常發事件），不把舊的結果當新事件。
            for branch in ci_refs.get(repo).into_iter().flatten() {
                let branch = match branch {
                    Some(b) => b.clone(),
                    None => match cursor.get(repo).and_then(|c| c.default_branch.clone()) {
                        Some(b) => b,
                        None => match gh.default_branch(&config, repo).await {
                            Ok(b) => {
                                cursor.entry(repo.clone()).or_default().default_branch =
                                    Some(b.clone());
                                b
                            }
                            Err(github::GhError::NotFound(_)) => continue,
                            Err(e) => {
                                note(e, &mut failure);
                                continue;
                            }
                        },
                    },
                };
                match gh.ci_status(&config, repo, &branch).await {
                    Ok(Some(ci)) => {
                        let c = cursor.entry(repo.clone()).or_default();
                        let last = c.branches.insert(branch.clone(), ci.sha.clone());
                        if last.is_some_and(|l| l != ci.sha) {
                            events.push(github::ci_event(repo, &ci, &html, Some(&branch)));
                        }
                    }
                    Ok(None) => {
                        let c = cursor.entry(repo.clone()).or_default();
                        c.branches.entry(branch).or_insert_with(|| "?".into());
                    }
                    Err(github::GhError::NotFound(_)) => {}
                    Err(e) => note(e, &mut failure),
                }
            }
        }
        // 任務在等的資源：每一輪都查現況，重啟或漏掉事件時靠它補上。
        if failure.as_ref().is_none_or(|f| !f.is_fatal()) {
            for (resource, since) in &waits {
                match gh.snapshot(&config, resource, *since).await {
                    // 從現況推出來的事件只用來喚醒等待的任務，不給 Goal 或 listener。
                    Ok(evs) => events.extend(evs.into_iter().map(|mut e| {
                        e.backfill = true;
                        e
                    })),
                    Err(github::GhError::NotFound(_)) => {}
                    Err(e) => {
                        let fatal = e.is_fatal();
                        note(e, &mut failure);
                        if fatal {
                            break;
                        }
                    }
                }
            }
        }
        Ok(Some(Fetched {
            generation,
            repos,
            cursor,
            events,
            failure,
        }))
    }

    /// 記下這一輪的結果；狀態改變（未知、正常、異常）時記一筆 `Automation`，變成異常時在監看 repo
    /// 的 project 對話裡貼一張通知，不會默默停止收事件。
    async fn record_health(
        &self,
        txn: &mut LogTxn<'_>,
        failure: Option<(String, bool)>,
    ) -> Result<(), Error> {
        let now = self.now();
        let mut h = source_health(txn.conn()).await?;
        let before = (h.error.is_some(), h.last_ok.is_some());
        match failure {
            None => {
                h.error = None;
                h.since = None;
                h.failures = 0;
                h.last_ok = Some(now);
            }
            Some((message, fatal)) => {
                h.failures += 1;
                if fatal || h.failures >= FAILURES_BEFORE_ERROR {
                    if h.error.is_none() {
                        h.since = Some(now);
                    }
                    h.error = Some(message);
                }
            }
        }
        let after = (h.error.is_some(), h.last_ok.is_some());
        sqlx::query("UPDATE sources SET health = ?, updated_at = ? WHERE id = ?")
            .bind(to_json(&h)?)
            .bind(now.0)
            .bind(github::SOURCE_ID)
            .execute(txn.conn())
            .await?;
        if before != after {
            if !before.0
                && let Some(message) = &h.error
            {
                let projects: Vec<String> =
                    sqlx::query_scalar("SELECT DISTINCT project_id FROM project_repos")
                        .fetch_all(txn.conn())
                        .await?;
                for p in projects {
                    let project: ProjectId = parse_id(p)?;
                    crate::tools::post_card(
                        txn,
                        project,
                        Card::Notice {
                            title: "GitHub 來源異常".into(),
                            body: format!("暫時收不到 GitHub 的事件：{message}。請到「設定 → GitHub」檢查 token。"),
                            actions: vec![pots_proto::NoticeAction::ViewAutomations],
                        },
                        None,
                    )
                    .await?;
                }
            }
            automation::push(txn, now).await?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // webhook

    /// `POST /webhooks/github`：驗簽、以 delivery ID 去重，正規化後在背景處理。
    pub async fn github_webhook(&self, headers: &HeaderMap, body: &[u8]) -> (StatusCode, String) {
        let Some(secret) = self.inner.github.webhook_secret() else {
            return (StatusCode::NOT_FOUND, "沒有設定 GitHub webhook".into());
        };
        let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
        if !github::verify_signature(&secret, body, header("x-hub-signature-256")) {
            return (StatusCode::UNAUTHORIZED, "簽章不符".into());
        }
        let event = header("x-github-event").unwrap_or_default().to_string();
        // GitHub 一定會帶 delivery ID；沒有的話無法去重重送，直接拒絕。
        let Some(delivery) = header("x-github-delivery")
            .map(|d| d.chars().take(100).collect::<String>())
            .filter(|d| !d.is_empty())
        else {
            return (StatusCode::BAD_REQUEST, "缺少 X-GitHub-Delivery".into());
        };
        if self.inner.shutdown.is_cancelled() {
            return (StatusCode::SERVICE_UNAVAILABLE, "服務正在關閉".into());
        }
        {
            let fresh = async {
                let mut tx = self.inner.store.begin().await?;
                let done = sqlx::query(
                    "INSERT OR IGNORE INTO source_deliveries (source_id, delivery_id, at) VALUES (?, ?, ?)",
                )
                .bind(github::SOURCE_ID)
                .bind(&delivery)
                .bind(self.now().0)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok::<_, Error>(done.rows_affected() > 0)
            }
            .await;
            match fresh {
                Ok(true) => {}
                Ok(false) => return (StatusCode::OK, "重複的投遞".into()),
                Err(e) => {
                    tracing::error!(error = %e, "無法記錄 webhook 投遞");
                    return (StatusCode::INTERNAL_SERVER_ERROR, "伺服器內部錯誤".into());
                }
            }
        }
        if event == "ping" {
            return (StatusCode::OK, "pong".into());
        }
        let Ok(payload) = serde_json::from_slice::<serde_json::Value>(body) else {
            return (StatusCode::BAD_REQUEST, "內容不是 JSON".into());
        };
        let w = self.clone();
        self.inner.tracker.spawn(async move {
            let config = match w.inner.store.acquire().await {
                Ok(mut conn) => github_config(&mut conn).await.unwrap_or_default(),
                Err(_) => GithubConfig::default(),
            };
            let html = w.inner.github.html_base_for(&config);
            let pushed = github::normalize(&event, &payload);
            let mut events = pushed.events;
            let mut ok = true;
            for (repo, sha) in pushed.ci_checks {
                match w.inner.github.ci_status(&config, &repo, &sha).await {
                    // webhook 的 CI 只喚醒等這個 sha 的任務；分支 head 的 CI 由輪詢確認。
                    Ok(Some(ci)) => events.push(github::ci_event(&repo, &ci, &html, None)),
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "無法查詢 CI 彙總");
                        ok = false;
                    }
                }
            }
            // 沒處理完的由輪詢與補漏接手；不撤銷投遞紀錄（簽章沒有時間，撤銷等於允許重放）。
            if !(w.ingest(events).await && ok) {
                tracing::warn!(%delivery, "webhook 沒有處理完，交給輪詢補上");
            }
        });
        (StatusCode::ACCEPTED, "已收到".into())
    }

    // -----------------------------------------------------------------------
    // 路由

    /// 依時間順序處理一批事件；全部成功時回 `true`。
    pub async fn ingest(&self, mut events: Vec<ExternalEvent>) -> bool {
        events.sort_by_key(|e| e.occurred_at);
        let mut ok = true;
        for ev in events {
            if let Err(e) = self.ingest_one(ev).await {
                tracing::error!(error = %e, "無法處理外部事件");
                ok = false;
            }
        }
        ok
    }

    async fn ingest_one(&self, ev: ExternalEvent) -> Result<(), Error> {
        let inner = &self.inner;
        let now = self.now();
        let mut txn = inner.hub.begin().await?;
        let id = Uuid::now_v7();
        let inserted = sqlx::query(
            "INSERT OR IGNORE INTO external_events (id, source_id, dedupe_key, kind, resource, trust, \
             payload, occurred_at, received_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(&ev.source)
        .bind(&ev.dedupe_key)
        .bind(ev.kind.name())
        .bind(&ev.resource)
        .bind(trust_str(ev.trust))
        .bind(to_json(&ev)?)
        .bind(ev.occurred_at.0)
        .bind(now.0)
        .execute(txn.conn())
        .await?;
        // 已經記過的事件（例如等待開始前 CI 就結束了，補漏時又查到）：只用來喚醒還在等的任務，
        // 不再送給 Goal 或 listener。
        let fresh = inserted.rows_affected() > 0 && !ev.backfill;
        let mut matched = Matched::default();
        let mut after = After::default();
        // 已經有對象處理這個事件的 project；它們的 listener 不再看。
        let mut routed: HashSet<ProjectId> = HashSet::new();
        let pauses = automation::pauses(txn.conn()).await?;

        // 1. 在等這個資源的任務與 Goal。
        let subs = sqlx::query(
            "SELECT task_id, goal_id, project_id, created_at FROM subscriptions WHERE resource_key = ? \
             ORDER BY created_at",
        )
        .bind(&ev.resource)
        .fetch_all(txn.conn())
        .await?;
        for row in &subs {
            let task: Option<String> = row.try_get("task_id")?;
            let goal: Option<String> = row.try_get("goal_id")?;
            let project: ProjectId = parse_id(row.try_get("project_id")?)?;
            if let Some(task) = task {
                let Some(t) = tasks::task(txn.conn(), parse_id(task)?).await? else {
                    continue;
                };
                let since = Timestamp(row.try_get("created_at")?);
                if let TaskState::Waiting { on, .. } = &t.state
                    && wakes(on, &ev)
                    && (on.is_state() || ev.occurred_at.0 >= since.0 - WAIT_SLACK_SECS)
                {
                    let on = on.clone();
                    if ci_result(&mut txn, &t, &on, &ev).await? {
                        after.kick_planner = true;
                    }
                    matched.tasks.push(t.id.to_string());
                    routed.insert(project);
                }
            } else if let Some(goal) = goal
                && fresh
            {
                let goal: pots_proto::GoalId = parse_id(goal)?;
                if !tasks::goal(txn.conn(), goal)
                    .await?
                    .is_some_and(|g| g.state.is_active())
                {
                    continue;
                }
                // 這個資源的事件歸這個 Goal（不再開新的），但暫停中或太頻繁時不叫醒 Planner。
                routed.insert(project);
                if automation::is_paused(&pauses, Some(project), now)
                    || goal_events_since(txn.conn(), goal, Timestamp(now.0 - 3600)).await?
                        >= MAX_GOAL_EVENTS_PER_HOUR
                {
                    continue;
                }
                planner::enqueue(
                    txn.conn(),
                    project,
                    goal,
                    &PlannerTrigger::External {
                        // 任何人都能寫的內容只給摘要。
                        event: ev.render_for_agent(ev.trust != TrustLevel::Public),
                    },
                )
                .await?;
                after.kick_planner = true;
                matched.goals.push(goal.to_string());
            }
        }

        // 2. 還沒有對象的 project 才看 listener。
        if fresh && let Some(repo) = ev.repo() {
            let projects = automation::projects_watching(txn.conn(), repo).await?;
            let mut fired = false;
            for l in automation::listeners(txn.conn()).await? {
                if l.state != ListenerDbState::Active
                    || !projects.contains(&l.spec.project)
                    || routed.contains(&l.spec.project)
                    || !automation::matches(&l.spec, &ev)
                    || automation::is_paused(&pauses, Some(l.spec.project), now)
                {
                    continue;
                }
                self.fire_listener(&mut txn, &l, &ev, id, now, &mut after)
                    .await?;
                matched.listeners.push(l.id.to_string());
                fired = true;
            }
            if fired {
                automation::push(&mut txn, now).await?;
            }
        }

        if fresh && !matched.is_empty() {
            sqlx::query("UPDATE external_events SET matched = ? WHERE id = ?")
                .bind(to_json(&matched)?)
                .bind(id.to_string())
                .execute(txn.conn())
                .await?;
        }
        self.finish(txn, after).await
    }

    async fn finish(&self, txn: LogTxn<'_>, after: After) -> Result<(), Error> {
        let planner = &self.inner.planner;
        txn.commit_then(|| {
            if after.kick_planner {
                planner.kick();
            }
            if let Some(g) = after.closed {
                planner.worker().close_goal(g);
            }
            // 可能有任務被喚醒或快速修改排了 worker。
            planner.worker().kick();
        })
        .await?;
        for (project, item, text) in after.chat {
            self.inner
                .runs
                .enqueue(project, Trigger { item, text }, false);
        }
        Ok(())
    }

    async fn fire_listener(
        &self,
        txn: &mut LogTxn<'_>,
        l: &ListenerRow,
        ev: &ExternalEvent,
        event_id: Uuid,
        now: Timestamp,
        after: &mut After,
    ) -> Result<(), Error> {
        let project = l.spec.project;
        let title = ev.summary.clone();
        // 重新啟用後重新計算，不算暫停前的觸發。
        let since = Timestamp((now.0 - 3600).max(l.updated_at.0 - 1));
        let recent = automation::listener_triggers_since(txn.conn(), l.id, since).await?;
        if recent >= l.spec.max_per_hour {
            automation::set_listener_state(txn.conn(), l.id, ListenerDbState::Throttled, now)
                .await?;
            automation::record_trigger(
                txn.conn(),
                &TriggerRow {
                    listener: Some(l.id),
                    project: Some(project),
                    event: Some(event_id),
                    title,
                    status: "skipped",
                    error: Some(format!("一小時內超過 {} 次，已暫停", l.spec.max_per_hour)),
                    ..Default::default()
                },
                now,
            )
            .await?;
            crate::tools::post_card(
                txn,
                project,
                Card::Notice {
                    title: "listener 已暫停".into(),
                    body: format!(
                        "「{}」一小時內觸發超過 {} 次，先暫停了。確認沒問題後到自動化頁重新啟用。",
                        automation::listener_title(&l.spec),
                        l.spec.max_per_hour
                    ),
                    actions: vec![pots_proto::NoticeAction::ViewAutomations],
                },
                None,
            )
            .await?;
            return Ok(());
        }
        let mut action = l.spec.action.clone();
        // Public 事件要有作者（author）白名單才能自動開工（check_spec 已擋，這裡再保險一次）。
        let whitelisted = automation::has_author_whitelist(&l.spec);
        if ev.trust == TrustLevel::Public && !whitelisted {
            action = ListenerActionSpec::NotifyChat;
        }
        match action {
            ListenerActionSpec::CreateGoal { template }
            | ListenerActionSpec::QuickFix { template } => {
                let quick = matches!(l.spec.action, ListenerActionSpec::QuickFix { .. });
                if quick && let Some(repo) = ev.repo() {
                    let description = format!(
                        "{}\n\n## 觸發事件\n\n{}",
                        template.trim(),
                        ev.render_for_agent(ev.trust != TrustLevel::Public || whitelisted)
                    );
                    let goal_title = tasks::one_line(&ev.summary, tasks::MAX_TITLE_CHARS);
                    let opened = crate::worker::open_quick(
                        txn,
                        project,
                        &goal_title,
                        &description,
                        repo,
                        &Principal::System,
                        None,
                    )
                    .await?;
                    match opened {
                        // 快速修改沒有 Planner，不訂閱資源（之後的事件不會送給它）。
                        Ok((goal, _)) => {
                            crate::tools::post_card(
                                txn,
                                project,
                                Card::Triggered {
                                    listener: l.id,
                                    goal,
                                    event: ev.summary.clone(),
                                },
                                None,
                            )
                            .await?;
                            automation::record_trigger(
                                txn.conn(),
                                &TriggerRow {
                                    listener: Some(l.id),
                                    project: Some(project),
                                    goal: Some(goal),
                                    event: Some(event_id),
                                    title: format!("{title} → 快速修改"),
                                    status: "done",
                                    ..Default::default()
                                },
                                now,
                            )
                            .await?;
                        }
                        Err(e) => {
                            automation::record_trigger(
                                txn.conn(),
                                &TriggerRow {
                                    listener: Some(l.id),
                                    project: Some(project),
                                    event: Some(event_id),
                                    title,
                                    status: "failed",
                                    error: Some(e),
                                    ..Default::default()
                                },
                                now,
                            )
                            .await?;
                        }
                    }
                    return Ok(());
                }
                // 事件沒有 repo 時快速修正沒辦法派 worker：改開一般 Goal（觸發紀錄註明）。
                let fallback = quick;
                let brief = format!(
                    "## 目標\n\n{}\n\n## 觸發事件\n\n{}",
                    template.trim(),
                    ev.render_for_agent(ev.trust != TrustLevel::Public || whitelisted)
                );
                let goal_title = tasks::one_line(&ev.summary, tasks::MAX_TITLE_CHARS);
                let opened = crate::tools::open_goal(
                    txn,
                    project,
                    &goal_title,
                    &brief,
                    &Principal::System,
                    &format!("listener {}", automation::listener_title(&l.spec)),
                    None,
                )
                .await?;
                match opened {
                    Ok(goal) => {
                        tasks::push_goal(txn, goal).await?;
                        subscribe_goal(txn.conn(), project, goal, &ev.resource, now).await?;
                        crate::tools::post_card(
                            txn,
                            project,
                            Card::Triggered {
                                listener: l.id,
                                goal,
                                event: ev.summary.clone(),
                            },
                            None,
                        )
                        .await?;
                        automation::record_trigger(
                            txn.conn(),
                            &TriggerRow {
                                listener: Some(l.id),
                                project: Some(project),
                                goal: Some(goal),
                                event: Some(event_id),
                                title: if fallback {
                                    format!("{title} → 開 Goal（事件沒有 repo，不能快速修正）")
                                } else {
                                    format!("{title} → 開 Goal")
                                },
                                status: "done",
                                ..Default::default()
                            },
                            now,
                        )
                        .await?;
                        after.kick_planner = true;
                    }
                    Err(e) => {
                        automation::record_trigger(
                            txn.conn(),
                            &TriggerRow {
                                listener: Some(l.id),
                                project: Some(project),
                                event: Some(event_id),
                                title,
                                status: "failed",
                                error: Some(e),
                                ..Default::default()
                            },
                            now,
                        )
                        .await?;
                    }
                }
            }
            ListenerActionSpec::NotifyChat => {
                let text = format!(
                    "（系統通知，不是使用者說的話）listener「{}」收到外部事件。看看需不需要處理；\
                     要開工時和使用者確認後用 create_goal。\n\n{}",
                    automation::listener_title(&l.spec),
                    // 對話歷史只放平台產生的摘要與連結，不放他人寫的內容（會進記憶）。
                    ev.render_summary()
                );
                let item = runs::post_system_trigger(
                    txn,
                    project,
                    &format!("外部事件：{}", ev.summary),
                    &text,
                )
                .await?;
                after.chat.push((project, item, text));
                automation::record_trigger(
                    txn.conn(),
                    &TriggerRow {
                        listener: Some(l.id),
                        project: Some(project),
                        event: Some(event_id),
                        title,
                        status: "notified",
                        ..Default::default()
                    },
                    now,
                )
                .await?;
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // sweeper

    async fn sweep_once(&self) -> Result<(), Error> {
        let inner = &self.inner;
        let now = self.now();
        let mut txn = inner.hub.begin().await?;
        let mut after = After::default();
        // 到期前先讀：到期的暫停期間錯過的排程也不補跑（以排程原本的時間判斷）。
        let pauses = automation::pauses(txn.conn()).await?;
        let mut changed = automation::expire_pauses(txn.conn(), now).await? > 0;
        prune(txn.conn(), now).await?;

        // 逾時的核准：在等它的任務轉 `NeedsHuman`（不是「逾時」喚醒）。
        let ends = crate::approvals::expire_due(&mut txn, now).await?;
        after.kick_planner |= ends.planner;
        after.closed = after.closed.or(ends.closed);

        // 等待中的任務：時間、子任務與逾時。
        let waiting: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM tasks WHERE state LIKE '{\"state\":\"Waiting\"%' ORDER BY created_at",
        )
        .fetch_all(txn.conn())
        .await?;
        for id in waiting {
            let Some(t) = tasks::task(txn.conn(), parse_id(id)?).await? else {
                continue;
            };
            let TaskState::Waiting { on, deadline } = t.state.clone() else {
                continue;
            };
            // 等核准的由上面的 `expire_due` 處理。
            if matches!(on, WaitCond::Approval { .. }) {
                continue;
            }
            let result = match &on {
                WaitCond::At { at } if at.0 <= now.0 => Some("時間到了".to_string()),
                WaitCond::Subtasks { tasks: ids } => {
                    let mut all_closed = true;
                    for sub in ids {
                        if tasks::task(txn.conn(), *sub)
                            .await?
                            .is_some_and(|s| s.state.is_open())
                        {
                            all_closed = false;
                            break;
                        }
                    }
                    all_closed.then(|| "子任務都結束了".to_string())
                }
                _ => None,
            };
            let result = result.or_else(|| (deadline.0 <= now.0).then(|| "逾時".to_string()));
            if let Some(result) = result {
                wake_task(&mut txn, &t, &on, &result).await?;
            }
        }

        // 到期的排程：錯過幾次都只補跑一次（下一次從現在算）；暫停期間錯過的不補跑。
        for s in automation::due_schedules(txn.conn(), now).await? {
            changed = true;
            if s.expires_at.is_some_and(|e| e.0 < s.next_at.0) {
                automation::advance_schedule(txn.conn(), s.id, None, now).await?;
                continue;
            }
            let next = s.spec.after(now, s.expires_at);
            let project = s.owner.project();
            let mut record = TriggerRow {
                schedule: Some(s.id),
                project,
                title: tasks::one_line(&s.prompt, 120),
                ..Default::default()
            };
            if automation::is_paused(&pauses, project, now)
                || automation::was_paused_at(&pauses, project, s.next_at)
            {
                record.status = "skipped";
                record.error = Some("暫停期間略過".into());
                automation::record_trigger(txn.conn(), &record, now).await?;
                automation::advance_schedule(txn.conn(), s.id, next, now).await?;
                continue;
            }
            match s.owner {
                ScheduleOwner::Chat(project) => {
                    let text = format!(
                        "（排程觸發，不是使用者說的話）你之前設定的排程時間到了，請照著做：\n\n{}",
                        s.prompt
                    );
                    let item = runs::post_system_trigger(
                        &mut txn,
                        project,
                        &format!("排程觸發：{}", tasks::one_line(&s.prompt, 200)),
                        &text,
                    )
                    .await?;
                    after.chat.push((project, item, text));
                    record.status = "done";
                }
                ScheduleOwner::Planner { project, goal } => {
                    let active = tasks::goal(txn.conn(), goal)
                        .await?
                        .is_some_and(|g| g.state.is_active());
                    if active {
                        planner::enqueue(
                            txn.conn(),
                            project,
                            goal,
                            &PlannerTrigger::Scheduled {
                                prompt: s.prompt.clone(),
                            },
                        )
                        .await?;
                        after.kick_planner = true;
                        record.goal = Some(goal);
                        record.status = "done";
                    } else {
                        record.status = "failed";
                        record.error = Some("Goal 已經結束".into());
                        automation::record_trigger(txn.conn(), &record, now).await?;
                        automation::advance_schedule(txn.conn(), s.id, None, now).await?;
                        continue;
                    }
                }
                ScheduleOwner::Global => {
                    record.status = "failed";
                    record.error = Some("總管 agent 尚未實作（M6）".into());
                }
            }
            automation::record_trigger(txn.conn(), &record, now).await?;
            automation::advance_schedule(txn.conn(), s.id, next, now).await?;
        }
        if changed {
            automation::push(&mut txn, now).await?;
        }
        self.finish(txn, after).await
    }
}

/// 一次 CI 失敗最多自動修幾次：連續失敗到第這一次就轉 `NeedsHuman`。
pub const MAX_CI_FAILURES: u32 = 3;

/// 事件滿足了任務的等待。CI 失敗時叫醒 worker 自己修（帶著第幾次），連續失敗到
/// [`MAX_CI_FAILURES`] 次時任務轉 `NeedsHuman`；CI 通過時歸零。回傳要不要叫醒 Planner。
async fn ci_result(
    txn: &mut LogTxn<'_>,
    task: &TaskRow,
    on: &WaitCond,
    ev: &ExternalEvent,
) -> Result<bool, Error> {
    // 只有 worker 等 CI（`ci_finished`）才算自動修；用 `resource` 等到 CI 事件的照常喚醒。
    let (WaitCond::CiFinished { .. }, event::EventKind::CiFinished { conclusion, .. }) =
        (on, &ev.kind)
    else {
        wake_task(txn, task, on, &ev.summary).await?;
        return Ok(false);
    };
    if conclusion == "success" {
        tasks::reset_ci_failures(txn.conn(), task.id).await?;
        wake_task(
            txn,
            task,
            on,
            &format!(
                "{}。沒有其他要改的話，用 request_approval 申請 merge",
                ev.summary
            ),
        )
        .await?;
        return Ok(false);
    }
    let n = tasks::bump_ci_failures(txn.conn(), task.id).await?;
    let link = ev
        .links
        .first()
        .map(|l| format!("（{l}）"))
        .unwrap_or_default();
    if n >= MAX_CI_FAILURES {
        crate::worker::end_task(
            txn,
            task,
            crate::worker::TaskEnd::NeedsHuman(format!(
                "CI 連續失敗 {n} 次，自動修復停止：{}{link}",
                ev.summary
            )),
            &Principal::System,
            None,
        )
        .await
        .map(|e| e.planner)
    } else {
        wake_task(
            txn,
            task,
            on,
            &format!(
                "{}{link}。這是第 {n} 次 CI 失敗（連續 {MAX_CI_FAILURES} 次就交給使用者）：找出原因（在工作目錄跑測試）、修好、commit，\
                 再 open_pr 與 wait_for 等 CI",
                ev.summary
            ),
        )
        .await?;
        Ok(false)
    }
}

/// 喚醒等待中的任務：回到 `Planned`、清掉訂閱，排一個 worker 的 run（呼叫端 commit 後叫醒 worker）。
pub async fn wake_task(
    txn: &mut LogTxn<'_>,
    task: &TaskRow,
    waited: &WaitCond,
    result: &str,
) -> Result<(), Error> {
    let Some(goal) = tasks::goal(txn.conn(), task.goal).await? else {
        return Ok(());
    };
    let reason = format!("{}：{}", waited.describe(), tasks::one_line(result, 300));
    tasks::set_task_state(
        txn.conn(),
        task,
        &TaskState::Planned,
        &Cause {
            by: Principal::System,
            reason: reason.clone(),
        },
    )
    .await?;
    sqlx::query("DELETE FROM subscriptions WHERE task_id = ?")
        .bind(task.id.to_string())
        .execute(txn.conn())
        .await?;
    crate::worker::enqueue(
        txn.conn(),
        goal.project,
        goal.id,
        task.id,
        &crate::worker::WorkerTrigger::Wake {
            waited: waited.clone(),
            result: result.to_string(),
        },
    )
    .await?;
    tasks::push_goal(txn, goal.id).await
}

/// 讓任務等待（`wait_for`）：寫入 `Waiting` 與訂閱。
pub async fn suspend_task(
    txn: &mut LogTxn<'_>,
    task: &TaskRow,
    project: ProjectId,
    on: WaitCond,
    deadline: Timestamp,
    by: &Principal,
    now: Timestamp,
) -> Result<(), Error> {
    let key = on.subscription_key();
    let reason = on.describe();
    tasks::set_task_state(
        txn.conn(),
        task,
        &TaskState::Waiting { on, deadline },
        &Cause {
            by: by.clone(),
            reason,
        },
    )
    .await?;
    if let Some(key) = key {
        sqlx::query(
            "INSERT OR IGNORE INTO subscriptions (id, project_id, task_id, resource_key, created_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(project.to_string())
        .bind(task.id.to_string())
        .bind(key)
        .bind(now.0)
        .execute(txn.conn())
        .await?;
    }
    tasks::push_goal(txn, task.goal).await
}

/// 這個 Goal 在 `since` 之後因為後續事件被叫醒了幾次。
async fn goal_events_since(
    conn: &mut SqliteConnection,
    goal: pots_proto::GoalId,
    since: Timestamp,
) -> Result<u32, Error> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM external_events WHERE received_at > ? AND matched LIKE ?",
    )
    .bind(since.0)
    .bind(format!("%\"goals\":[%\"{goal}\"%"))
    .fetch_one(conn)
    .await?;
    Ok(n as u32)
}

/// 清掉太舊的活動紀錄與 webhook 投遞紀錄。
async fn prune(conn: &mut SqliteConnection, now: Timestamp) -> Result<(), Error> {
    sqlx::query("DELETE FROM external_events WHERE received_at < ?")
        .bind(now.0 - EVENT_RETENTION_SECS)
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM source_deliveries WHERE at < ?")
        .bind(now.0 - DELIVERY_RETENTION_SECS)
        .execute(conn)
        .await?;
    Ok(())
}

async fn subscribe_goal(
    conn: &mut SqliteConnection,
    project: ProjectId,
    goal: pots_proto::GoalId,
    resource: &str,
    now: Timestamp,
) -> Result<(), Error> {
    sqlx::query(
        "INSERT OR IGNORE INTO subscriptions (id, project_id, goal_id, resource_key, created_at) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(project.to_string())
    .bind(goal.to_string())
    .bind(resource)
    .bind(now.0)
    .execute(conn)
    .await?;
    Ok(())
}

fn trust_str(t: TrustLevel) -> &'static str {
    match t {
        TrustLevel::Owned => "owned",
        TrustLevel::Members => "members",
        TrustLevel::Public => "public",
    }
}

async fn ensure_github_source(
    conn: &mut SqliteConnection,
    gh: &Github,
    now: Timestamp,
) -> Result<(), Error> {
    use SourceConnector as _;
    sqlx::query(
        "INSERT OR IGNORE INTO sources (id, kind, name, config, trust, ingest, created_at, updated_at) \
         VALUES (?, ?, ?, '{}', ?, ?, ?, ?)",
    )
    .bind(github::SOURCE_ID)
    .bind(gh.kind())
    .bind("GitHub")
    .bind(trust_str(gh.default_trust()))
    .bind(to_json(&gh.ingest())?)
    .bind(now.0)
    .bind(now.0)
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn github_config(conn: &mut SqliteConnection) -> Result<GithubConfig, Error> {
    let text: Option<String> = sqlx::query_scalar("SELECT config FROM sources WHERE id = ?")
        .bind(github::SOURCE_ID)
        .fetch_optional(conn)
        .await?;
    Ok(text
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default())
}

pub async fn set_github_config(
    conn: &mut SqliteConnection,
    config: &GithubConfig,
    now: Timestamp,
) -> Result<(), Error> {
    sqlx::query("UPDATE sources SET config = ?, updated_at = ? WHERE id = ?")
        .bind(to_json(config)?)
        .bind(now.0)
        .bind(github::SOURCE_ID)
        .execute(conn)
        .await?;
    Ok(())
}

async fn github_cursor(conn: &mut SqliteConnection) -> Result<github::Cursor, Error> {
    let text: Option<String> = sqlx::query_scalar("SELECT cursor FROM sources WHERE id = ?")
        .bind(github::SOURCE_ID)
        .fetch_optional(conn)
        .await?;
    Ok(text
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default())
}

async fn set_github_cursor(
    conn: &mut SqliteConnection,
    cursor: &github::Cursor,
) -> Result<(), Error> {
    sqlx::query("UPDATE sources SET cursor = ? WHERE id = ?")
        .bind(to_json(cursor)?)
        .bind(github::SOURCE_ID)
        .execute(conn)
        .await?;
    Ok(())
}

/// 只留還在監看的 repo 的輪詢進度。
pub async fn prune_cursor(conn: &mut SqliteConnection, watched: &[String]) -> Result<(), Error> {
    let mut cursor = github_cursor(conn).await?;
    let before = cursor.len();
    cursor.retain(|repo, _| watched.contains(repo));
    if cursor.len() != before {
        set_github_cursor(conn, &cursor).await?;
    }
    Ok(())
}

async fn source_health(conn: &mut SqliteConnection) -> Result<Health, Error> {
    let text: Option<String> = sqlx::query_scalar("SELECT health FROM sources WHERE id = ?")
        .bind(github::SOURCE_ID)
        .fetch_optional(conn)
        .await?;
    Ok(text
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default())
}

/// listener 要看 CI 的分支：每個 repo 的 `branch` 欄位值；`None` 是 repo 的預設分支
/// （沒有指定分支的 listener）。
async fn ci_branches(
    conn: &mut SqliteConnection,
) -> Result<BTreeMap<String, Vec<Option<String>>>, Error> {
    let mut out: BTreeMap<String, Vec<Option<String>>> = BTreeMap::new();
    for l in automation::listeners(conn).await? {
        if l.state != ListenerDbState::Active || !l.spec.kinds.iter().any(|k| k == "ci.finished") {
            continue;
        }
        let branches: Vec<Option<String>> = l
            .spec
            .fields
            .iter()
            .find(|f| f.field == "branch")
            .map(|f| f.any_of.iter().cloned().map(Some).collect())
            .unwrap_or_else(|| vec![None]);
        for repo in automation::project_repos(conn, l.spec.project).await? {
            let list = out.entry(repo).or_default();
            for b in &branches {
                if !list.contains(b) {
                    list.push(b.clone());
                }
            }
        }
    }
    Ok(out)
}

/// 等待中的任務訂閱的資源與最早的訂閱時間。
async fn waiting_resources(conn: &mut SqliteConnection) -> Result<Vec<(String, Timestamp)>, Error> {
    let rows = sqlx::query(
        "SELECT s.resource_key AS key, MIN(s.created_at) AS since FROM subscriptions s \
         JOIN tasks t ON t.id = s.task_id WHERE t.state LIKE '{\"state\":\"Waiting\"%' \
         GROUP BY s.resource_key ORDER BY since",
    )
    .fetch_all(conn)
    .await?;
    rows.iter()
        .map(|r| Ok((r.try_get("key")?, Timestamp(r.try_get("since")?))))
        .collect()
}

/// 自動化頁的來源清單。
pub async fn source_views(conn: &mut SqliteConnection) -> Result<Vec<SourceView>, Error> {
    let rows = sqlx::query(
        "SELECT id, kind, name, trust, ingest, health, enabled FROM sources ORDER BY id",
    )
    .fetch_all(conn)
    .await?;
    let mut out = Vec::new();
    for row in &rows {
        let health: String = row.try_get("health")?;
        let health: Health = serde_json::from_str(&health).unwrap_or_default();
        let trust: String = row.try_get("trust")?;
        let ingest: String = row.try_get("ingest")?;
        let ingest: Vec<Ingest> = serde_json::from_str(&ingest).unwrap_or_default();
        out.push(SourceView {
            id: row.try_get("id")?,
            kind: match row.try_get::<String, _>("kind")?.as_str() {
                "github" => "GitHub".into(),
                k => k.to_string(),
            },
            name: row.try_get("name")?,
            trust: match trust.as_str() {
                "owned" => TrustLevel::Owned,
                "public" => TrustLevel::Public,
                _ => TrustLevel::Members,
            },
            ingest: ingest
                .iter()
                .map(|i| match i {
                    Ingest::Push => "webhook",
                    Ingest::Poll => "輪詢",
                    Ingest::Stream => "長連線",
                })
                .collect::<Vec<_>>()
                .join("、"),
            health: match (health.error, health.since, health.last_ok) {
                (Some(message), since, _) => SourceHealth::Error {
                    message,
                    since: since.unwrap_or(Timestamp(0)),
                },
                (None, _, Some(_)) => SourceHealth::Ok,
                (None, _, None) => SourceHealth::Unknown,
            },
            last_ok: health.last_ok,
            enabled: row.try_get::<i64, _>("enabled")? != 0,
        });
    }
    Ok(out)
}

/// 設定視窗的 GitHub 設定：儲存後記一筆 `Github`，並馬上輪詢一次。
pub async fn save_github(watcher: &Watcher, hub: &Hub, draft: GithubDraft) -> Result<(), String> {
    let result: Result<Result<(), String>, Error> = async {
        let mut txn = hub.begin().await?;
        let config = github_config(txn.conn()).await?;
        let next = match watcher.github().apply(&config, &draft) {
            Ok(n) => n,
            Err(e) => return Ok(Err(e)),
        };
        set_github_config(txn.conn(), &next, watcher.now()).await?;
        // 換了 token：之前的異常狀態不再適用。
        if draft.token.is_some() {
            sqlx::query("UPDATE sources SET health = '{}' WHERE id = ?")
                .bind(github::SOURCE_ID)
                .execute(txn.conn())
                .await?;
            automation::push(&mut txn, watcher.now()).await?;
        }
        let github = watcher.github().view(&next);
        txn.push(|seq| ServerEvent::Github { seq, github }).await?;
        txn.commit().await?;
        Ok(Ok(()))
    }
    .await;
    match result {
        Ok(r) => {
            if r.is_ok() {
                watcher.poll_now();
            }
            r
        }
        Err(e) => {
            tracing::error!(error = %e, "無法儲存 GitHub 設定");
            Err("伺服器內部錯誤".into())
        }
    }
}

/// 測試連線；成功時記下帳號名稱（token 是已存的那個時）。
pub async fn test_github(
    watcher: &Watcher,
    hub: &Hub,
    request: u64,
    draft: GithubDraft,
) -> ServerEvent {
    let config = match watcher.inner.store.acquire().await {
        Ok(mut conn) => github_config(&mut conn).await.unwrap_or_default(),
        Err(_) => GithubConfig::default(),
    };
    let result = watcher.github().test(&config, &draft).await;
    if let Ok(login) = &result
        && draft.token.as_deref().is_none_or(|t| t.trim().is_empty())
        && draft.api_base.is_none()
        && config.login.as_deref() != Some(login.as_str())
    {
        let saved: Result<(), Error> = async {
            let mut txn = hub.begin().await?;
            let mut config = github_config(txn.conn()).await?;
            config.login = Some(login.clone());
            set_github_config(txn.conn(), &config, watcher.now()).await?;
            let github = watcher.github().view(&config);
            txn.push(|seq| ServerEvent::Github { seq, github }).await?;
            txn.commit().await
        }
        .await;
        if let Err(e) = saved {
            tracing::warn!(error = %e, "無法記下 GitHub 帳號");
        }
    }
    ServerEvent::GithubTest { request, result }
}
