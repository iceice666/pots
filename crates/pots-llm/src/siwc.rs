//! Sign in with ChatGPT（SIWC）開源 token-sharing 流程。
//!
//! 規格摘要與來源見 `proposal/agent-runtime.md` 與 `target/tmp/siwc-research.md`
//! （研究筆記，之後整理進提案）。重點：
//!
//! - 瀏覽器登入，loopback callback 固定 `http://127.0.0.1:<port>/auth/callback`。
//! - 第一次用 `client_id=dynamic_agent_client` 加 `agent_name_hint`、`ext_agent_host_id`
//!   註冊，callback 帶回 issued `client_id`，之後一直沿用。
//! - Host ID 每個 runtime 安裝實例一個（`urn:uuid:<v4>`），跟憑證分開保存。
//! - 每份授權只有一個 refresh owner；refresh token 會輪替，要原子更新。
//!
//! [`SiwcAuth`] 是一份授權在行程內的唯一管理者：refresh 由一把 async mutex 序列化，
//! 同時有多個請求發現 token 過期時只會 refresh 一次。

mod login;
mod oidc;
mod store;

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;
use url::Url;

pub use self::login::redact_authorize_url;
use self::login::{LoginHints, LoginTransaction};
use self::oidc::{Discovery, OidcClient, TokenError, TokenResponse, issuer_string};
pub use self::store::{CREDENTIALS_FILE, FileCredentialStore, load_or_create_host_id};
use crate::provider::LlmError;

/// 首次註冊用的 client ID；不是要保存的 ID。
pub const DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";
/// 授權請求的 scope。
pub const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// ChatGPT plan 推論需要實際授予的 scope。
pub const PLAN_USAGE_SCOPE: &str = "chatgpt.tokens.use.direct";
/// loopback callback 的路徑；只有 port 可以變。
pub const CALLBACK_PATH: &str = "/auth/callback";
/// access token 剩不到這麼久就先 refresh。
const REFRESH_MARGIN_SECS: i64 = 120;
/// 登出時撤銷遇到暫時錯誤的重試次數。
const REVOKE_ATTEMPTS: u32 = 3;

#[derive(Clone, Debug)]
pub struct SiwcConfig {
    /// 預設 `https://auth.openai.com`；從 `/.well-known/openid-configuration` 取端點。
    pub issuer: Url,
    /// Responses API 的 resource，也是 token 請求的 `resource`。預設 `https://api.openai.com/v1`。
    pub resource: Url,
    /// `agent_name_hint`，固定 `Pots`。
    pub app_name: String,
    /// 這個 runtime 安裝實例的 host ID（見 [`load_or_create_host_id`]）。
    pub host_id: String,
    /// 偏好的 callback port；`None` 或被占用時用隨機 port。
    pub callback_port: Option<u16>,
    /// 等使用者在瀏覽器完成登入的上限。
    pub login_timeout: Duration,
}

impl SiwcConfig {
    /// 正式環境的預設值。
    pub fn new(host_id: impl Into<String>) -> Self {
        Self {
            issuer: Url::parse("https://auth.openai.com").expect("固定的 issuer URL"),
            resource: Url::parse("https://api.openai.com/v1").expect("固定的 resource URL"),
            app_name: "Pots".into(),
            host_id: host_id.into(),
            callback_port: None,
            login_timeout: Duration::from_secs(10 * 60),
        }
    }

    fn resource_str(&self) -> &str {
        self.resource.as_str()
    }
}

/// 登入狀態。不含 access／refresh token，可以送給 UI。
///
/// [`AuthStatus::Pending`] 的 URL 在回流登入時含 `id_token_hint`（使用者身分資訊），
/// 只用來開瀏覽器，不要寫進日誌。`Debug` 只印遮蔽過的 URL（見 [`redact_authorize_url`]）。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthStatus {
    SignedOut,
    /// 等使用者在瀏覽器完成登入。
    Pending {
        url: String,
    },
    SignedIn {
        email: Option<String>,
        /// 是否授予 `chatgpt.tokens.use.direct`；沒有就不能推論。
        plan_usage: bool,
    },
    /// 授權失效或設定錯誤，需要使用者處理。
    Error {
        message: String,
    },
}

