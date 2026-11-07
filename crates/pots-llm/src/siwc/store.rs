//! 憑證檔與 Host ID 檔。兩者分開保存：登出只清憑證，Host ID 跟著安裝實例走。
//!
//! 檔案權限 `0600`（目錄 `0700`），寫入一律「寫暫存檔 → fsync → rename → fsync 目錄」，
//! 中途當掉也不會留下半份檔案。

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::{CredentialStore, StoredCredentials};
use crate::provider::LlmError;

/// 憑證檔名。
pub const CREDENTIALS_FILE: &str = "chatgpt-credentials.json";

/// 把憑證存在 `dir/chatgpt-credentials.json` 的 [`CredentialStore`]。
///
/// 只處理單一行程內的原子性；多個行程共用同一個目錄時要由部署層確保只有一個 refresh owner。
#[derive(Clone, Debug)]
pub struct FileCredentialStore {
    dir: PathBuf,
}

impl FileCredentialStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(CREDENTIALS_FILE)
    }
}

fn io_error(context: &str, error: io::Error) -> LlmError {
    LlmError::Other(format!("{context}：{error}"))
}

#[async_trait::async_trait]
impl CredentialStore for FileCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, LlmError> {
        let path = self.path();
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error("無法讀取憑證檔", error)),
        };
        // 不把 serde 錯誤原文帶出去：錯誤訊息可能引用到檔案內容。
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|_| LlmError::Other(format!("憑證檔格式錯誤：{}", path.display())))
    }

    async fn save(&self, credentials: &StoredCredentials) -> Result<(), LlmError> {
        let json = serde_json::to_vec_pretty(credentials)
            .map_err(|e| LlmError::Other(format!("無法序列化憑證：{e}")))?;
        let dir = self.dir.clone();
        tokio::task::spawn_blocking(move || write_atomic(&dir, CREDENTIALS_FILE, &json))
            .await
            .map_err(|e| LlmError::Other(format!("寫入憑證的工作失敗：{e}")))?
            .map_err(|e| io_error("無法寫入憑證檔", e))
    }
}

/// 建立 `0700` 的目錄（已存在就不動權限）。
fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

/// 開一個新的 `0600` 檔案；已存在就失敗。
fn create_private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

fn temp_path(dir: &Path, name: &str) -> PathBuf {
    let suffix = uuid::Uuid::new_v4().simple();
    dir.join(format!(".{name}.{suffix}.tmp"))
}

/// 把 `content` 寫到暫存檔並 fsync，回傳暫存檔路徑。
fn write_temp(dir: &Path, name: &str, content: &[u8]) -> io::Result<PathBuf> {
    create_private_dir(dir)?;
    let temp = temp_path(dir, name);
    let written = create_private_file(&temp).and_then(|mut file| {
        file.write_all(content)?;
        file.sync_all()
    });
    match written {
        Ok(()) => Ok(temp),
        Err(error) => {
            let _ = fs::remove_file(&temp);
            Err(error)
        }
    }
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// 原子取代 `dir/name`。
fn write_atomic(dir: &Path, name: &str, content: &[u8]) -> io::Result<()> {
    let temp = write_temp(dir, name, content)?;
    if let Err(error) = fs::rename(&temp, dir.join(name)) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    sync_dir(dir)
}

/// 讀取或建立這個 runtime 安裝實例的 Host ID（`urn:uuid:<v4>`），檔案權限 `0600`。
///
/// 檔案已存在但內容不是合法的 Host ID 時回錯誤，不會悄悄換一個新的。
/// 建立時用 hard link 做「不存在才建立」，兩個行程同時建立也只會有一個勝出。
pub fn load_or_create_host_id(path: &Path) -> io::Result<String> {
    if let Some(id) = read_host_id(path)? {
        return Ok(id);
    }
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Host ID 路徑沒有檔名"))?;
    let id = format!("urn:uuid:{}", uuid::Uuid::new_v4().hyphenated());
    let temp = write_temp(dir, name, format!("{id}\n").as_bytes())?;
    let linked = fs::hard_link(&temp, path);
    let _ = fs::remove_file(&temp);
    match linked {
        Ok(()) => {
            sync_dir(dir)?;
            Ok(id)
        }
        // 別的行程先建好了：用它的。
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            read_host_id(path)?.ok_or_else(|| io::Error::other("Host ID 檔案建立後讀不到"))
        }
        Err(error) => Err(error),
    }
}

fn read_host_id(path: &Path) -> io::Result<Option<String>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let id = text.trim();
    let valid = id
        .strip_prefix("urn:uuid:")
        .and_then(|uuid| uuid::Uuid::try_parse(uuid).ok())
        .is_some();
    if valid {
        Ok(Some(id.to_owned()))
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Host ID 檔案內容不合法：{}", path.display()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials() -> StoredCredentials {
        StoredCredentials {
            issuer: "https://auth.example".into(),
            client_id: "oaiapp_1".into(),
            subject: Some("user-1".into()),
            email: None,
            scopes: vec!["openid".into()],
            access_token: Some("secret-access".into()),
            refresh_token: Some("secret-refresh".into()),
            id_token: None,
            expires_at: Some(1),
            earliest_refresh_at: None,
        }
    }

    #[tokio::test]
    async fn file_store_roundtrip_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileCredentialStore::new(dir.path().join("nested"));
        assert!(store.load().await.unwrap().is_none());
        store.save(&credentials()).await.unwrap();
        let mut updated = credentials();
        updated.refresh_token = Some("rotated".into());
        store.save(&updated).await.unwrap();
        let loaded = store.load().await.unwrap().unwrap();
        assert_eq!(loaded.refresh_token.as_deref(), Some("rotated"));
        // 沒有殘留暫存檔。
        let entries: Vec<_> = fs::read_dir(dir.path().join("nested")).unwrap().collect();
        assert_eq!(entries.len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(store.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn debug_hides_tokens() {
        let text = format!("{:?}", credentials());
        assert!(!text.contains("secret"));
    }

    #[test]
    fn host_id_is_stable_and_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host-id");
        let first = load_or_create_host_id(&path).unwrap();
        assert!(first.starts_with("urn:uuid:"));
        assert_eq!(load_or_create_host_id(&path).unwrap(), first);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        fs::write(&path, "garbage").unwrap();
        assert!(load_or_create_host_id(&path).is_err());
    }
}
