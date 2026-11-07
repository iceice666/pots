//! 模型 provider 設定與路由。
//!
//! - 內建的 ChatGPT 方案（[`ProviderId::CHATGPT`]）不是資料庫的一列：一定是清單第一筆，
//!   推論走組裝端注入的 [`Deps::provider`](crate::Deps::provider)，可以指定模型
//!   （`instance_settings.chatgpt_model`，空字串表示自動選）。
//! - API provider（OpenAI／Anthropic 相容）存在 `model_connections`；API key 不進資料庫，
//!   存在 `data_dir/auth/providers/<id>.json`（`0600`，原子寫入），`secret_ref` 記相對路徑。
//!   key 不會出現在日誌、`Debug` 與事件裡，事件只帶末四碼（`key_hint`）。
//!   OpenAI 相容的 key 可以不填（本機的 Ollama 等）：沒有 key 檔、`key_hint` 是 `None`，
//!   provider 用空字串建立（不送 `Authorization`）。Anthropic 相容的一定要有 key。
//! - 使用中的 provider 存在 `instance_settings.active_provider`；沒有這列表示還沒設定過，
//!   啟動時套用 [`Config::default_provider`](crate::Config::default_provider)，
//!   或在 ChatGPT 已登入時選 ChatGPT（讓舊使用者升級後照常可用）。
//!
//! 所有變更都在 [`Hub`](crate::hub::Hub) 的日誌鎖內進行：資料庫、key 檔與 `Providers`
//! 事件一起寫，commit 之後才換掉記憶體裡的設定，所以快照、事件與路由一致。
//!
//! run 開始時用 [`ProviderRouter`] 依當下使用中的 provider 決定一次 [`Route`]，整個 run 都用它。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pots_llm::{LlmError, ModelProvider};
use pots_proto::{
    ModelOption, ProviderDraft, ProviderId, ProviderKind, ProviderView, ProvidersView, ServerEvent,
    Timestamp,
};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::store::{ConnectionRow, Store};
use crate::{App, Error};

pub(crate) const SETTING_ACTIVE: &str = "active_provider";
pub(crate) const SETTING_CHATGPT_MODEL: &str = "chatgpt_model";
/// API key 檔案的目錄，相對於資料目錄。
const SECRET_DIR: &str = "auth/providers";
/// `TestProvider` 列模型的時限。
const TEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_NAME: usize = 100;
/// key 至少這麼長才在 `key_hint` 顯示末四碼。
const MIN_HINT_KEY_LEN: usize = 16;
/// 換了主機卻沒有重新輸入 key：不把已存的 key 送到別的主機。
const ORIGIN_CHANGED: &str = "Base URL 換了主機，請重新輸入 API key";
/// 寫入中的新 key 檔的副檔名；commit 之後才 rename 成正式的檔名。
const STAGED_SUFFIX: &str = ".new";
const CHATGPT_NAME: &str = "ChatGPT";

/// 建立 API provider（OpenAI／Anthropic 相容）的工廠，由組裝端注入（[`Deps::api_providers`]）。
/// 正式版是 [`HttpApiProviders`](crate::service::HttpApiProviders)；測試注入腳本化的 provider。
///
/// 回傳的錯誤訊息會顯示給使用者，不能包含 API key。
///
/// [`Deps::api_providers`]: crate::Deps::api_providers
#[async_trait::async_trait]
pub trait ApiProviderFactory: Send + Sync {
    /// 用這組設定建立推論用的 provider。只做本機檢查，不連網。
    fn build(
        &self,
        kind: ProviderKind,
        base_url: &Url,
        api_key: &str,
    ) -> Result<Arc<dyn ModelProvider>, String>;

    /// 列出可用的模型，用來驗證 base URL 與 key。
    async fn list_models(
        &self,
        kind: ProviderKind,
        base_url: &Url,
        api_key: &str,
    ) -> Result<Vec<ModelOption>, LlmError>;

    /// 內建 ChatGPT 方案的模型清單（需要已登入）。預設不支援。
    async fn chatgpt_models(&self) -> Result<Vec<ModelOption>, LlmError> {
        Err(LlmError::Unsupported(
            "這個組裝不能列出 ChatGPT 的模型".into(),
        ))
    }
}