impl std::fmt::Debug for AuthStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SignedOut => f.write_str("SignedOut"),
            Self::Pending { url } => f
                .debug_struct("Pending")
                .field("url", &redact_authorize_url(url))
                .finish(),
            Self::SignedIn { email, plan_usage } => f
                .debug_struct("SignedIn")
                .field("email", email)
                .field("plan_usage", plan_usage)
                .finish(),
            Self::Error { message } => f.debug_struct("Error").field("message", message).finish(),
        }
    }
}

/// 目前有效的 access token。`Debug` 不印出內容。
#[derive(Clone, PartialEq, Eq)]
pub struct AccessToken(pub String);

impl std::fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AccessToken(..)")
    }
}

/// 給 provider 取 token 用。
#[async_trait::async_trait]
pub trait TokenSource: Send + Sync {
    /// 取得有效的 access token；快過期時先 refresh（同一份授權同時只有一個 refresh）。
    async fn access_token(&self) -> Result<AccessToken, LlmError>;

    /// 推論回 401 時呼叫：若 `rejected` 仍是目前的 token，強制 refresh 一次並回傳新的。
    async fn token_rejected(&self, rejected: &AccessToken) -> Result<AccessToken, LlmError>;
}

/// 受保護的憑證儲存。實作要原子寫入，檔案權限 `0600`。
#[async_trait::async_trait]
pub trait CredentialStore: Send + Sync {
    async fn load(&self) -> Result<Option<StoredCredentials>, LlmError>;
    async fn save(&self, credentials: &StoredCredentials) -> Result<(), LlmError>;
}

/// 持久化的授權。`Debug` 不印出 token。
#[derive(Clone, Serialize, Deserialize)]
pub struct StoredCredentials {
    pub issuer: String,
    /// issued client ID（`oaiapp_...`）；登出後仍保留，下次登入沿用。
    /// 空字串代表沒有可沿用的註冊，下次登入用 [`DYNAMIC_CLIENT_ID`] 重新註冊。
    pub client_id: String,
    /// 驗證過的 ID token `sub`，帳號身分以它為準（不是 email）。
    pub subject: Option<String>,
    pub email: Option<String>,
    /// token 回應實際授予的 scope。
    pub scopes: Vec<String>,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    /// 保留給回流登入的 `id_token_hint`；登出時清掉。
    pub id_token: Option<String>,
    /// access token 到期時間（Unix 秒）。
    pub expires_at: Option<i64>,
    /// token 回應的 `earliest_refresh_at`；格式未經文件確認，原樣保存、目前不使用。
    #[serde(default)]
    pub earliest_refresh_at: Option<serde_json::Value>,
}

impl std::fmt::Debug for StoredCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredCredentials")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("subject", &self.subject)
            .field("scopes", &self.scopes)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl StoredCredentials {
    fn plan_usage(&self) -> bool {
        self.scopes.iter().any(|scope| scope == PLAN_USAGE_SCOPE)
    }

    fn is_signed_in(&self) -> bool {
        self.refresh_token.is_some() || self.access_token.is_some()
    }

    /// 還沒進入 refresh 緩衝期的 access token。沒有到期時間時視為有效，等 401 再處理。
    fn fresh_access_token(&self, now: i64) -> Option<AccessToken> {
        let token = self.access_token.as_ref()?;
        let fresh = self
            .expires_at
            .is_none_or(|expires_at| expires_at - now > REFRESH_MARGIN_SECS);
        fresh.then(|| AccessToken(token.clone()))
    }

    /// 套用 token 回應；沒給的欄位保留舊值（例如 refresh 沒回新的 refresh token）。
    fn apply(&mut self, tokens: TokenResponse, now: i64) {
        self.access_token = Some(tokens.access_token);
        if let Some(refresh_token) = tokens.refresh_token {
            self.refresh_token = Some(refresh_token);
        }
        if let Some(scope) = tokens.scope {
            self.scopes = scope.split_whitespace().map(str::to_owned).collect();
        }
        self.expires_at = tokens.expires_in.map(|seconds| now + seconds);
        if tokens.earliest_refresh_at.is_some() {
            self.earliest_refresh_at = tokens.earliest_refresh_at;
        }
    }

    /// 清掉不能再用的 token，保留 issued client ID 與帳號對應。
    fn clear_tokens(&mut self) {
        self.access_token = None;
        self.refresh_token = None;
        self.expires_at = None;
    }

    /// 丟掉 issued client ID 與帳號對應（保留 issuer），下次登入重新註冊。
    fn forget_registration(&mut self) {
        self.clear_tokens();
        self.client_id.clear();
        self.subject = None;
        self.email = None;
        self.scopes.clear();
        self.id_token = None;
        self.earliest_refresh_at = None;
    }
}

