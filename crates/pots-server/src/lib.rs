//! Pots 服務：gateway（HTTP 與 WebSocket）、儲存、事件日誌、agent run 與 Observational Memory。
//!
//! 桌面版由 app 用 `--serve` 以子行程啟動自己，再呼叫 [`run_until_signal`]（或
//! [`start`] 取得 [`ServerHandle`]）；多設備版是獨立的 `pots-server` binary。兩者走同一條
//! 程式路徑，差別只在 [`Config`]（`proposal/deployment.md`）。
//!
//! 模型與授權由組裝端注入（[`Deps`]）：`ModelProvider` 負責內建 ChatGPT 方案的推論，
//! [`ModelAuthControl`] 負責登入狀態，[`ApiProviderFactory`] 建立使用者在設定新增的
//! API provider。伺服器不知道背後是不是 SIWC。使用中的 provider 由伺服器保存與路由
//! （見 `providers` 模組）。

mod approvals;
mod artifacts;
mod attachments;
mod automation;
mod budget;
mod commands;
pub mod config;
pub mod cost;
mod datadir;
mod gateway;
mod hub;
mod images;
mod memory;
pub mod model_auth;
mod notifications;
mod planner;
mod preset_provider;
mod profiles;
mod project_env;
mod providers;
mod runs;
pub mod service;
mod store;
mod tasks;
mod team;
mod tools;
mod user_md;
mod watcher;
mod workdir;
mod worker;
mod ws;

use std::collections::BTreeSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use pots_llm::ModelProvider;
use pots_proto::{AgentState, InstanceId, MsgStatus, ProjectId, ServerEvent, StreamItemKind};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub use config::{
    AuthConfig, Clock, Config, ListenConfig, ManualClock, StoreConfig, WatcherConfig, WorkerConfig,
};
pub use datadir::{BuildInfo, ConnectionInfo, LocalToken, read_connection_info};
pub use images::{ImageSlot, StoreImages};
pub use memory::OmConfig;
pub use model_auth::{FakeModelAuth, ModelAuthControl};
pub use providers::{ApiProviderFactory, NoApiProviders};

use crate::hub::{Hub, InstanceInfo};
use crate::memory::Memory;
use crate::planner::Planner;
use crate::profiles::Profiles;
use crate::providers::{ProviderRegistry, ProviderRouter};
use crate::runs::Runs;
use crate::store::{ItemStatus, Store};
use crate::watcher::Watcher;
use crate::worker::Worker;