/// 不支援 API provider 的工廠：[`Deps::new`](crate::Deps::new) 的預設值。
pub struct NoApiProviders;

#[async_trait::async_trait]
impl ApiProviderFactory for NoApiProviders {
    fn build(&self, _: ProviderKind, _: &Url, _: &str) -> Result<Arc<dyn ModelProvider>, String> {
        Err("這個組裝不支援 API provider".into())
    }

    async fn list_models(
        &self,
        _: ProviderKind,
        _: &Url,
        _: &str,
    ) -> Result<Vec<ModelOption>, LlmError> {
        Err(LlmError::Unsupported("這個組裝不支援 API provider".into()))
    }
}

/// API key。`Debug` 不印出內容。
#[derive(Clone, PartialEq, Eq)]
struct ApiKey(String);

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKey(..)")
    }
}

impl ApiKey {
    /// `…` 加末四碼；短於 16 字的 key 只顯示 `…`（表示有 key），免得露出太多。
    fn hint(&self) -> String {
        let chars: Vec<char> = self.0.chars().collect();
        if chars.len() < MIN_HINT_KEY_LEN {
            return "…".into();
        }
        let tail: String = chars[chars.len() - 4..].iter().collect();
        format!("…{tail}")
    }
}

/// key 檔的內容。
#[derive(Serialize, Deserialize)]
struct SecretFile {
    api_key: String,
}

#[derive(Clone)]
struct ApiEntry {
    row: ConnectionRow,
    key: Option<ApiKey>,
    /// 建好的 provider；設定有問題（例如 key 檔不見）時是推論要回的錯誤。
    built: Result<Arc<dyn ModelProvider>, LlmError>,
}

impl ApiEntry {
    fn view(&self) -> ProviderView {
        ProviderView {
            id: self.row.id,
            kind: self.row.kind,
            name: self.row.name.clone(),
            base_url: self.row.base_url.clone(),
            model: self.row.model.clone(),
            key_hint: self.key.as_ref().map(ApiKey::hint),
        }
    }
}

/// 記憶體裡的完整設定。只在持有日誌鎖時整份替換。
#[derive(Clone)]
struct ProviderState {
    active: Option<ProviderId>,
    chatgpt_model: String,
    api: Vec<ApiEntry>,
}

impl ProviderState {
    fn view(&self) -> ProvidersView {
        let mut items = vec![ProviderView {
            id: ProviderId::CHATGPT,
            kind: ProviderKind::ChatGpt,
            name: CHATGPT_NAME.into(),
            base_url: String::new(),
            model: self.chatgpt_model.clone(),
            key_hint: None,
        }];
        items.extend(self.api.iter().map(ApiEntry::view));
        ProvidersView {
            active: self.active,
            items,
        }
    }

    fn entry(&self, id: ProviderId) -> Option<&ApiEntry> {
        self.api.iter().find(|e| e.row.id == id)
    }

    fn contains(&self, id: ProviderId) -> bool {
        id == ProviderId::CHATGPT || self.entry(id).is_some()
    }
}

/// provider 設定與建好的 API provider。
pub(crate) struct ProviderRegistry {
    data_dir: PathBuf,
    factory: Arc<dyn ApiProviderFactory>,
    state: Mutex<ProviderState>,
}

/// [`ProviderRegistry::load`] 的結果：還沒寫進資料庫的初始選擇。
pub(crate) struct Loaded {
    pub registry: ProviderRegistry,
    /// 資料庫還沒有 `active_provider`，啟動時決定了一個，要寫進去。
    pub initial_active: Option<ProviderId>,
}