/// 登出的結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogoutOutcome {
    /// 已向 OpenAI 撤銷 refresh token。
    Revoked,
    /// 本機沒有可撤銷的 refresh token。
    NothingToRevoke,
    /// 本機已清除，但遠端撤銷沒有確認；要告訴使用者可到 ChatGPT 設定中斷開 app。
    LocalOnly { reason: String },
}

/// 進行中的登入流程。
struct ActiveLogin {
    id: u64,
    url: String,
    cancel: CancellationToken,
}

/// 登入流程的結束方式。
enum LoginEnd {
    SignedIn,
    Cancelled,
    Failed(String),
}

/// 一份 SIWC 授權的唯一管理者：登入、refresh、登出都經過它。
pub struct SiwcAuth {
    config: SiwcConfig,
    store: Arc<dyn CredentialStore>,
    oidc: OidcClient,
    status: watch::Sender<AuthStatus>,
    /// 憑證快取。refresh 期間一直持有這把鎖，所以同時只會有一個 refresh。
    session: Mutex<Option<StoredCredentials>>,
    /// 同時只能有一個登入流程；設定期間持有，避免兩個 `start_login` 同時開 listener。
    login: Mutex<Option<ActiveLogin>>,
    next_login_id: std::sync::atomic::AtomicU64,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn status_of(credentials: Option<&StoredCredentials>) -> AuthStatus {
    match credentials {
        Some(credentials) if credentials.is_signed_in() => AuthStatus::SignedIn {
            email: credentials.email.clone(),
            plan_usage: credentials.plan_usage(),
        },
        _ => AuthStatus::SignedOut,
    }
}

impl SiwcAuth {
    /// 從 `store` 載入既有授權。issuer 不同的憑證不使用（也不刪除）。
    pub async fn open(
        config: SiwcConfig,
        store: Arc<dyn CredentialStore>,
        http: reqwest::Client,
    ) -> Result<Arc<Self>, LlmError> {
        let issuer = issuer_string(&config.issuer);
        let credentials = store.load().await?.filter(|credentials| {
            let same = credentials.issuer.trim_end_matches('/') == issuer;
            if !same {
                tracing::warn!(stored = %credentials.issuer, "憑證屬於其他 issuer，忽略");
            }
            same
        });
        let (status, _) = watch::channel(status_of(credentials.as_ref()));
        Ok(Arc::new(Self {
            oidc: OidcClient::new(config.issuer.clone(), http),
            config,
            store,
            status,
            session: Mutex::new(credentials),
            login: Mutex::new(None),
            next_login_id: std::sync::atomic::AtomicU64::new(0),
        }))
    }

    pub fn status(&self) -> watch::Receiver<AuthStatus> {
        self.status.subscribe()
    }

    fn set_status(&self, status: AuthStatus) {
        self.status.send_if_modified(|current| {
            let changed = *current != status;
            *current = status;
            changed
        });
    }

    /// refresh 造成的狀態變化；登入流程進行中時保留 [`AuthStatus::Pending`]，URL 才不會不見。
    fn set_status_unless_pending(&self, status: AuthStatus) {
        self.status.send_if_modified(|current| {
            if matches!(current, AuthStatus::Pending { .. }) || *current == status {
                return false;
            }
            *current = status;
            true
        });
    }

    /// 首次註冊拿到 issued client ID 但換 token 失敗時先記住它，下次登入沿用、不重新註冊。
    async fn remember_client_id(&self, issuer: &str, client_id: &str) {
        let mut session = self.session.lock().await;
        if session
            .as_ref()
            .is_some_and(|credentials| !credentials.client_id.is_empty())
        {
            return;
        }
        let credentials = StoredCredentials {
            issuer: issuer.to_owned(),
            client_id: client_id.to_owned(),
            subject: None,
            email: None,
            scopes: Vec::new(),
            access_token: None,
            refresh_token: None,
            id_token: None,
            expires_at: None,
            earliest_refresh_at: None,
        };
        self.persist(&credentials).await;
        *session = Some(credentials);
    }