/// 關閉時等 agent run 收尾的時間。
const RUN_SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("設定不合理：{0}")]
    Config(String),
    #[error("尚未支援：{0}")]
    Unsupported(String),
    /// 同一個資料目錄已經有服務在跑。帶它的連線資訊；對方剛啟動、還沒寫
    /// `server.json` 時是 `None`，稍後用 [`read_connection_info`] 再讀一次。
    #[error("這個資料目錄已經有 Pots 服務在執行")]
    AlreadyRunning(Option<Box<ConnectionInfo>>),
    #[error("檔案系統：{0}")]
    Io(#[from] std::io::Error),
    #[error("資料庫：{0}")]
    Db(#[from] sqlx::Error),
    #[error("資料庫 migration：{0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("資料格式：{0}")]
    Json(#[from] serde_json::Error),
    #[error("資料損壞：{0}")]
    Corrupt(String),
}

/// 由組裝端注入的模型與授權。
#[derive(Clone)]
pub struct Deps {
    /// 內建 ChatGPT 方案的推論。
    pub provider: Arc<dyn ModelProvider>,
    /// ChatGPT 方案沒有在設定指定模型時用的模型 ID（正式版是
    /// [`service::AUTO_MODEL`]，由 provider 自己決定）。
    pub model: String,
    pub model_auth: Arc<dyn ModelAuthControl>,
    /// 建立使用者新增的 API provider（OpenAI／Anthropic 相容）。
    pub api_providers: Arc<dyn ApiProviderFactory>,
    /// 圖片附件的內容來源（見 [`images::ImageSlot`]）。組裝端建立 provider 時還沒有資料庫，
    /// 只能先放一個空的槽；[`start`] 開好資料庫之後把 [`images::StoreImages`] 填進去，
    /// 已經建好的 provider 共用同一個槽，所以正式推論路徑拿到的是填好的來源。
    pub images: images::ImageSlot,
}

impl Deps {
    /// 只有 ChatGPT 方案、不支援 API provider（[`NoApiProviders`]）的組合。
    pub fn new(
        provider: Arc<dyn ModelProvider>,
        model: impl Into<String>,
        model_auth: Arc<dyn ModelAuthControl>,
    ) -> Self {
        Self {
            provider,
            model: model.into(),
            model_auth,
            api_providers: Arc::new(NoApiProviders),
            images: images::ImageSlot::new(),
        }
    }

    pub fn with_api_providers(mut self, factory: Arc<dyn ApiProviderFactory>) -> Self {
        self.api_providers = factory;
        self
    }

    /// 直接指定圖片來源（測試用；正式路徑由 [`start`] 填）。
    pub fn with_images(self, loader: Arc<dyn pots_llm::image::ImageLoader>) -> Self {
        self.images.fill(loader);
        self
    }
}

/// 各個 handler 共用的狀態。
pub(crate) struct App {
    pub hub: Arc<Hub>,
    pub store: Store,
    pub runs: Runs,
    /// 總管的全域對話（`kind = 'overseer'` 的內部 project）。
    pub overseer_runs: Runs,
    pub planner: Planner,
    pub worker: Worker,
    pub memory: Memory,
    pub watcher: Watcher,
    pub providers: Arc<ProviderRegistry>,
    pub model_auth: Arc<dyn ModelAuthControl>,
    pub shutdown: CancellationToken,
}

/// 執行中的服務。
#[derive(Debug)]
pub struct ServerHandle {
    pub addr: SocketAddr,
    /// 客戶端要帶的 `Authorization: Bearer` token。`Debug` 不會印出。
    pub token: LocalToken,
    pub instance_id: InstanceId,
    shutdown: CancellationToken,
    task: JoinHandle<Result<(), Error>>,
}

impl ServerHandle {
    /// 開始正常關閉：停止接受連線、取消 run、關閉 WebSocket、刪除 `server.json`、釋放鎖。
    /// 用 [`wait`](Self::wait) 等它完成。
    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }

    /// 觸發後會關閉服務的 token，可以交給訊號處理或系統匣選單。
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    pub fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo {
            pid: std::process::id(),
            addr: self.addr,
            token: self.token.clone(),
            instance_id: self.instance_id,
            build: Some(BuildInfo::current()),
        }
    }

    /// 等服務結束（關閉流程全部完成後才回傳）。
    pub async fn wait(self) -> Result<(), Error> {
        match self.task.await {
            Ok(r) => r,
            Err(e) => Err(Error::Io(std::io::Error::other(e))),
        }
    }
}

