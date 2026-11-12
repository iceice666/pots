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
use pots_llm::reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use pots_llm::responses::{DEFAULT_BASE_URL, ResponsesProvider};
use pots_llm::siwc::{
    AuthStatus, FileCredentialStore, SiwcAuth, SiwcConfig, load_or_create_host_id,
};
use pots_llm::{EventStream, LlmError, ModelProvider, ModelRequest};
use pots_proto::{ModelAuthView, ModelOption, ProviderHeader, ProviderKind};
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::preset_provider::PresetProvider;
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
    let result = runtime.block_on(run(Config::desktop(data_dir), model));
    shutdown_tracing();
    result
}

/// 日誌寫到 stderr（桌面 app 把它導到資料目錄的 `server.log`）。`RUST_LOG` 可調整等級。
///
/// 設定 `OTEL_EXPORTER_OTLP_ENDPOINT`（或 `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`）時，
/// 另外把 span 以 OTLP/HTTP 送到收集器；服務名稱預設 `pots`，可用 `OTEL_SERVICE_NAME`
/// 覆寫。沒有設定時只寫 stderr，和平常一樣。`shutdown_tracing` 會把佇列中的 span 送完。
pub fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    // 只初始化一次；重複呼叫（例如測試）不會換掉全域 subscriber。
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if ONCE.set(()).is_err() {
        return;
    }
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if !otlp_configured() {
        let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
        return;
    }
    match otlp_layer() {
        Ok(provider) => {
            use opentelemetry::trace::TracerProvider as _;
            use tracing_subscriber::layer::SubscriberExt as _;
            use tracing_subscriber::util::SubscriberInitExt as _;
            let tracer = provider.tracer("pots");
            let layer = tracing_opentelemetry::layer().with_tracer(tracer);
            let fmt = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
            let init = tracing_subscriber::registry()
                .with(filter)
                .with(fmt)
                .with(layer)
                .try_init();
            if init.is_err() {
                let _ = provider.shutdown();
                return;
            }
            *tracer_provider()
                .get_or_init(|| std::sync::Mutex::new(None))
                .lock()
                .unwrap() = Some(provider);
        }
        Err(e) => {
            eprintln!("OpenTelemetry 初始化失敗，只寫 stderr：{e}");
            let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
        }
    }
}

/// 把尚未送出的 span 送完。程式結束前呼叫；沒有啟用 OpenTelemetry 時什麼都不做。
pub fn shutdown_tracing() {
    if let Some(provider) = tracer_provider().get()
        && let Some(provider) = provider.lock().unwrap().take()
    {
        let _ = provider.shutdown();
    }
}

fn tracer_provider() -> &'static std::sync::OnceLock<
    std::sync::Mutex<Option<opentelemetry_sdk::trace::SdkTracerProvider>>,
> {
    static PROVIDER: std::sync::OnceLock<
        std::sync::Mutex<Option<opentelemetry_sdk::trace::SdkTracerProvider>>,
    > = std::sync::OnceLock::new();
    &PROVIDER
}

fn otlp_configured() -> bool {
    [
        "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
        "OTEL_EXPORTER_OTLP_ENDPOINT",
    ]
    .iter()
    .any(|key| std::env::var(key).is_ok_and(|v| !v.trim().is_empty()))
}

/// 建立 OTLP/HTTP 的 tracer provider；端點、標頭與時限都照 OpenTelemetry 的環境變數。
fn otlp_layer() -> Result<opentelemetry_sdk::trace::SdkTracerProvider, String> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .build()
        .map_err(|e| e.to_string())?;
    let service = std::env::var("OTEL_SERVICE_NAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "pots".into());
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            opentelemetry_sdk::Resource::builder()
                .with_service_name(service)
                .build(),
        )
        .build();
    Ok(provider)
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

    /// 圖片附件的內容來源：歷史裡的 [`pots_llm::Item::Image`] 只有附件參照，送出前由它讀出
    /// base64（見 [`pots_llm::image`]）。附件儲存接上去之後由組裝端呼叫。
    pub fn with_images(mut self, loader: Arc<dyn pots_llm::image::ImageLoader>) -> Self {
        self.inner = self.inner.with_images(loader);
        self
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
    /// 圖片附件的內容來源；見 [`pots_llm::image`]。
    images: Option<Arc<dyn pots_llm::image::ImageLoader>>,
}

impl HttpApiProviders {
    pub fn new(http: pots_llm::reqwest::Client, chatgpt: Option<Arc<AutoModel>>) -> Self {
        Self {
            http,
            chatgpt,
            images: None,
        }
    }