    /// 存檔失敗只記錄：記憶體裡已經是輪替後的 token，不能退回舊的。
    async fn persist(&self, credentials: &StoredCredentials) {
        if let Err(error) = self.store.save(credentials).await {
            tracing::error!(%error, "無法保存 ChatGPT 憑證；重啟後可能需要重新登入");
        }
    }

    /// 開始登入：先綁好 loopback listener，再把授權 URL 放進 [`AuthStatus::Pending`]。
    /// 登入完成、失敗或取消後狀態會更新；同時只能有一個登入流程，已經在進行時回傳同一個 URL。
    /// 呼叫端負責用系統瀏覽器開啟回傳的 URL。
    pub async fn start_login(self: &Arc<Self>) -> Result<String, LlmError> {
        let mut active = self.login.lock().await;
        if let Some(login) = active.as_ref().filter(|login| !login.cancel.is_cancelled()) {
            return Ok(login.url.clone());
        }
        let discovery = self.oidc.discovery().await?;
        let listener = login::bind_loopback(self.config.callback_port).await?;
        let port = listener
            .local_addr()
            .map_err(|e| LlmError::Other(format!("無法取得 callback port：{e}")))?
            .port();

        let previous = self.session.lock().await.clone();
        let issued = previous
            .as_ref()
            .filter(|credentials| !credentials.client_id.is_empty());
        let transaction = LoginTransaction {
            state: login::random_token()?,
            nonce: login::random_token()?,
            verifier: login::random_token()?,
            redirect_uri: format!("http://127.0.0.1:{port}{CALLBACK_PATH}"),
            client_id: issued.map_or_else(|| DYNAMIC_CLIENT_ID.to_owned(), |c| c.client_id.clone()),
            previous_subject: issued.and_then(|c| c.subject.clone()),
        };
        let hints = LoginHints {
            id_token: issued.and_then(|c| c.id_token.as_deref()),
            email: issued.and_then(|c| c.email.as_deref()),
        };
        let url = login::authorize_url(
            &discovery.authorization_endpoint,
            &transaction,
            &self.config.app_name,
            &self.config.host_id,
            self.config.resource_str(),
            &hints,
        )
        .to_string();

        let id = self
            .next_login_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let cancel = CancellationToken::new();
        *active = Some(ActiveLogin {
            id,
            url: url.clone(),
            cancel: cancel.clone(),
        });
        self.set_status(AuthStatus::Pending { url: url.clone() });
        let requests = login::serve_callback(listener, cancel.clone());
        let auth = Arc::clone(self);
        tokio::spawn(async move {
            auth.run_login(id, discovery, transaction, requests, cancel)
                .await;
        });
        Ok(url)
    }

    /// 取消進行中的登入；狀態回到登入前的樣子。
    pub async fn cancel_login(&self) {
        if let Some(login) = self.login.lock().await.as_ref() {
            login.cancel.cancel();
        }
    }

    /// 等 callback、逾時或取消，然後收尾。
    async fn run_login(
        &self,
        id: u64,
        discovery: Discovery,
        transaction: LoginTransaction,
        mut requests: tokio::sync::mpsc::Receiver<login::CallbackRequest>,
        cancel: CancellationToken,
    ) {
        let deadline = tokio::time::sleep(self.config.login_timeout);
        tokio::pin!(deadline);
        let end = loop {
            let request = tokio::select! {
                () = cancel.cancelled() => break LoginEnd::Cancelled,
                () = &mut deadline => break LoginEnd::Failed("登入逾時，請重新登入".into()),
                request = requests.recv() => match request {
                    Some(request) => request,
                    None => break LoginEnd::Failed("loopback callback 意外關閉".into()),
                },
            };
            // state 不符：可能是別的分頁或惡意請求，不中斷正在等的流程。
            if request.params.get("state") != Some(&transaction.state) {
                let _ = request.reply.send((
                    StatusCode::BAD_REQUEST,
                    "登入請求不符（state 不一致），請回到 Pots 重新登入。".into(),
                ));
                continue;
            }
            let result = self
                .complete_login(&discovery, &transaction, &request.params, &cancel)
                .await;
            let reply = match &result {
                Ok(()) => (
                    StatusCode::OK,
                    "登入完成，可以關閉這個分頁回到 Pots。".to_owned(),
                ),
                Err(_) => (
                    StatusCode::BAD_REQUEST,
                    "登入沒有完成，請回到 Pots 查看原因。".to_owned(),
                ),
            };
            let _ = request.reply.send(reply);
            break match result {
                Ok(()) => LoginEnd::SignedIn,
                // 換 token 時被取消或登出：complete_login 已經丟掉（並撤銷）新的 token。
                Err(_) if cancel.is_cancelled() => LoginEnd::Cancelled,
                Err(message) => LoginEnd::Failed(message),
            };
        };
        // 停掉 listener（已回覆的頁面會先送完）。
        cancel.cancel();

        // 整段持有 login 鎖：跟 `start_login` 設定 Pending 的順序一致。
        let mut active = self.login.lock().await;
        let still_current = active.as_ref().is_some_and(|login| login.id == id);
        if !still_current {
            // 取消後已經開了新的登入：狀態屬於新的流程，不能蓋掉它的 Pending。
            if let LoginEnd::Failed(message) = end {
                tracing::info!(%message, "舊的 ChatGPT 登入流程結束");
            }
            return;
        }
        *active = None;
        match end {
            LoginEnd::SignedIn => {}
            LoginEnd::Cancelled => {
                let status = status_of(self.session.lock().await.as_ref());
                self.set_status(status);
            }
            LoginEnd::Failed(message) => {
                tracing::warn!(%message, "ChatGPT 登入失敗");
                self.set_status(AuthStatus::Error { message });
            }
        }
    }