/// 啟動服務並等到收到 ctrl-c（Unix 另含 SIGTERM）後正常關閉。給 bin 與桌面 app 的 `--serve` 用。
pub async fn run_until_signal(config: Config, deps: Deps) -> Result<(), Error> {
    let handle = start(config, deps).await?;
    tracing::info!(addr = %handle.addr, instance = %handle.instance_id, "Pots 服務已啟動");
    let shutdown = handle.shutdown_token();
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("收到結束訊號，正在關閉");
        shutdown.cancel();
    });
    handle.wait().await
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// 啟動服務：鎖住資料目錄、開資料庫、復原上次中斷的 run、綁定 port、寫 `server.json`。
pub async fn start(config: Config, deps: Deps) -> Result<ServerHandle, Error> {
    config.validate()?;
    let port = config.check_supported()?;

    let data_dir = datadir::DataDir::acquire(&config.data_dir).await?;
    let profiles = Profiles::load(&config.data_dir)?;
    // SIWC 的 host ID（`data_dir/host-id`）由組裝端用 `pots_llm` 載入，這裡不碰；
    // 組裝端可以在呼叫 `start` 之前用 `config.data_dir` 取得它。

    let store = match &config.store {
        StoreConfig::Sqlite { path } => Store::open_sqlite(path).await?,
    };
    let instance_name = config
        .instance_name
        .clone()
        .unwrap_or_else(default_instance_name);
    let instance = store.ensure_instance(&instance_name).await?;
    store.ensure_overseer().await?;
    let pruned = store.prune_events(config.event_retention).await?;
    if pruned > 0 {
        tracing::debug!(pruned, "清掉舊的事件日誌");
    }
    let backfilled = store.backfill_om_tokens().await?;
    if backfilled > 0 {
        tracing::info!(backfilled, "補算舊的原始歷史的 token 數");
    }

    // 圖片附件的內容來源：讀 `<data_dir>/attachments/<sha256>`，base64 只在送出請求那一刻
    // 產生（見 `images`）。組裝端建 provider 時只有一個空槽，這裡把它填起來——ChatGPT 的
    // provider（`Deps::provider`）、`HttpApiProviders` 建出來的 API provider，以及
    // `ProviderRegistry` 之後新建／重建的 provider 都共用同一個槽。
    deps.images.fill(Arc::new(images::StoreImages::new(
        store.clone(),
        config.data_dir.clone(),
    )));

    let auth_rx = deps.model_auth.status();
    let initial_auth = auth_rx.borrow().clone();
    // 還沒選過 provider：用設定的預設值；沒有預設但 ChatGPT 已登入（升級前的使用者）就選 ChatGPT。
    let fallback = config.default_provider.or_else(|| {
        matches!(initial_auth, pots_proto::ModelAuthView::SignedIn { .. })
            .then_some(pots_proto::ProviderId::CHATGPT)
    });
    let loaded = ProviderRegistry::load(
        &store,
        &config.data_dir,
        deps.api_providers.clone(),
        fallback,
    )
    .await?;
    let registry = Arc::new(loaded.registry);
    let hub = Arc::new(
        Hub::new(
            store.clone(),
            InstanceInfo {
                id: instance.id,
                name: instance.name.clone(),
                kind: config.kind,
            },
            initial_auth.clone(),
            registry.view(),
        )
        .await?,
    );
    recover(&hub).await?;
    // 授權狀態可能在服務沒跑的時候變了；每次啟動都記一筆，讓重連的客戶端拿到最新狀態。
    hub.set_model_auth(initial_auth, true).await?;
    // provider 設定也記一筆（第一次啟動時順便寫入初始的選擇）。
    {
        let mut txn = hub.begin().await?;
        if let Some(id) = loaded.initial_active {
            store::Store::set_setting(txn.conn(), providers::SETTING_ACTIVE, &id.to_string())
                .await?;
        }
        let providers = registry.view();
        txn.push(|seq| ServerEvent::Providers { seq, providers })
            .await?;
        txn.commit().await?;
    }

    let router = Arc::new(ProviderRouter::new(
        registry.clone(),
        deps.provider,
        deps.model,
    ));
    let memory = Memory::new(
        hub.clone(),
        store.clone(),
        router.clone(),
        config.memory.clone(),
    );
    let clock = config.watcher.clock.clone();
    let worker = Worker::new(
        hub.clone(),
        store.clone(),
        router.clone(),
        profiles.clone(),
        config.worker.clone(),
        config.data_dir.clone(),
        instance.id.to_string(),
        clock.clone(),
    );
    let planner = Planner::new(
        hub.clone(),
        store.clone(),
        router.clone(),
        profiles.planner,
        clock,
        worker.clone(),
    );
    let runs = Runs::new(
        hub.clone(),
        store.clone(),
        memory.clone(),
        router.clone(),
        deps.model_auth.status(),
        profiles.chat,
        planner.clone(),
    );
    let overseer_runs = Runs::new(
        hub.clone(),
        store.clone(),
        memory.clone(),
        router,
        deps.model_auth.status(),
        profiles.global.clone(),
        planner.clone(),
    );
    let watcher = Watcher::new(
        hub.clone(),
        store.clone(),
        planner.clone(),
        runs.clone(),
        &config.data_dir,
        config.watcher.clone(),
    );
    watcher.init().await?;
    watcher.bind_overseer(overseer_runs.clone());
    worker.bind(planner.clone(), watcher.clone());
    {
        let mut txn = hub.begin().await?;
        let settings = worker::load_settings(txn.conn(), &config.worker.defaults).await?;
        let view = worker::settings_view(&settings, &config.data_dir);
        txn.push(|seq| ServerEvent::Worker { seq, worker: view })
            .await?;
        txn.commit().await?;
    }
    let shutdown = CancellationToken::new();
    let app = Arc::new(App {
        hub: hub.clone(),
        store: store.clone(),
        runs: runs.clone(),
        overseer_runs: overseer_runs.clone(),
        planner: planner.clone(),
        worker: worker.clone(),
        memory: memory.clone(),
        watcher: watcher.clone(),
        providers: registry,
        model_auth: deps.model_auth,
        shutdown: shutdown.clone(),
    });

    // 每個 project 一份 Project Notes（你可以直接編輯；agent 只能提案）。
    {
        let mut txn = hub.begin().await?;
        for p in store.list_projects().await? {
            user_md::ensure_project_notes(&mut txn, p.id).await?;
        }
        txn.commit().await?;
    }

    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
    let addr = listener.local_addr()?;
    let token = LocalToken::generate();
    let info = ConnectionInfo {
        pid: std::process::id(),
        addr,
        token: token.clone(),
        instance_id: instance.id,
        build: Some(BuildInfo::current()),
    };
    data_dir.write_connection_info(&info)?;
    worker.set_port(addr.port());
    // 啟動不會再失敗了：上次關閉時可能還有沒整理完的記憶。
    match store.list_projects().await {
        Ok(projects) => projects.iter().for_each(|p| memory.wake(p.id)),
        Err(e) => tracing::error!(error = %e, "無法列出 project，稍後再整理記憶"),
    }
    // 上次沒跑完（或排隊中）的 Planner run 接著跑。
    planner.start();
    planner.kick();
    // 上次沒跑完（或排隊中）的 worker run 接著跑。
    worker.start();
    worker.kick();
    // 輪詢與 sweeper：啟動時馬上各跑一次，補上停機期間漏掉的事件與到期的等待、排程。
    watcher.start();
    // 上次核准了、還沒執行完的動作（merge、刪分支）接著執行。
    let exec = approvals::Exec {
        hub: hub.clone(),
        store: store.clone(),
        watcher: watcher.clone(),
        worker: worker.clone(),
    };
    if let Err(e) = exec.resume().await {
        tracing::error!(error = %e, "無法接著執行已核准的動作");
    }

    let forwarder = tokio::spawn(forward_model_auth(hub.clone(), auth_rx, shutdown.clone()));
    let router = gateway::router(
        app,
        Arc::new(gateway::Guard {
            port: addr.port(),
            token: token.as_str().to_string(),
        }),
    )
    // Claude Code worker 的平台工具：不經 gateway 的本機 token 與 Host 檢查（另有自己的 Host 白名單），
    // 改驗每個 run 的 capability。
    .merge(worker::mcp::router(worker.grants(), shutdown.clone()));

    let task = {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let serve = axum::serve(listener, router)
                .with_graceful_shutdown(shutdown.clone().cancelled_owned());
            let (served, ()) = tokio::join!(serve, async {
                shutdown.cancelled().await;
                // watcher 先停：它最後送出的觸發還要排得進 run（之後由 `recover` 補寫）。
                watcher.shutdown(RUN_SHUTDOWN_GRACE).await;
                tokio::join!(
                    runs.shutdown(RUN_SHUTDOWN_GRACE),
                    overseer_runs.shutdown(RUN_SHUTDOWN_GRACE),
                    planner.shutdown(RUN_SHUTDOWN_GRACE),
                    worker.shutdown(RUN_SHUTDOWN_GRACE),
                    memory.shutdown(RUN_SHUTDOWN_GRACE)
                );
            });
            let _ = forwarder.await;
            data_dir.remove_connection_info();
            store.close().await;
            drop(data_dir);
            tracing::info!("Pots 服務已關閉");
            served.map_err(Error::Io)
        })
    };

    Ok(ServerHandle {
        addr,
        token,
        instance_id: instance.id,
        shutdown,
        task,
    })
}

