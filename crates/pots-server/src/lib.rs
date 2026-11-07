//! Pots 服務：gateway（HTTP 與 WebSocket）、儲存、事件日誌與 agent run。
//!
//! 桌面版由 app 用 `--serve` 以子行程啟動自己，再呼叫 [`run_until_signal`]（或
//! [`start`] 取得 [`ServerHandle`]）；多設備版是獨立的 `pots-server` binary。兩者走同一條
//! 程式路徑，差別只在 [`Config`]（`proposal/deployment.md`）。
//!
//! 模型與授權由組裝端注入（[`Deps`]）：`ModelProvider` 負責推論，[`ModelAuthControl`]
//! 負責登入狀態。伺服器不知道背後是不是 SIWC。

mod commands;
pub mod config;
mod datadir;
mod gateway;
mod hub;
pub mod model_auth;
mod runs;
mod store;
mod ws;

use std::collections::BTreeSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use pots_llm::ModelProvider;
use pots_proto::{
    AgentState, InstanceId, MsgStatus, ProjectId, ServerEvent, StreamItemKind, StreamScope,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub use config::{AuthConfig, Config, ListenConfig, StoreConfig};
pub use datadir::{ConnectionInfo, LocalToken, read_connection_info};
pub use model_auth::{FakeModelAuth, ModelAuthControl};
pub use runs::{AgentProfile, CHAT_PROFILE};

use crate::hub::{Hub, InstanceInfo};
use crate::runs::Runs;
use crate::store::{ItemStatus, Store};

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
    AlreadyRunning(Option<ConnectionInfo>),
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
    pub provider: Arc<dyn ModelProvider>,
    /// chat profile 用的模型 ID。
    pub model: String,
    pub model_auth: Arc<dyn ModelAuthControl>,
}

/// 各個 handler 共用的狀態。
pub(crate) struct App {
    pub hub: Arc<Hub>,
    pub store: Store,
    pub runs: Runs,
    pub model_auth: Arc<dyn ModelAuthControl>,
    pub shutdown: CancellationToken,
    pub agent_name: String,
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
    let pruned = store.prune_events(config.event_retention).await?;
    if pruned > 0 {
        tracing::debug!(pruned, "清掉舊的事件日誌");
    }

    let auth_rx = deps.model_auth.status();
    let initial_auth = auth_rx.borrow().clone();
    let hub = Arc::new(
        Hub::new(
            store.clone(),
            InstanceInfo {
                id: instance.id,
                name: instance.name.clone(),
                kind: config.kind,
            },
            initial_auth.clone(),
        )
        .await?,
    );
    recover(&hub).await?;
    // 授權狀態可能在服務沒跑的時候變了；每次啟動都記一筆，讓重連的客戶端拿到最新狀態。
    hub.set_model_auth(initial_auth, true).await?;

    let runs = Runs::new(hub.clone(), store.clone(), deps.provider, deps.model);
    let shutdown = CancellationToken::new();
    let app = Arc::new(App {
        hub: hub.clone(),
        store: store.clone(),
        runs: runs.clone(),
        model_auth: deps.model_auth,
        shutdown: shutdown.clone(),
        agent_name: config.agent_name.clone(),
    });

    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
    let addr = listener.local_addr()?;
    let token = LocalToken::generate();
    let info = ConnectionInfo {
        pid: std::process::id(),
        addr,
        token: token.clone(),
        instance_id: instance.id,
    };
    data_dir.write_connection_info(&info)?;

    let forwarder = tokio::spawn(forward_model_auth(hub.clone(), auth_rx, shutdown.clone()));
    let router = gateway::router(
        app,
        Arc::new(gateway::Guard {
            port: addr.port(),
            token: token.as_str().to_string(),
        }),
    );

    let task = {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let serve = axum::serve(listener, router)
                .with_graceful_shutdown(shutdown.clone().cancelled_owned());
            let (served, ()) = tokio::join!(serve, async {
                shutdown.cancelled().await;
                runs.shutdown(RUN_SHUTDOWN_GRACE).await;
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
            scope: StreamScope::Project(project),
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
        if let StreamItemKind::Human(msg) = &item.kind {
            Store::append_om_raw(
                txn.conn(),
                project,
                None,
                &pots_llm::Item::User {
                    text: msg.text.clone(),
                },
            )
            .await?;
        }
        Store::mark_consumed(txn.conn(), item.id, None).await?;
    }
    txn.commit().await?;
    if stale > 0 {
        tracing::warn!(stale, "上次結束時有 run 還在執行，已標成失敗");
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
            cards: Vec::new(),
            steer: false,
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
            Deps {
                provider: provider.clone(),
                model: "m".into(),
                model_auth: Arc::new(FakeModelAuth::signed_out()),
            },
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
        assert!(provider.requests().is_empty());
    }
}