impl ProviderRegistry {
    /// 從資料庫與 key 檔載入。`fallback` 是資料庫還沒有 `active_provider` 時要用的選擇。
    pub async fn load(
        store: &Store,
        data_dir: &Path,
        factory: Arc<dyn ApiProviderFactory>,
        fallback: Option<ProviderId>,
    ) -> Result<Loaded, Error> {
        let registry = Self {
            data_dir: data_dir.to_path_buf(),
            factory,
            state: Mutex::new(ProviderState {
                active: None,
                chatgpt_model: String::new(),
                api: Vec::new(),
            }),
        };
        let mut api = Vec::new();
        for row in store.list_connections().await? {
            let key = match &row.secret_ref {
                Some(rel) => registry.read_secret(rel),
                None => None,
            };
            api.push(registry.entry(row, key));
        }
        let chatgpt_model = store
            .setting(SETTING_CHATGPT_MODEL)
            .await?
            .unwrap_or_default();
        let mut state = ProviderState {
            active: None,
            chatgpt_model,
            api,
        };
        let mut initial_active = None;
        match store.setting(SETTING_ACTIVE).await? {
            None => {
                state.active = fallback;
                initial_active = fallback;
            }
            Some(s) if s.is_empty() => {}
            Some(s) => match s.parse::<ProviderId>() {
                Ok(id) if state.contains(id) => state.active = Some(id),
                _ => tracing::warn!(value = %s, "使用中的 provider 不存在，改成未設定"),
            },
        }
        *registry.state.lock().unwrap() = state;
        Ok(Loaded {
            registry,
            initial_active,
        })
    }

    pub fn view(&self) -> ProvidersView {
        self.state.lock().unwrap().view()
    }

    fn snapshot(&self) -> ProviderState {
        self.state.lock().unwrap().clone()
    }

    fn replace(&self, state: ProviderState) {
        *self.state.lock().unwrap() = state;
    }

    /// 建一筆 API provider；建不起來時記下推論要回的錯誤。
    /// 有 `secret_ref` 但讀不到 key、或必須有 key 的種類沒有 key 時，推論回 `InvalidCredentials`。
    fn entry(&self, row: ConnectionRow, key: Option<ApiKey>) -> ApiEntry {
        let missing = key.is_none() && (row.secret_ref.is_some() || key_required(row.kind));
        let built = match Url::parse(&row.base_url) {
            _ if missing => Err(LlmError::InvalidCredentials(
                "找不到已存的 API key，請重新輸入".into(),
            )),
            Err(_) => Err(LlmError::Other("provider 的 base URL 格式不對".into())),
            Ok(url) => self
                .factory
                .build(row.kind, &url, key_str(&key))
                .map_err(|m| LlmError::Other(format!("無法建立 provider：{m}"))),
        };
        if let Err(e) = &built {
            tracing::warn!(provider = %row.id, error = %e, "無法建立模型 provider");
        }
        ApiEntry { row, key, built }
    }

    fn secret_rel(id: ProviderId) -> String {
        format!("{SECRET_DIR}/{id}.json")
    }

    fn read_secret(&self, rel: &str) -> Option<ApiKey> {
        let path = self.data_dir.join(rel);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "無法讀取 API key 檔");
                return None;
            }
        };
        match serde_json::from_slice::<SecretFile>(&bytes) {
            Ok(f) => Some(ApiKey(f.api_key)),
            Err(_) => {
                tracing::warn!(path = %path.display(), "API key 檔格式不對");
                None
            }
        }
    }

    /// 把新 key 寫到 `<rel>.new`（`0600`）。要等資料庫 commit 之後用
    /// [`install_secret`](Self::install_secret) 換上，失敗時用 `remove_secret` 刪掉，
    /// 原本的 key 檔在 commit 之前都不動。
    fn stage_secret(&self, rel: &str, key: &ApiKey) -> std::io::Result<String> {
        if let Some(dir) = Path::new(rel).parent() {
            crate::datadir::create_private_dirs_under(&self.data_dir, dir)?;
        }
        let json = serde_json::to_vec(&SecretFile {
            api_key: key.0.clone(),
        })?;
        let staged = format!("{rel}{STAGED_SUFFIX}");
        crate::datadir::write_private_file(&self.data_dir.join(&staged), &json)?;
        Ok(staged)
    }

    /// 把暫存的 key 檔換成正式的檔名。
    fn install_secret(&self, staged: &str, rel: &str) {
        if let Err(e) = std::fs::rename(self.data_dir.join(staged), self.data_dir.join(rel)) {
            tracing::error!(error = %e, "無法換上新的 API key 檔；重啟後要重新輸入 key");
        }
    }

    fn remove_secret(&self, rel: &str) {
        let path = self.data_dir.join(rel);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(path = %path.display(), error = %e, "無法刪除 API key 檔"),
        }
    }
}