    /// 處理 state 已驗證的 callback：換 token、驗 ID token、保存。錯誤訊息給使用者看。
    async fn complete_login(
        &self,
        discovery: &Discovery,
        transaction: &LoginTransaction,
        params: &std::collections::HashMap<String, String>,
        cancel: &CancellationToken,
    ) -> Result<(), String> {
        if let Some(error) = params.get("error") {
            return Err(if error == "access_denied" {
                "已在瀏覽器拒絕或取消授權".into()
            } else {
                let description = params.get("error_description").map_or("", String::as_str);
                format!("授權失敗：{error} {description}").trim().to_owned()
            });
        }
        let code = params
            .get("code")
            .filter(|code| !code.is_empty())
            .ok_or("callback 缺少授權碼")?;
        let returned = params.get("client_id").filter(|id| !id.is_empty());
        let client_id = if transaction.is_registration() {
            match returned {
                Some(id) if id != DYNAMIC_CLIENT_ID => id.clone(),
                _ => return Err("註冊沒有完成：callback 沒有帶回 issued client_id".into()),
            }
        } else {
            if returned.is_some_and(|id| *id != transaction.client_id) {
                return Err("callback 的 client_id 與登入請求不符".into());
            }
            transaction.client_id.clone()
        };

        let tokens = self
            .oidc
            .token(
                discovery,
                &[
                    ("grant_type", "authorization_code"),
                    ("client_id", &client_id),
                    ("code", code),
                    ("code_verifier", &transaction.verifier),
                    ("redirect_uri", &transaction.redirect_uri),
                    ("resource", self.config.resource_str()),
                ],
            )
            .await;
        let tokens = match tokens {
            Ok(tokens) => tokens,
            Err(error) => {
                if transaction.is_registration() {
                    self.remember_client_id(&discovery.issuer, &client_id).await;
                }
                return Err(match error {
                    TokenError::Terminal(_) => "授權碼已失效，請重新登入".to_owned(),
                    other => other.message().to_owned(),
                });
            }
        };
        let issued_refresh = tokens.refresh_token.clone();
        let result = self
            .accept_tokens(discovery, transaction, &client_id, tokens, cancel)
            .await;
        if result.is_err()
            && let Some(refresh_token) = issued_refresh
        {
            // 新發的 token 沒有被採用：撤銷它，不留下沒人管的授權（盡力而為）。
            self.revoke_discarded(&client_id, &refresh_token).await;
        }
        result
    }

