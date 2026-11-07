//! 正式組裝：ChatGPT Plan（SIWC 授權）加 Responses provider，以及使用者在設定新增的
//! API provider（[`HttpApiProviders`]：OpenAI／Anthropic 相容）。
//!
//! `pots-server` bin 與桌面 app 的 `--serve` 都從這裡啟動，測試則直接呼叫 [`start`]
//! 注入假的 provider。資料目錄裡和授權有關的檔案：
//!
//! - `host-id`：這個 runtime 安裝實例的 SIWC host ID，跟憑證分開保存，登出也不刪。
//! - `auth/chatgpt-credentials.json`：issued client ID 與 token（`0600`）。
//! - `auth/providers/<id>.json`：API provider 的 key（`0600`，由伺服器管理）。
//!
//! ChatGPT 的模型：設定裡指定的優先；其次是環境變數 `POTS_CHAT_MODEL`；都沒有時登入後取
//! 帳號可用模型清單的第一個（伺服器排序），快取到授權狀態改變為止。
//!
//! [`start`]: crate::start

use std::path::Path;
use std::sync::Arc;

use pots_llm::anthropic::AnthropicProvider;
use pots_llm::openai_compat::OpenAiCompatProvider;
use pots_llm::responses::{DEFAULT_BASE_URL, ResponsesProvider};
use pots_llm::siwc::{
    AuthStatus, FileCredentialStore, SiwcAuth, SiwcConfig, load_or_create_host_id,
};
use pots_llm::{EventStream, LlmError, ModelProvider, ModelRequest};
use pots_proto::{ModelAuthView, ModelOption, ProviderKind};
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::{ApiProviderFactory, Config, Deps, Error, ModelAuthControl};

/// [`Deps::model`] 用這個值時，由 [`AutoModel`] 在第一次推論前決定實際的模型。
pub const AUTO_MODEL: &str = "auto";

