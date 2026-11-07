//! 服務設定（`proposal/deployment.md#切換點`）。
//!
//! 「桌面版」與「多設備版」只是預先填好的 [`Config`]。M0 只實作桌面版的組合：
//! SQLite、loopback、本機 token；其他選項先保留型別，啟動時回 [`Error::Unsupported`]。

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use pots_proto::InstanceKind;

use crate::Error;

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
    /// 新 project 的對話 agent 顯示名稱。
    pub agent_name: String,
    /// 實例名稱；`None` 時用主機名稱，取不到就是「本機」。只在第一次初始化時寫入。
    pub instance_name: Option<String>,
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
            agent_name: "Pots".into(),
            instance_name: None,
        }
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
        if self.agent_name.trim().is_empty() {
            return Err(Error::Config("agent 名稱不能是空的".into()));
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