    /// 驗證換來的 token 並保存。回錯誤時這組 token 沒有被採用，由呼叫端撤銷。
    async fn accept_tokens(
        &self,
        discovery: &Discovery,
        transaction: &LoginTransaction,
        client_id: &str,
        tokens: TokenResponse,
        cancel: &CancellationToken,
    ) -> Result<(), String> {
        let id_token = tokens.id_token.clone().ok_or("token 回應沒有 ID token")?;
        let claims = self
            .oidc
            .verify_id_token(discovery, &id_token, client_id, Some(&transaction.nonce))
            .await
            .map_err(|error| error.to_string())?;

        let mut session = self.session.lock().await;
        // 換 token 的期間可能已經取消或登出（`logout` 會先取消登入、再拿 session 鎖），
        // 這時不能再存下新的 token。
        if cancel.is_cancelled() {
            return Err("登入已取消".into());
        }
        if transaction
            .previous_subject
            .as_ref()
            .is_some_and(|previous| *previous != claims.sub)
        {
            if session
                .as_ref()
                .is_some_and(StoredCredentials::is_signed_in)
            {
                // 回流登入的保護：還有 token 時不能被別的帳號取代。
                return Err(
                    "登入的帳號與目前登入的帳號不同；請改用原帳號登入，或先登出再換帳號".into(),
                );
            }
            // 已登出、換了帳號：issued client ID 綁定原本選的使用者／workspace，不能給別的帳號沿用。
            // 這次先拒絕，並丟掉舊的註冊，下次「開始登入」會用 dynamic_agent_client 重新註冊。
            if let Some(credentials) = session
                .as_mut()
                .filter(|credentials| credentials.client_id == client_id)
            {
                credentials.forget_registration();
                self.persist(credentials).await;
            }
            return Err("選擇的帳號與這個 Pots 原本註冊的帳號不同。已清除舊的註冊，\
                        請重新開始登入，以這個帳號重新註冊"
                .into());
        }

        let now = now_secs();
        let mut credentials = StoredCredentials {
            issuer: discovery.issuer.clone(),
            client_id: client_id.to_owned(),
            subject: Some(claims.sub),
            email: claims.email,
            scopes: Vec::new(),
            access_token: None,
            refresh_token: None,
            id_token: Some(id_token),
            expires_at: None,
            earliest_refresh_at: None,
        };
        credentials.apply(tokens, now);
        if !credentials.plan_usage() {
            tracing::warn!("登入成功，但沒有授予 {PLAN_USAGE_SCOPE}；無法使用 ChatGPT plan 推論");
        }
        self.store
            .save(&credentials)
            .await
            .map_err(|error| format!("無法保存憑證：{error}"))?;
        self.set_status(status_of(Some(&credentials)));
        *session = Some(credentials);
        Ok(())
    }

    /// 撤銷 refresh token 並清掉本機 token；保留 issued client ID 與 host ID。
    /// 遠端撤銷失敗時仍清除本機 token，並以 [`LogoutOutcome::LocalOnly`] 告知。
    pub async fn logout(&self) -> Result<LogoutOutcome, LlmError> {
        self.cancel_login().await;
        let mut session = self.session.lock().await;
        let Some(credentials) = session.as_mut() else {
            self.set_status(AuthStatus::SignedOut);
            return Ok(LogoutOutcome::NothingToRevoke);
        };
        let outcome = match credentials.refresh_token.clone() {
            None => LogoutOutcome::NothingToRevoke,
            Some(refresh_token) => self.revoke(&credentials.client_id, &refresh_token).await,
        };
        credentials.clear_tokens();
        credentials.id_token = None;
        let result = self.store.save(credentials).await;
        self.set_status(AuthStatus::SignedOut);
        result.map(|()| outcome)
    }

    /// 撤銷沒有被採用的新 refresh token；失敗只記錄。
    async fn revoke_discarded(&self, client_id: &str, refresh_token: &str) {
        match self.revoke(client_id, refresh_token).await {
            LogoutOutcome::LocalOnly { reason } => {
                tracing::warn!(%reason, "無法撤銷沒有採用的 refresh token");
            }
            _ => tracing::info!("已撤銷沒有採用的 refresh token"),
        }
    }

