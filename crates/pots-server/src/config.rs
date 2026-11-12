//! 服務設定（`proposal/deployment.md#切換點`）。
//!
//! 「桌面版」與「多設備版」只是預先填好的 [`Config`]。M0 只實作桌面版的組合：
//! SQLite、loopback、本機 token；其他選項先保留型別，啟動時回 [`Error::Unsupported`]。

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use pots_proto::{InstanceKind, ProviderId, Timestamp};

use crate::Error;
use crate::memory::OmConfig;

/// 事件日誌預設保留的筆數；更早的事件刪掉後，落後太多的客戶端改拿快照。
pub const DEFAULT_EVENT_RETENTION: u64 = 50_000;

#[derive(Clone, Debug)]
pub struct Config {
    /// 資料目錄：資料庫、鎖檔、`server.json` 與 SIWC 的 host ID 都放這裡。
    pub data_dir: PathBuf,
    pub kind: InstanceKind,
    pub store: StoreConfig,
    pub listen: ListenConfig,
    pub auth: AuthConfig,
    /// 事件日誌保留的筆數。
    pub event_retention: u64,
    /// 實例名稱；`None` 時用主機名稱，取不到就是「本機」。只在第一次初始化時寫入。
    pub instance_name: Option<String>,
    /// 資料庫還沒記錄過使用中的 provider 時（第一次啟動）要選的 provider。
    /// 桌面版是 `None`：使用者到設定自己選；但 ChatGPT 在啟動時已登入的話會選 ChatGPT，
    /// 讓升級前就登入的使用者照常可用。之後一律以資料庫為準。
    pub default_provider: Option<ProviderId>,
    /// Observational Memory 的閾值（`proposal/memory.md`）。
    pub memory: OmConfig,
    /// 外部事件、等待與排程（`proposal/events-and-automation.md`）。
    pub watcher: WatcherConfig,
    /// Coding worker（`proposal/agent-runtime.md#worker-adapter內建-worker-與外部-coding-cli`）。
    pub worker: WorkerConfig,
}

/// worker 的部署設定；使用者可改的（後端、執行環境、image）存在資料庫，由設定視窗修改。
#[derive(Clone, Debug)]
pub struct WorkerConfig {
    /// 資料庫還沒有 worker 設定時用的值。
    pub defaults: pots_proto::WorkerSettingsDraft,
    /// `claude` 執行檔（本機執行時的路徑；容器裡一律是 `claude`）。
    pub claude_bin: String,
    /// 測試用：git 的 base URL（例如 `file:///tmp/remotes`）；`None` 時依 GitHub API 推算。
    pub git_base: Option<String>,
    /// 一次 worker run 最多跑多久（從環境的準備腳本跑完開始算）。
    pub run_timeout: Duration,
    /// 環境分析（環境 worker）一次 run 最多跑多久：建置 image 很慢。
    pub env_run_timeout: Duration,
    /// 中斷後等 CLI 收尾的時間。
    pub grace: Duration,
    /// `false` 時不處理 worker 佇列（任務排著不跑），給只測規劃與等待的測試用。
    pub run_queue: bool,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            defaults: pots_proto::WorkerSettingsDraft {
                backend: pots_proto::WorkerBackendKind::Builtin,
                exec: pots_proto::ExecKind::Container,
                runtime: "docker".into(),
                image: "pots-worker:latest".into(),
                claude_model: String::new(),
                max_parallel: 2,
            },
            claude_bin: "claude".into(),
            git_base: None,
            run_timeout: Duration::from_secs(60 * 60),
            env_run_timeout: Duration::from_secs(120 * 60),
            grace: Duration::from_secs(15),
            run_queue: true,
        }
    }
}

/// 排程、逾時與 listener 上限用的時鐘。正式版是系統時間；測試用 [`ManualClock`] 撥時間。
#[derive(Clone)]
pub struct Clock(Arc<dyn Fn() -> Timestamp + Send + Sync>);

impl Clock {
    pub fn system() -> Self {
        Self(Arc::new(Timestamp::now))
    }

    pub fn now(&self) -> Timestamp {
        (self.0)()
    }
}

impl std::fmt::Debug for Clock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Clock({})", self.now().0)
    }
}

/// 手動撥的時鐘，給測試用。
#[derive(Clone, Debug)]
pub struct ManualClock(Arc<AtomicI64>);

impl ManualClock {
    pub fn new(start: Timestamp) -> Self {
        Self(Arc::new(AtomicI64::new(start.0)))
    }

    pub fn clock(&self) -> Clock {
        let secs = self.0.clone();
        Clock(Arc::new(move || Timestamp(secs.load(Ordering::SeqCst))))
    }

    pub fn advance(&self, secs: i64) {
        self.0.fetch_add(secs, Ordering::SeqCst);
    }

    pub fn now(&self) -> Timestamp {
        Timestamp(self.0.load(Ordering::SeqCst))
    }
}

