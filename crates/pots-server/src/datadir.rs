//! 資料目錄：權限、單一實例鎖與給客戶端的連線資訊（`proposal/deployment.md#桌面版app-內嵌服務`）。
//!
//! - 目錄權限 `0700`。
//! - `pots.lock` 用檔案鎖保護，服務整個生命週期都持有；第二個服務直接失敗，
//!   回報已在跑的那一個的連線資訊，讓 app 改連過去。
//! - 綁定 port 之後寫 `server.json`（`0600`，原子寫入），正常結束時刪除。
//! - SIWC 的 `host-id` 也在這個目錄，由 `pots_llm` 管理，這裡不碰。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use pots_proto::InstanceId;
use serde::{Deserialize, Serialize};

use crate::Error;

const LOCK_FILE: &str = "pots.lock";
const INFO_FILE: &str = "server.json";

/// 啟動 token。`Debug` 不印出內容。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LocalToken(String);

impl LocalToken {
    /// 32 個隨機位元組，base64url（不補 `=`）。
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).expect("作業系統沒有提供亂數");
        Self(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for LocalToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalToken(..)")
    }
}

/// 客戶端連上本機服務需要的資訊，存在 `server.json`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionInfo {
    pub pid: u32,
    pub addr: SocketAddr,
    pub token: LocalToken,
    pub instance_id: InstanceId,
}

impl ConnectionInfo {
    pub fn ws_url(&self) -> String {
        format!("ws://{}/ws", self.addr)
    }

    pub fn http_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

/// 讀 `server.json`。檔案可能是上次異常結束留下的，客戶端要用 `/healthz` 或實際連線確認。
pub fn read_connection_info(data_dir: impl AsRef<Path>) -> Option<ConnectionInfo> {
    let bytes = std::fs::read(data_dir.as_ref().join(INFO_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 持有中的資料目錄。drop 時釋放鎖。
#[derive(Debug)]
pub(crate) struct DataDir {
    path: PathBuf,
    // 只靠存在來持有鎖；行程結束時作業系統也會釋放。
    _lock: File,
}

impl DataDir {
    /// 建立目錄（`0700`）並取得單一實例鎖。
    pub(crate) async fn acquire(path: &Path) -> Result<Self, Error> {
        create_private_dir(path)?;
        let lock_path = path.join(LOCK_FILE);
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)?;
        match lock.try_lock() {
            Ok(()) => Ok(Self {
                path: path.to_path_buf(),
                _lock: lock,
            }),
            Err(std::fs::TryLockError::WouldBlock) => {
                // 另一個服務可能剛拿到鎖、還沒寫 server.json，稍等一下。
                let mut info = read_connection_info(path);
                for _ in 0..30 {
                    if info.is_some() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    info = read_connection_info(path);
                }
                Err(Error::AlreadyRunning(info))
            }
            Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
        }
    }

    /// 原子寫入 `server.json`，權限 `0600`。
    pub(crate) fn write_connection_info(&self, info: &ConnectionInfo) -> Result<(), Error> {
        let json = serde_json::to_vec_pretty(info)?;
        write_private_file(&self.path.join(INFO_FILE), &json)?;
        Ok(())
    }

    pub(crate) fn remove_connection_info(&self) {
        let _ = std::fs::remove_file(self.path.join(INFO_FILE));
    }
}

/// 原子寫入只有自己能讀的檔案（`0600`）：先寫 `<name>.tmp`、fsync，再 rename 蓋掉。
/// 上層目錄要已經存在。
pub(crate) fn write_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    let _ = std::fs::remove_file(&tmp);
    let mut file = private_file_options().open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn private_file_options() -> OpenOptions {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts
}

/// 建立 `base` 底下的相對目錄 `rel`，中間每一層都設成 `0700`（例如 `auth/providers` 的 `auth/`）。
/// `base` 本身不動。
pub(crate) fn create_private_dirs_under(base: &Path, rel: &Path) -> std::io::Result<()> {
    let mut dir = base.to_path_buf();
    for part in rel.components() {
        dir.push(part);
        create_private_dir(&dir)?;
    }
    Ok(())
}

pub(crate) fn create_private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_32_bytes_base64url() {
        let a = LocalToken::generate();
        let b = LocalToken::generate();
        assert_ne!(a, b);
        assert_eq!(a.as_str().len(), 43);
        assert!(
            a.as_str()
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_eq!(format!("{a:?}"), "LocalToken(..)");
    }

    #[tokio::test]
    async fn lock_and_info_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("data");
        let held = DataDir::acquire(&dir).await.unwrap();

        let info = ConnectionInfo {
            pid: 1,
            addr: "127.0.0.1:1234".parse().unwrap(),
            token: LocalToken::generate(),
            instance_id: InstanceId::from_u64(9),
        };
        held.write_connection_info(&info).unwrap();
        assert_eq!(read_connection_info(&dir), Some(info.clone()));
        assert!(!format!("{info:?}").contains(info.token.as_str()));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir), 0o700);
            assert_eq!(mode(&dir.join(INFO_FILE)), 0o600);
        }

        match DataDir::acquire(&dir).await {
            Err(Error::AlreadyRunning(Some(got))) => assert_eq!(got, info),
            other => panic!("預期 AlreadyRunning，得到 {other:?}"),
        }

        held.remove_connection_info();
        drop(held);
        assert_eq!(read_connection_info(&dir), None);
        DataDir::acquire(&dir).await.unwrap();
    }
}