/// 建立連到 ChatGPT 的 [`Deps`]。`model` 為 `None` 時自動選模型。
pub async fn chatgpt_deps(data_dir: &Path, model: Option<String>) -> Result<Deps, Error> {
    crate::datadir::create_private_dir(data_dir)?;
    let host_id = load_or_create_host_id(&data_dir.join("host-id"))?;
    let http = pots_llm::reqwest::Client::builder()
        .user_agent(concat!("pots/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| Error::Config(format!("無法建立 HTTP client：{e}")))?;
    let store = Arc::new(FileCredentialStore::new(data_dir.join("auth")));
    let auth = SiwcAuth::open(SiwcConfig::new(host_id), store, http.clone())
        .await
        .map_err(|e| Error::Config(format!("無法載入 ChatGPT 授權：{e}")))?;
    let base = Url::parse(DEFAULT_BASE_URL).map_err(|e| Error::Config(e.to_string()))?;
    let provider = ResponsesProvider::new(base, auth.clone(), http.clone());
    let control = SiwcControl::new(auth);
    let auto = Arc::new(AutoModel::new(provider, model.is_none(), control.status()));
    Ok(Deps {
        provider: auto.clone(),
        model: model.unwrap_or_else(|| AUTO_MODEL.into()),
        model_auth: Arc::new(control),
        api_providers: Arc::new(HttpApiProviders::new(http, Some(auto))),
    })
}

/// 用 ChatGPT 授權啟動服務，直到收到結束訊號。
pub async fn run(config: Config, model: Option<String>) -> Result<(), Error> {
    let deps = chatgpt_deps(&config.data_dir, model).await?;
    crate::run_until_signal(config, deps).await
}

/// 給桌面 app 的 `--serve`：建立 runtime 與日誌，以桌面版設定跑服務。
pub fn run_blocking(data_dir: &Path) -> Result<(), Error> {
    // 趁執行檔還沒被換掉，先記下這個行程的 build（寫進 `server.json`）。
    let _ = crate::BuildInfo::current();
    init_tracing();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let model = std::env::var("POTS_CHAT_MODEL")
        .ok()
        .filter(|m| !m.trim().is_empty());
    runtime.block_on(run(Config::desktop(data_dir), model))
}

/// 日誌寫到 stderr（桌面 app 把它導到資料目錄的 `server.log`）。`RUST_LOG` 可調整等級。
pub fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .try_init();
}

// ---------------------------------------------------------------------------

/// 把 [`SiwcAuth`] 包成伺服器的 [`ModelAuthControl`]。
pub struct SiwcControl {
    auth: Arc<SiwcAuth>,
    view: watch::Receiver<ModelAuthView>,
}

impl SiwcControl {
    /// 需要在 tokio runtime 裡呼叫：會開一個 task 把 SIWC 狀態轉成 [`ModelAuthView`]。
    pub fn new(auth: Arc<SiwcAuth>) -> Self {
        let mut source = auth.status();
        let (tx, view) = watch::channel(auth_view(&source.borrow_and_update()));
        tokio::spawn(async move {
            while source.changed().await.is_ok() {
                let next = auth_view(&source.borrow_and_update());
                tx.send_if_modified(|current| {
                    let changed = *current != next;
                    *current = next;
                    changed
                });
                if tx.is_closed() {
                    break;
                }
            }
        });
        Self { auth, view }
    }
}

fn auth_view(status: &AuthStatus) -> ModelAuthView {
    match status {
        AuthStatus::SignedOut => ModelAuthView::SignedOut,
        AuthStatus::Pending { url } => ModelAuthView::Pending { url: url.clone() },
        AuthStatus::SignedIn {
            email,
            plan_usage: true,
        } => ModelAuthView::SignedIn {
            account: email.clone(),
        },
        AuthStatus::SignedIn {
            email,
            plan_usage: false,
        } => ModelAuthView::Error {
            message: format!(
                "{}沒有授予 ChatGPT 方案的推論權限。重新登入時請同意「使用 ChatGPT 方案」，或確認帳號方案符合資格。",
                email
                    .as_deref()
                    .map(|e| format!("{e} "))
                    .unwrap_or_default()
            ),
        },
        AuthStatus::Error { message } => ModelAuthView::Error {
            message: message.clone(),
        },
    }
}

#[async_trait::async_trait]
impl ModelAuthControl for SiwcControl {
    fn status(&self) -> watch::Receiver<ModelAuthView> {
        self.view.clone()
    }

    async fn start_login(&self) -> Result<String, LlmError> {
        self.auth.start_login().await
    }

    async fn cancel_login(&self) {
        self.auth.cancel_login().await;
    }

    async fn logout(&self) -> Result<(), LlmError> {
        let outcome = self.auth.logout().await?;
        tracing::info!(?outcome, "已登出 ChatGPT");
        Ok(())
    }
}

// ---------------------------------------------------------------------------

/// 沒有指定模型時，用帳號模型清單的第一個。授權狀態一變（換帳號、重新登入）就重新查。
pub struct AutoModel {
    inner: ResponsesProvider,
    auto: bool,
    auth: Mutex<watch::Receiver<ModelAuthView>>,
    chosen: Mutex<Option<String>>,
}

impl AutoModel {
    pub fn new(inner: ResponsesProvider, auto: bool, auth: watch::Receiver<ModelAuthView>) -> Self {
        Self {
            inner,
            auto,
            auth: Mutex::new(auth),
            chosen: Mutex::new(None),
        }
    }

    async fn resolve(&self) -> Result<String, LlmError> {
        {
            let mut auth = self.auth.lock().await;
            if auth.has_changed().unwrap_or(false) {
                auth.mark_unchanged();
                *self.chosen.lock().await = None;
            }
        }
        let mut chosen = self.chosen.lock().await;
        if let Some(model) = chosen.as_ref() {
            return Ok(model.clone());
        }
        let models = self.inner.list_models().await?;
        let first = models
            .into_iter()
            .next()
            .ok_or_else(|| LlmError::NotEntitled("這個帳號沒有可用的模型".into()))?;
        tracing::info!(model = %first.slug, "自動選擇模型");
        *chosen = Some(first.slug.clone());
        Ok(first.slug)
    }
}

impl AutoModel {
    /// 帳號可用的模型（需要已登入）。
    pub async fn list_models(&self) -> Result<Vec<pots_llm::responses::ModelInfo>, LlmError> {
        self.inner.list_models().await
    }
}

#[async_trait::async_trait]
impl ModelProvider for AutoModel {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn stream(
        &self,
        mut request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, LlmError> {
        if self.auto && request.model == AUTO_MODEL {
            request.model = self.resolve().await?;
        }
        self.inner.stream(request, cancel).await
    }
}

// ---------------------------------------------------------------------------

/// 正式的 [`ApiProviderFactory`]：pots-llm 的 OpenAI／Anthropic 相容 provider，共用一個
/// HTTP client。ChatGPT 的模型清單經由 [`AutoModel`]。
pub struct HttpApiProviders {
    http: pots_llm::reqwest::Client,
    chatgpt: Option<Arc<AutoModel>>,
}

impl HttpApiProviders {
    pub fn new(http: pots_llm::reqwest::Client, chatgpt: Option<Arc<AutoModel>>) -> Self {
        Self { http, chatgpt }
    }
}

fn model_options(models: Vec<pots_llm::responses::ModelInfo>) -> Vec<ModelOption> {
    models
        .into_iter()
        .map(|m| ModelOption {
            id: m.slug,
            display_name: m.display_name,
        })
        .collect()
}

#[async_trait::async_trait]
impl ApiProviderFactory for HttpApiProviders {
    fn build(
        &self,
        kind: ProviderKind,
        base_url: &Url,
        api_key: &str,
    ) -> Result<Arc<dyn ModelProvider>, String> {
        let (base, key, http) = (base_url.clone(), api_key.to_string(), self.http.clone());
        match kind {
            ProviderKind::OpenAiCompatible => {
                Ok(Arc::new(OpenAiCompatProvider::new(base, key, http)))
            }
            ProviderKind::AnthropicCompatible => {
                Ok(Arc::new(AnthropicProvider::new(base, key, http)))
            }
            ProviderKind::ChatGpt => Err("ChatGPT 方案不是 API provider".into()),
        }
    }

    async fn list_models(
        &self,
        kind: ProviderKind,
        base_url: &Url,
        api_key: &str,
    ) -> Result<Vec<ModelOption>, LlmError> {
        let (base, key, http) = (base_url.clone(), api_key.to_string(), self.http.clone());
        let models = match kind {
            ProviderKind::OpenAiCompatible => {
                OpenAiCompatProvider::new(base, key, http)
                    .list_models()
                    .await?
            }
            ProviderKind::AnthropicCompatible => {
                AnthropicProvider::new(base, key, http)
                    .list_models()
                    .await?
            }
            ProviderKind::ChatGpt => return self.chatgpt_models().await,
        };
        Ok(model_options(models))
    }

    async fn chatgpt_models(&self) -> Result<Vec<ModelOption>, LlmError> {
        match &self.chatgpt {
            Some(auto) => Ok(model_options(auto.list_models().await?)),
            None => Err(LlmError::Unsupported("這個組裝沒有 ChatGPT 方案".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_usage_without_scope_is_an_error() {
        let view = auth_view(&AuthStatus::SignedIn {
            email: Some("a@example.com".into()),
            plan_usage: false,
        });
        assert!(
            matches!(view, ModelAuthView::Error { message } if message.contains("a@example.com"))
        );
        assert_eq!(
            auth_view(&AuthStatus::SignedIn {
                email: None,
                plan_usage: true
            }),
            ModelAuthView::SignedIn { account: None }
        );
    }
}