/// GitHub API 的預設 base URL。
pub const GITHUB_API: &str = "https://api.github.com";

#[derive(Clone, Debug)]
pub struct WatcherConfig {
    /// 多久輪詢一次外部來源。
    pub poll_interval: Duration,
    /// sweeper（逾時、排程、自動恢復）多久掃一次。
    pub sweep_interval: Duration,
    /// 使用者沒有指定時的 GitHub API base URL。
    pub github_api: String,
    pub clock: Clock,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(60),
            sweep_interval: Duration::from_secs(60),
            github_api: GITHUB_API.into(),
            clock: Clock::system(),
        }
    }
}

#[derive(Clone, Debug)]
pub enum StoreConfig {
    Sqlite { path: PathBuf },
    // Postgres { url } 在 M8 加入。
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListenConfig {
    /// 只綁 127.0.0.1；`port` 為 0 時由系統挑隨機 port。
    Loopback { port: u16 },
    /// 綁定指定位址（私有網路或反向代理後方）。
    Bind { addr: SocketAddr },
}

impl ListenConfig {
    pub fn is_loopback(self) -> bool {
        match self {
            Self::Loopback { .. } => true,
            Self::Bind { addr } => addr.ip().is_loopback(),
        }
    }
}

/// Pots 本身的存取控制；和模型 provider 的授權無關。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthConfig {
    /// 啟動時產生的本機 token，寫在只有自己能讀的 `server.json`。
    LocalToken,
    Passkey,
    /// 只接受 `trusted_proxy` 送來的身分標頭。
    ForwardAuth {
        trusted_proxy: IpAddr,
    },
}

impl Config {
    /// 桌面版的預設組合：`data_dir/pots.db`（WAL）、loopback 隨機 port、本機 token。
    pub fn desktop(data_dir: impl Into<PathBuf>) -> Self {
        let data_dir = data_dir.into();
        Self {
            store: StoreConfig::Sqlite {
                path: data_dir.join("pots.db"),
            },
            data_dir,
            kind: InstanceKind::Desktop,
            listen: ListenConfig::Loopback { port: 0 },
            auth: AuthConfig::LocalToken,
            event_retention: DEFAULT_EVENT_RETENTION,
            instance_name: None,
            default_provider: None,
            memory: OmConfig::default(),
            watcher: WatcherConfig::default(),
            worker: WorkerConfig::default(),
        }
    }

    pub fn with_default_provider(mut self, id: ProviderId) -> Self {
        self.default_provider = Some(id);
        self
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.listen = ListenConfig::Loopback { port };
        self
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// 拒絕不合理的組合（`proposal/deployment.md#設定驗證`）。
    pub fn validate(&self) -> Result<(), Error> {
        if !self.listen.is_loopback() && self.auth == AuthConfig::LocalToken {
            return Err(Error::Config(
                "不是只綁 loopback 時不能用本機 token；非本機連線必須登入".into(),
            ));
        }
        if self.event_retention == 0 {
            return Err(Error::Config("事件日誌至少要保留 1 筆".into()));
        }
        if self.watcher.poll_interval.is_zero() || self.watcher.sweep_interval.is_zero() {
            return Err(Error::Config("輪詢與 sweeper 的間隔不能是 0".into()));
        }
        Ok(())
    }

    /// M0 實際能跑的組合：loopback 加本機 token。
    pub(crate) fn check_supported(&self) -> Result<u16, Error> {
        let ListenConfig::Loopback { port } = self.listen else {
            return Err(Error::Unsupported("M0 只支援 loopback".into()));
        };
        if self.auth != AuthConfig::LocalToken {
            return Err(Error::Unsupported("M0 只支援本機 token".into()));
        }
        Ok(port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_defaults_are_valid() {
        let cfg = Config::desktop("/tmp/pots-test");
        cfg.validate().unwrap();
        assert_eq!(cfg.check_supported().unwrap(), 0);
        let StoreConfig::Sqlite { path } = &cfg.store;
        assert_eq!(path, Path::new("/tmp/pots-test/pots.db"));
    }

    #[test]
    fn public_listen_needs_login() {
        let mut cfg = Config::desktop("/tmp/pots-test");
        cfg.listen = ListenConfig::Bind {
            addr: "0.0.0.0:8080".parse().unwrap(),
        };
        assert!(matches!(cfg.validate(), Err(Error::Config(_))));
        cfg.auth = AuthConfig::Passkey;
        cfg.validate().unwrap();
        assert!(matches!(cfg.check_supported(), Err(Error::Unsupported(_))));

        // 綁 loopback 位址的 Bind 仍然算本機。
        cfg.listen = ListenConfig::Bind {
            addr: "127.0.0.1:8080".parse().unwrap(),
        };
        cfg.auth = AuthConfig::LocalToken;
        cfg.validate().unwrap();
    }
}