fn default_instance_name() -> String {
    let host = gethostname::gethostname()
        .to_string_lossy()
        .trim()
        .to_string();
    let host = host.strip_suffix(".local").unwrap_or(&host).to_string();
    if host.is_empty() {
        "本機".into()
    } else {
        host
    }
}

/// 上次異常結束留下的狀態：還在跑的 run 標成失敗、串流中的訊息標成 Failed，
/// 還沒進 `om_raw` 的人類訊息補寫進去（不自動觸發 run）。
async fn recover(hub: &Hub) -> Result<(), Error> {
    let mut txn = hub.begin().await?;
    let stale = Store::fail_stale_runs(txn.conn(), "服務中斷").await?;
    let (requeued, gave_up) = planner::requeue_stale(txn.conn()).await?;
    planner::note_gave_up(&mut txn, &gave_up).await?;
    let (worker_requeued, worker_gave_up) = worker::requeue_stale(txn.conn()).await?;
    worker::note_gave_up(&mut txn, &worker_gave_up).await?;
    let mut idle = BTreeSet::<ProjectId>::new();
    for (project, mut item) in Store::items_with_status(txn.conn(), ItemStatus::Streaming).await? {
        if let StreamItemKind::Agent(msg) = &mut item.kind {
            msg.status = MsgStatus::Failed;
            if !msg.text.is_empty() {
                msg.text.push_str("\n\n");
            }
            msg.text.push_str("（服務中斷，回覆未完成）");
        }
        Store::update_item(txn.conn(), &item, ItemStatus::Failed).await?;
        txn.push(|seq| ServerEvent::ItemDone {
            seq,
            scope: store::stream_scope(project),
            item,
        })
        .await?;
        idle.insert(project);
    }
    for project in idle {
        txn.push(|seq| ServerEvent::AgentState {
            seq,
            project,
            state: AgentState::Idle,
        })
        .await?;
    }
    for (project, item) in Store::items_with_status(txn.conn(), ItemStatus::Pending).await? {
        match &item.kind {
            StreamItemKind::Human(msg) => {
                runs::consume(&mut txn, project, item.id, &msg.text, None).await?;
            }
            // 排程與 listener 的系統觸發：和人類訊息一樣寫進歷史（不自動觸發 run）。
            _ => {
                let text: Option<String> =
                    sqlx::query_scalar("SELECT trigger_text FROM stream_items WHERE id = ?")
                        .bind(item.id.to_string())
                        .fetch_one(txn.conn())
                        .await?;
                match text {
                    Some(text) => runs::consume(&mut txn, project, item.id, &text, None).await?,
                    None => Store::mark_consumed(txn.conn(), item.id, None).await?,
                }
            }
        }
    }
    txn.commit().await?;
    if stale > 0 {
        tracing::warn!(stale, "上次結束時有 run 還在執行，已標成失敗");
    }
    if requeued > 0 {
        tracing::warn!(requeued, "上次結束時有 Planner run 還在執行，已放回佇列");
    }
    if worker_requeued > 0 {
        tracing::warn!(
            worker_requeued,
            "上次結束時有 worker run 還在執行，已放回佇列"
        );
    }
    Ok(())
}