    /// 圖片附件的內容來源。沒有設定時，歷史裡出現 [`pots_llm::Item::Image`] 會回錯誤，
    /// 而不是默默送一個少了圖的請求。
    pub fn with_images(mut self, loader: Arc<dyn pots_llm::image::ImageLoader>) -> Self {
        self.images = Some(loader);
        self
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

/// 把協定裡的 header 轉成 reqwest 的 [`HeaderMap`]；值標成 sensitive，避免任何 Debug 輸出。
fn header_map(headers: &[ProviderHeader]) -> Result<HeaderMap, String> {
    let mut map = HeaderMap::new();
    for header in headers {
        let name = HeaderName::from_bytes(header.name.as_bytes())
            .map_err(|_| format!("header 名稱「{}」含有無效字元", header.name))?;
        let mut value = HeaderValue::from_str(&header.value)
            .map_err(|_| format!("header「{}」的值含有無效字元", header.name))?;
        value.set_sensitive(true);
        map.insert(name, value);
    }
    Ok(map)
}

#[async_trait::async_trait]
impl ApiProviderFactory for HttpApiProviders {
    fn build(
        &self,
        kind: ProviderKind,
        base_url: &Url,
        api_key: &str,
        headers: &[ProviderHeader],
    ) -> Result<Arc<dyn ModelProvider>, String> {
        let (base, key, http) = (base_url.clone(), api_key.to_string(), self.http.clone());
        let headers = header_map(headers)?;
        if let Some(preset) = kind.preset() {
            return Ok(Arc::new(PresetProvider::new(
                preset,
                base,
                key,
                http,
                headers,
                self.images.clone(),
            )));
        }
        let images = self.images.clone();
        match kind {
            ProviderKind::OpenAiCompatible => {
                let provider = OpenAiCompatProvider::new(base, key, http).with_headers(headers);
                Ok(Arc::new(match images {
                    Some(loader) => provider.with_images(loader),
                    None => provider,
                }))
            }
            ProviderKind::OpenAiResponses => {
                let provider =
                    ResponsesProvider::with_api_key(base, key, http).with_headers(headers);
                Ok(Arc::new(match images {
                    Some(loader) => provider.with_images(loader),
                    None => provider,
                }))
            }
            ProviderKind::AnthropicCompatible => {
                let provider = AnthropicProvider::new(base, key, http).with_headers(headers);
                Ok(Arc::new(match images {
                    Some(loader) => provider.with_images(loader),
                    None => provider,
                }))
            }
            ProviderKind::ChatGpt => Err("ChatGPT 方案不是 API provider".into()),
            ProviderKind::OpenCodeZen | ProviderKind::OpenCodeGo => {
                unreachable!("preset 在上面的分支處理")
            }
        }
    }

    async fn list_models(
        &self,
        kind: ProviderKind,
        base_url: &Url,
        api_key: &str,
        headers: &[ProviderHeader],
    ) -> Result<Vec<ModelOption>, LlmError> {
        let (base, key, http) = (base_url.clone(), api_key.to_string(), self.http.clone());
        let headers = header_map(headers).map_err(LlmError::Other)?;
        if let Some(preset) = kind.preset() {
            return Ok(
                PresetProvider::new(preset, base, key, http, headers, self.images.clone())
                    .models()
                    .into_iter()
                    .map(|m| ModelOption {
                        id: m.slug,
                        display_name: m.display_name,
                    })
                    .collect(),
            );
        }
        let models = match kind {
            ProviderKind::OpenAiCompatible => {
                OpenAiCompatProvider::new(base, key, http)
                    .with_headers(headers)
                    .list_models()
                    .await?
            }
            ProviderKind::OpenAiResponses => {
                ResponsesProvider::with_api_key(base, key, http)
                    .with_headers(headers)
                    .list_models()
                    .await?
            }
            ProviderKind::AnthropicCompatible => {
                AnthropicProvider::new(base, key, http)
                    .with_headers(headers)
                    .list_models()
                    .await?
            }
            ProviderKind::ChatGpt => return self.chatgpt_models().await,
            ProviderKind::OpenCodeZen | ProviderKind::OpenCodeGo => {
                unreachable!("preset 在上面的分支處理")
            }
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

    #[test]
    fn api_providers_take_the_attachment_loader() {
        // 有 loader 時照樣建得起來（loader 只在送出含圖片的請求時才會被讀）。
        let dir = std::env::temp_dir().join(format!("pots-att-{}", uuid::Uuid::new_v4()));
        let loader: Arc<dyn pots_llm::image::ImageLoader> =
            Arc::new(pots_llm::image::DirectoryImages::new(&dir));
        let factory = HttpApiProviders::new(reqwest::Client::new(), None).with_images(loader);
        let base = Url::parse("https://api.openai.com/v1").unwrap();
        for kind in [
            ProviderKind::OpenAiCompatible,
            ProviderKind::OpenAiResponses,
            ProviderKind::AnthropicCompatible,
            ProviderKind::OpenCodeGo,
        ] {
            factory.build(kind, &base, "sk-test", &[]).unwrap();
        }
    }

    #[test]
    fn api_providers_build_by_kind() {
        let factory = HttpApiProviders::new(reqwest::Client::new(), None);
        let base = Url::parse("https://api.openai.com/v1").unwrap();
        assert_eq!(
            factory
                .build(ProviderKind::OpenAiResponses, &base, "sk-test", &[])
                .unwrap()
                .name(),
            "openai-responses"
        );
        assert_eq!(
            factory
                .build(ProviderKind::OpenAiCompatible, &base, "sk-test", &[])
                .unwrap()
                .name(),
            "openai-compatible"
        );
    }
}