// ---------------------------------------------------------------------------
// 推論路由

/// 一個 run 用的 provider、模型與 provider ID。run 開始時決定一次，整個 run 都用它
/// （中途在設定切換 provider 只影響下一個 run）。
pub(crate) struct Route {
    pub id: ProviderId,
    pub provider: Arc<dyn ModelProvider>,
    pub model: String,
}

/// 依使用中的 provider 決定 run 的 [`Route`]。
pub(crate) struct ProviderRouter {
    registry: Arc<ProviderRegistry>,
    chatgpt: Arc<dyn ModelProvider>,
    /// ChatGPT 沒有指定模型時用的值（[`Deps::model`](crate::Deps::model)）。
    chatgpt_default_model: String,
}

impl ProviderRouter {
    pub fn new(
        registry: Arc<ProviderRegistry>,
        chatgpt: Arc<dyn ModelProvider>,
        chatgpt_default_model: String,
    ) -> Self {
        Self {
            registry,
            chatgpt,
            chatgpt_default_model,
        }
    }

    /// 沒有使用中的 provider 時回 [`LlmError::NotConfigured`]；API provider 沒選模型時回
    /// `Unsupported`；設定有問題（例如 key 檔不見）時回建立時記下的錯誤。
    pub fn resolve(&self) -> Result<Route, LlmError> {
        let state = self.registry.state.lock().unwrap();
        match state.active {
            None => Err(LlmError::NotConfigured),
            Some(ProviderId::CHATGPT) => {
                let model = if state.chatgpt_model.is_empty() {
                    self.chatgpt_default_model.clone()
                } else {
                    state.chatgpt_model.clone()
                };
                Ok(Route {
                    id: ProviderId::CHATGPT,
                    provider: self.chatgpt.clone(),
                    model,
                })
            }
            Some(id) => {
                let entry = state.entry(id).ok_or(LlmError::NotConfigured)?;
                if entry.row.model.is_empty() {
                    return Err(LlmError::Unsupported("請在設定選擇模型".into()));
                }
                Ok(Route {
                    id,
                    provider: entry.built.clone()?,
                    model: entry.row.model.clone(),
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 指令

fn internal(e: Error) -> String {
    tracing::error!(error = %e, "provider 指令執行失敗");
    "伺服器內部錯誤".into()
}

/// 驗證 base URL：http(s)、有 host。回傳正規化（去掉結尾 `/`）的字串與解析結果。
fn parse_base_url(raw: &str) -> Result<(String, Url), String> {
    let trimmed = raw.trim().trim_end_matches('/');
    let url = Url::parse(trimmed).map_err(|_| "base URL 格式不對".to_string())?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none_or(str::is_empty) {
        return Err("base URL 必須是 http:// 或 https:// 開頭的網址".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("base URL 不能包含帳號或密碼，API key 請填在 key 欄位".into());
    }
    Ok((trimmed.to_string(), url))
}

/// 存的 base URL 與新的網址是不是同一個 origin（scheme、host、port）。
fn same_origin(stored: &str, url: &Url) -> bool {
    Url::parse(stored).is_ok_and(|s| s.origin() == url.origin())
}

/// 這個種類的 provider 必須有 API key。OpenAI 相容的可以沒有（例如本機的 Ollama，
/// 沒有 key 時不送 `Authorization`）。
fn key_required(kind: ProviderKind) -> bool {
    kind != ProviderKind::OpenAiCompatible
}

/// 沒有 key 時給 provider 空字串。
fn key_str(key: &Option<ApiKey>) -> &str {
    key.as_ref().map_or("", |k| k.0.as_str())
}

/// 空白或空字串的 key 視同沒有填（沿用已存的 key）。
fn draft_key(draft: &ProviderDraft) -> Option<ApiKey> {
    draft
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(|k| ApiKey(k.to_string()))
}

/// `SaveProvider`。
pub(crate) async fn save(app: &App, draft: ProviderDraft) -> Result<(), String> {
    let registry = &app.providers;
    let mut txn = app.hub.begin().await.map_err(internal)?;
    // 變更只在日誌鎖內發生，拿到鎖之後讀到的設定在 commit 前不會變。
    let mut state = registry.snapshot();
    // 暫存的新 key 檔與正式檔名：commit 之後換上，失敗時刪掉。
    let mut staged: Option<(String, String)> = None;

    if draft.kind == ProviderKind::ChatGpt {
        // 內建的 ChatGPT 只能改模型；名稱、base URL 與 key 忽略。
        if draft.id != Some(ProviderId::CHATGPT) {
            return Err("ChatGPT 方案只有內建的一筆，不能新增".into());
        }
        let model = draft.model.trim().to_string();
        Store::set_setting(txn.conn(), SETTING_CHATGPT_MODEL, &model)
            .await
            .map_err(internal)?;
        state.chatgpt_model = model;
    } else {
        if draft.id == Some(ProviderId::CHATGPT) {
            return Err("不能改變 provider 的種類".into());
        }
        let name = draft.name.trim().to_string();
        if name.is_empty() {
            return Err("provider 名稱不能是空的".into());
        }
        if name.chars().count() > MAX_NAME {
            return Err(format!("provider 名稱最多 {MAX_NAME} 個字"));
        }
        let (base_url, url) = parse_base_url(&draft.base_url)?;
        let typed_key = draft_key(&draft);
        let model = draft.model.trim().to_string();

        let (row, key, is_new) = match draft.id {
            None => {
                let key = typed_key.clone();
                if key.is_none() && key_required(draft.kind) {
                    return Err("請輸入 API key".into());
                }
                let id = ProviderId::new();
                let row = ConnectionRow {
                    id,
                    kind: draft.kind,
                    name,
                    base_url,
                    model,
                    secret_ref: key.is_some().then(|| ProviderRegistry::secret_rel(id)),
                    created_at: Timestamp::now(),
                };
                (row, key, true)
            }
            Some(id) => {
                let Some(existing) = state.entry(id) else {
                    return Err("找不到這個 provider".into());
                };
                if existing.row.kind != draft.kind {
                    return Err("不能改變 provider 的種類".into());
                }
                if typed_key.is_none()
                    && existing.key.is_some()
                    && !same_origin(&existing.row.base_url, &url)
                {
                    return Err(ORIGIN_CHANGED.into());
                }
                let key = typed_key.clone().or_else(|| existing.key.clone());
                if key.is_none() && key_required(draft.kind) {
                    return Err("請輸入 API key".into());
                }
                let secret_ref = if typed_key.is_some() {
                    Some(ProviderRegistry::secret_rel(id))
                } else {
                    existing.row.secret_ref.clone()
                };
                let row = ConnectionRow {
                    name,
                    base_url,
                    model,
                    secret_ref,
                    ..existing.row.clone()
                };
                (row, key, false)
            }
        };

        let built = registry
            .factory
            .build(row.kind, &url, key_str(&key))
            .map_err(|m| format!("無法建立 provider：{m}"))?;
        if let Some(typed) = &typed_key {
            let rel = ProviderRegistry::secret_rel(row.id);
            let tmp = registry.stage_secret(&rel, typed).map_err(|e| {
                tracing::error!(error = %e, "無法寫入 API key 檔");
                "無法儲存 API key".to_string()
            })?;
            staged = Some((tmp, rel));
        }
        let saved: Result<(), Error> = async {
            Store::upsert_connection(txn.conn(), &row).await?;
            if is_new && state.active.is_none() {
                Store::set_setting(txn.conn(), SETTING_ACTIVE, &row.id.to_string()).await?;
            }
            Ok(())
        }
        .await;
        if let Err(e) = saved {
            if let Some((tmp, _)) = &staged {
                registry.remove_secret(tmp);
            }
            return Err(internal(e));
        }
        if is_new && state.active.is_none() {
            state.active = Some(row.id);
        }
        let entry = ApiEntry {
            row,
            key,
            built: Ok(built),
        };
        match state.api.iter_mut().find(|e| e.row.id == entry.row.id) {
            Some(slot) => *slot = entry,
            None => state.api.push(entry),
        }
    }

    let providers = state.view();
    let committed: Result<(), Error> = async {
        txn.push(|seq| ServerEvent::Providers { seq, providers })
            .await?;
        txn.commit_then(|| {
            if let Some((tmp, rel)) = &staged {
                registry.install_secret(tmp, rel);
            }
            registry.replace(state);
        })
        .await
    }
    .await;
    if let Err(e) = committed {
        if let Some((tmp, _)) = &staged {
            registry.remove_secret(tmp);
        }
        return Err(internal(e));
    }
    Ok(())
}

/// `DeleteProvider`。刪掉使用中的那個時 `active` 變成 `None`。
pub(crate) async fn delete(app: &App, id: ProviderId) -> Result<(), String> {
    if id == ProviderId::CHATGPT {
        return Err("內建的 ChatGPT 不能刪除".into());
    }
    let registry = &app.providers;
    let mut txn = app.hub.begin().await.map_err(internal)?;
    let mut state = registry.snapshot();
    let Some(pos) = state.api.iter().position(|e| e.row.id == id) else {
        return Err("找不到這個 provider".into());
    };
    let removed = state.api.remove(pos);
    if state.active == Some(id) {
        state.active = None;
    }
    let providers = state.view();
    let result: Result<(), Error> = async {
        Store::delete_connection(txn.conn(), id).await?;
        if providers.active.is_none() {
            Store::set_setting(txn.conn(), SETTING_ACTIVE, "").await?;
        }
        txn.push(|seq| ServerEvent::Providers { seq, providers })
            .await?;
        txn.commit_then(|| registry.replace(state)).await
    }
    .await;
    result.map_err(internal)?;
    // commit 之後才刪 key 檔：資料庫寫入失敗時設定仍完整。
    if let Some(rel) = &removed.row.secret_ref {
        registry.remove_secret(rel);
    }
    Ok(())
}

/// `SetActiveProvider`。
pub(crate) async fn set_active(app: &App, id: ProviderId) -> Result<(), String> {
    let registry = &app.providers;
    let mut txn = app.hub.begin().await.map_err(internal)?;
    let mut state = registry.snapshot();
    if !state.contains(id) {
        return Err("找不到這個 provider".into());
    }
    state.active = Some(id);
    let providers = state.view();
    let result: Result<(), Error> = async {
        Store::set_setting(txn.conn(), SETTING_ACTIVE, &id.to_string()).await?;
        txn.push(|seq| ServerEvent::Providers { seq, providers })
            .await?;
        txn.commit_then(|| registry.replace(state)).await
    }
    .await;
    result.map_err(internal)
}

/// `TestProvider`：用草稿列出模型，不儲存。回給送出指令的連線。
///
/// ChatGPT 方案用已登入的帳號列模型（經 [`ApiProviderFactory::chatgpt_models`]）；
/// 沒登入時回「請先登入 ChatGPT」。
pub(crate) async fn test(
    registry: &ProviderRegistry,
    request: u64,
    draft: ProviderDraft,
) -> ServerEvent {
    let result = test_inner(registry, &draft).await;
    if let Err(message) = &result {
        tracing::info!(kind = ?draft.kind, %message, "provider 測試失敗");
    }
    ServerEvent::ProviderTest { request, result }
}

async fn test_inner(
    registry: &ProviderRegistry,
    draft: &ProviderDraft,
) -> Result<Vec<ModelOption>, String> {
    if draft.kind == ProviderKind::ChatGpt {
        return match tokio::time::timeout(TEST_TIMEOUT, registry.factory.chatgpt_models()).await {
            Err(_) => Err(timeout_message()),
            Ok(r) => r.map_err(|e| describe(&e, None)),
        };
    }
    let (_, url) = parse_base_url(&draft.base_url)?;
    let key = match draft_key(draft) {
        Some(k) => Some(k),
        None => {
            let stored = draft.id.and_then(|id| {
                let state = registry.state.lock().unwrap();
                state
                    .entry(id)
                    .map(|e| (e.row.kind, e.row.base_url.clone(), e.key.clone()))
            });
            match stored {
                Some((kind, _, _)) if kind != draft.kind => {
                    return Err("不能改變 provider 的種類".into());
                }
                Some((_, stored_url, Some(_))) if !same_origin(&stored_url, &url) => {
                    return Err(ORIGIN_CHANGED.into());
                }
                Some((_, _, Some(key))) => Some(key),
                _ if key_required(draft.kind) => return Err("請輸入 API key".into()),
                _ => None,
            }
        }
    };
    let listed = tokio::time::timeout(
        TEST_TIMEOUT,
        registry
            .factory
            .list_models(draft.kind, &url, key_str(&key)),
    )
    .await;
    match listed {
        Err(_) => Err(timeout_message()),
        Ok(Ok(models)) => Ok(models),
        Ok(Err(e)) => Err(describe(&e, key.as_ref())),
    }
}

fn timeout_message() -> String {
    format!("連線逾時（{} 秒內沒有回應）", TEST_TIMEOUT.as_secs())
}

/// 測試失敗的簡短說明。保險起見把 key 從訊息裡拿掉。
fn describe(e: &LlmError, key: Option<&ApiKey>) -> String {
    let message = match e {
        LlmError::InvalidCredentials(_) => "API key 無效或沒有權限".to_string(),
        LlmError::NeedsLogin => "請先登入 ChatGPT".into(),
        LlmError::NotEntitled(m) => format!("這個帳號不能使用：{m}"),
        LlmError::UsageLimit => "額度已用完".into(),
        LlmError::Unavailable(m) => format!("無法連線：{m}"),
        LlmError::Unsupported(m) => format!("不支援：{m}"),
        LlmError::Interrupted(m) => format!("連線中斷：{m}"),
        LlmError::Cancelled => "已取消".into(),
        LlmError::NotConfigured => "還沒設定 provider".into(),
        LlmError::Other(m) => m.clone(),
    };
    match key {
        Some(k) if !k.0.is_empty() => message.replace(&k.0, "…"),
        _ => message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_hint_and_debug() {
        let key = ApiKey("sk-secret-abcd1234".into());
        assert_eq!(key.hint(), "…1234");
        assert_eq!(ApiKey("short".into()).hint(), "…");
        assert_eq!(ApiKey("sk-fifteen-1234".into()).hint(), "…");
        assert_eq!(ApiKey("sk-sixteen-12345".into()).hint(), "…2345");
        assert!(!format!("{key:?}").contains("secret"));
    }

    #[test]
    fn base_url_validation() {
        assert_eq!(
            parse_base_url(" https://api.example.com/v1/ ").unwrap().0,
            "https://api.example.com/v1"
        );
        assert!(parse_base_url("http://localhost:11434/v1").is_ok());
        assert!(parse_base_url("ftp://example.com").is_err());
        assert!(parse_base_url("api.example.com").is_err());
        assert!(parse_base_url("").is_err());
        assert!(parse_base_url("https://user:pw@api.example.com/v1").is_err());
        assert!(parse_base_url("https://user@api.example.com/v1").is_err());
    }

    #[test]
    fn origin_comparison() {
        let url = |s| Url::parse(s).unwrap();
        let stored = "https://api.example.com/v1";
        assert!(same_origin(stored, &url("https://api.example.com/v2")));
        assert!(same_origin(stored, &url("https://api.example.com:443/v1")));
        assert!(!same_origin(stored, &url("http://api.example.com/v1")));
        assert!(!same_origin(stored, &url("https://evil.example.com/v1")));
        assert!(!same_origin(
            stored,
            &url("https://api.example.com:8443/v1")
        ));
    }

    #[test]
    fn describe_redacts_key() {
        let key = ApiKey("sk-leaky-9999".into());
        let msg = describe(&LlmError::Other("bad key sk-leaky-9999".into()), Some(&key));
        assert!(!msg.contains("sk-leaky"));
    }
}