/// 把授權狀態的變化記成 `ModelAuth` 事件。
async fn forward_model_auth(
    hub: Arc<Hub>,
    mut rx: tokio::sync::watch::Receiver<pots_proto::ModelAuthView>,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            changed = rx.changed() => {
                if changed.is_err() {
                    return;
                }
                let view = rx.borrow_and_update().clone();
                if let Err(e) = hub.set_model_auth(view, false).await {
                    tracing::error!(error = %e, "無法記錄模型授權狀態");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use pots_proto::{AgentRole, Author, ItemId, Msg, MsgStatus, StreamItemView, Timestamp};

    use super::*;
    use crate::store::ProjectRow;

    fn msg(author: Author, text: &str, status: MsgStatus) -> Msg {
        Msg {
            author,
            text: text.into(),
            status,
            tools: None,
            narration: Vec::new(),
            cards: Vec::new(),
            steer: false,
            attachments: Vec::new(),
        }
    }

    /// 模擬異常結束：資料庫裡留著 running 的 run、串流中的訊息與還沒進歷史的人類訊息。
    #[tokio::test]
    async fn recovers_after_crash() {
        let tmp = tempfile::tempdir().unwrap();
        let config = Config::desktop(tmp.path());
        let project = ProjectId::new();
        let agent_item = ItemId::new();
        {
            let StoreConfig::Sqlite { path } = &config.store;
            std::fs::create_dir_all(tmp.path()).unwrap();
            let store = Store::open_sqlite(path).await.unwrap();
            let mut tx = store.begin().await.unwrap();
            Store::insert_project(
                &mut tx,
                &ProjectRow {
                    id: project,
                    name: "當機".into(),
                    instructions: String::new(),
                    agent_name: "Pots".into(),
                    workdir: None,
                    created_at: Timestamp::now(),
                },
            )
            .await
            .unwrap();
            let run = uuid::Uuid::now_v7();
            Store::insert_run(&mut tx, run, project, "chat", &serde_json::json!({}))
                .await
                .unwrap();
            Store::append_om_raw(
                &mut tx,
                project,
                Some(run),
                &pots_llm::Item::User { text: "A".into() },
            )
            .await
            .unwrap();
            let agent = StreamItemView {
                id: agent_item,
                at: Timestamp::now(),
                kind: StreamItemKind::Agent(msg(
                    Author::Agent {
                        label: "Pots".into(),
                        role: AgentRole::Project,
                    },
                    "寫到",
                    MsgStatus::Streaming,
                )),
            };
            Store::insert_item(&mut tx, project, &agent, ItemStatus::Streaming, Some(run))
                .await
                .unwrap();
            let human = StreamItemView {
                id: ItemId::new(),
                at: Timestamp::now(),
                kind: StreamItemKind::Human(msg(Author::Human, "B", MsgStatus::Done)),
            };
            Store::insert_item(&mut tx, project, &human, ItemStatus::Pending, None)
                .await
                .unwrap();
            tx.commit().await.unwrap();
            store.close().await;
        }

        let provider = pots_llm::testing::ScriptedProvider::new([]);
        let handle = start(
            config.clone(),
            Deps::new(provider.clone(), "m", Arc::new(FakeModelAuth::signed_out())),
        )
        .await
        .unwrap();
        handle.shutdown();
        handle.wait().await.unwrap();

        let StoreConfig::Sqlite { path } = &config.store;
        let store = Store::open_sqlite(path).await.unwrap();
        let items = store.latest_items(project, 10).await.unwrap();
        let StreamItemKind::Agent(m) = &items[0].kind else {
            panic!()
        };
        assert_eq!(m.status, MsgStatus::Failed);
        assert_eq!(m.text, "寫到\n\n（服務中斷，回覆未完成）");
        let history = store.om_raw_history(project).await.unwrap();
        assert_eq!(
            history,
            [
                pots_llm::Item::User { text: "A".into() },
                pots_llm::Item::User { text: "B".into() },
            ]
        );
        let mut conn = store.begin().await.unwrap();
        assert!(
            Store::items_with_status(&mut conn, ItemStatus::Pending)
                .await
                .unwrap()
                .is_empty()
        );
        // 復原時記了 ItemDone 與 AgentState Idle。
        let events = store.events_after(0).await.unwrap();
        assert!(events.iter().any(|(_, j)| j.contains("ItemDone")));
        assert!(events.iter().any(|(_, j)| j.contains("\"Idle\"")));
        // 補進歷史的 B 記了 ItemUpdated。
        assert!(events.iter().any(|(_, j)| j.contains("ItemUpdated")));
        assert!(provider.requests().is_empty());
    }
}