    async fn revoke(&self, client_id: &str, refresh_token: &str) -> LogoutOutcome {
        let discovery = match self.oidc.discovery().await {
            Ok(discovery) => discovery,
            Err(error) => {
                return LogoutOutcome::LocalOnly {
                    reason: error.to_string(),
                };
            }
        };
        let mut delay = Duration::from_millis(500);
        for attempt in 1..=REVOKE_ATTEMPTS {
            match self.oidc.revoke(&discovery, client_id, refresh_token).await {
                Ok(()) => return LogoutOutcome::Revoked,
                Err(TokenError::Transient(reason)) if attempt < REVOKE_ATTEMPTS => {
                    tracing::info!(%reason, attempt, "撤銷暫時失敗，稍後重試");
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
                Err(error) => {
                    return LogoutOutcome::LocalOnly {
                        reason: error.message().to_owned(),
                    };
                }
            }
        }
        LogoutOutcome::LocalOnly {
            reason: "撤銷沒有成功".into(),
        }
    }

    /// 在持有 session 鎖的情況下 refresh。成功時原子保存輪替後的 token。
    async fn refresh_locked(
        &self,
        session: &mut Option<StoredCredentials>,
    ) -> Result<AccessToken, LlmError> {
        let credentials = session.as_mut().ok_or(LlmError::NeedsLogin)?;
        let refresh_token = credentials
            .refresh_token
            .clone()
            .ok_or(LlmError::NeedsLogin)?;
        let discovery = self.oidc.discovery().await?;
        let client_id = credentials.client_id.clone();
        let result = self
            .oidc
            .token(
                &discovery,
                &[
                    ("grant_type", "refresh_token"),
                    ("client_id", &client_id),
                    ("refresh_token", &refresh_token),
                    ("resource", self.config.resource_str()),
                ],
            )
            .await;
        let mut tokens = match result {
            Ok(tokens) => tokens,
            Err(TokenError::Terminal(message)) => {
                tracing::warn!(%message, "refresh token 已失效，需要重新登入");
                credentials.clear_tokens();
                self.persist(credentials).await;
                self.set_status_unless_pending(AuthStatus::Error {
                    message: "ChatGPT 授權已失效，請重新登入".into(),
                });
                return Err(LlmError::NeedsLogin);
            }
            Err(TokenError::Client(message)) => {
                self.set_status_unless_pending(AuthStatus::Error {
                    message: format!("ChatGPT client 設定錯誤：{message}"),
                });
                return Err(LlmError::Other(message));
            }
            Err(TokenError::Transient(message)) => return Err(LlmError::Unavailable(message)),
            Err(TokenError::Other(message)) => return Err(LlmError::Other(message)),
        };

        // refresh 回來的 ID token 驗證失敗時不採用，但 token 照樣保存（舊的 refresh token 已經作廢）。
        if let Some(id_token) = tokens.id_token.take() {
            match self
                .oidc
                .verify_id_token(&discovery, &id_token, &client_id, None)
                .await
            {
                Ok(claims)
                    if credentials
                        .subject
                        .as_ref()
                        .is_none_or(|s| *s == claims.sub) =>
                {
                    credentials.id_token = Some(id_token);
                    if claims.email.is_some() {
                        credentials.email = claims.email;
                    }
                }
                Ok(_) => tracing::warn!("refresh 回來的 ID token 帳號不同，忽略"),
                Err(error) => tracing::warn!(%error, "refresh 回來的 ID token 無法驗證，忽略"),
            }
        }
        credentials.apply(tokens, now_secs());
        self.persist(credentials).await;
        self.set_status_unless_pending(status_of(Some(credentials)));
        credentials
            .access_token
            .clone()
            .map(AccessToken)
            .ok_or(LlmError::NeedsLogin)
    }
}

#[async_trait::async_trait]
impl TokenSource for SiwcAuth {
    async fn access_token(&self) -> Result<AccessToken, LlmError> {
        let mut session = self.session.lock().await;
        let credentials = session.as_ref().ok_or(LlmError::NeedsLogin)?;
        if !credentials.is_signed_in() {
            return Err(LlmError::NeedsLogin);
        }
        if !credentials.plan_usage() {
            return Err(LlmError::NotEntitled(format!(
                "登入時沒有授予 {PLAN_USAGE_SCOPE}"
            )));
        }
        if let Some(token) = credentials.fresh_access_token(now_secs()) {
            return Ok(token);
        }
        self.refresh_locked(&mut session).await
    }

    async fn token_rejected(&self, rejected: &AccessToken) -> Result<AccessToken, LlmError> {
        let mut session = self.session.lock().await;
        let current = session
            .as_ref()
            .and_then(|credentials| credentials.fresh_access_token(now_secs()));
        // 別人已經換過 token：直接用新的。
        if let Some(current) = current.filter(|current| current != rejected) {
            return Ok(current);
        }
        self.refresh_locked(&mut session).await
    }
}
