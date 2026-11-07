//! SIWC 登入、refresh、登出對假 OIDC server 的整合測試。

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{FakeOidc, ISSUED_CLIENT_ID, RefreshMode};
use pots_llm::LlmError;
use pots_llm::siwc::{
    AccessToken, AuthStatus, CredentialStore, DYNAMIC_CLIENT_ID, FileCredentialStore,
    LogoutOutcome, SiwcAuth, SiwcConfig, StoredCredentials, TokenSource, load_or_create_host_id,
};
use tempfile::TempDir;
use url::Url;

struct Harness {
    fake: FakeOidc,
    dir: TempDir,
    host_id: String,
}

impl Harness {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let host_id = load_or_create_host_id(&dir.path().join("host-id")).unwrap();
        Self {
            fake: FakeOidc::start().await,
            dir,
            host_id,
        }
    }

    fn store(&self) -> FileCredentialStore {
        FileCredentialStore::new(self.dir.path().join("auth"))
    }

    fn config(&self) -> SiwcConfig {
        SiwcConfig {
            issuer: self.fake.issuer.clone(),
            login_timeout: Duration::from_secs(10),
            ..SiwcConfig::new(self.host_id.clone())
        }
    }

    async fn open_with(&self, config: SiwcConfig) -> Arc<SiwcAuth> {
        SiwcAuth::open(config, Arc::new(self.store()), reqwest::Client::new())
            .await
            .unwrap()
    }

    async fn open(&self) -> Arc<SiwcAuth> {
        self.open_with(self.config()).await
    }

    async fn stored(&self) -> StoredCredentials {
        self.store().load().await.unwrap().unwrap()
    }

    /// 存一份「已登入」的憑證，refresh token `rt-seed` 在假 server 上有效。
    async fn seed(&self, expires_in: i64) {
        self.fake
            .add_refresh_token("rt-seed", ISSUED_CLIENT_ID, "user-1");
        let credentials = StoredCredentials {
            issuer: self.fake.issuer.as_str().trim_end_matches('/').into(),
            client_id: ISSUED_CLIENT_ID.into(),
            subject: Some("user-1".into()),
            email: Some("u@example.com".into()),
            scopes: common::FULL_SCOPE.split(' ').map(str::to_owned).collect(),
            access_token: Some("at-seed".into()),
            refresh_token: Some("rt-seed".into()),
            id_token: None,
            expires_at: Some(now() + expires_in),
            earliest_refresh_at: None,
        };
        self.store().save(&credentials).await.unwrap();
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn query(url: &str) -> HashMap<String, String> {
    Url::parse(url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect()
}

async fn wait_status(auth: &SiwcAuth, predicate: impl Fn(&AuthStatus) -> bool) -> AuthStatus {
    let mut status = auth.status();
    tokio::time::timeout(Duration::from_secs(10), status.wait_for(|s| predicate(s)))
        .await
        .expect("等待登入狀態逾時")
        .unwrap()
        .clone()
}

#[tokio::test]
async fn first_login_registers_then_restart_and_returning_login() {
    let h = Harness::new().await;
    let auth = h.open().await;
    assert_eq!(*auth.status().borrow(), AuthStatus::SignedOut);
    assert_eq!(auth.access_token().await.unwrap_err(), LlmError::NeedsLogin);

    let url = auth.start_login().await.unwrap();
    assert_eq!(
        *auth.status().borrow(),
        AuthStatus::Pending { url: url.clone() }
    );
    // 同時只有一個登入流程。
    assert_eq!(auth.start_login().await.unwrap(), url);

    let params = query(&url);
    assert!(url.starts_with(&format!("{}authorize?", h.fake.issuer)));
    assert_eq!(params["client_id"], DYNAMIC_CLIENT_ID);
    assert_eq!(params["agent_name_hint"], "Pots");
    assert_eq!(params["ext_agent_host_id"], h.host_id);
    assert!(h.host_id.starts_with("urn:uuid:"));
    assert_eq!(params["response_type"], "code");
    assert_eq!(params["scope"], pots_llm::siwc::SCOPES);
    assert_eq!(params["resource"], common::RESOURCE);
    assert_eq!(params["code_challenge_method"], "S256");
    let redirect = Url::parse(&params["redirect_uri"]).unwrap();
    assert_eq!(redirect.scheme(), "http");
    assert_eq!(redirect.host_str(), Some("127.0.0.1"));
    assert_eq!(redirect.path(), "/auth/callback");
    assert!(!params.contains_key("id_token_hint"));

    let http = reqwest::Client::new();
    // state 不符的請求被拒，但不會中斷正在等的流程。
    let mut forged = redirect.clone();
    forged
        .query_pairs_mut()
        .append_pair("code", "x")
        .append_pair("state", "forged");
    assert_eq!(http.get(forged).send().await.unwrap().status(), 400);

    let callback = h.fake.approve(&url, "user-1", "u@example.com");
    let response = http.get(callback).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let status = wait_status(&auth, |s| matches!(s, AuthStatus::SignedIn { .. })).await;
    assert_eq!(
        status,
        AuthStatus::SignedIn {
            email: Some("u@example.com".into()),
            plan_usage: true
        }
    );

    // 換 token 的請求：issued client、同一個 redirect_uri、PKCE（假 server 已驗證）。
    let exchange = h.fake.with(|inner| inner.token_requests[0].clone());
    assert_eq!(exchange["grant_type"], "authorization_code");
    assert_eq!(exchange["client_id"], ISSUED_CLIENT_ID);
    assert_eq!(exchange["redirect_uri"], params["redirect_uri"]);

    let stored = h.stored().await;
    assert_eq!(stored.client_id, ISSUED_CLIENT_ID);
    assert_eq!(stored.subject.as_deref(), Some("user-1"));
    assert!(stored.id_token.is_some());
    assert!(stored.earliest_refresh_at.is_some());
    let token = auth.access_token().await.unwrap();
    assert_eq!(Some(&token.0), stored.access_token.as_ref());

    // 登入流程結束後 callback 不再被接受（listener 關閉，或關閉前回 410）。
    if let Ok(response) = http.get(redirect.clone()).send().await {
        assert_eq!(response.status(), 410);
    }

    // 重啟：同一個 store，保持登入、不需要 refresh。
    drop(auth);
    let auth = h.open().await;
    assert!(matches!(
        *auth.status().borrow(),
        AuthStatus::SignedIn { .. }
    ));
    assert_eq!(auth.access_token().await.unwrap(), token);
    assert_eq!(h.fake.with(|inner| inner.refresh_calls), 0);

    // 回流登入：沿用 issued client，帶 id_token_hint／login_hint，不帶 agent_name_hint。
    let url = auth.start_login().await.unwrap();
    let params = query(&url);
    assert_eq!(params["client_id"], ISSUED_CLIENT_ID);
    assert!(!params.contains_key("agent_name_hint"));
    assert_eq!(params.get("id_token_hint"), stored.id_token.as_ref());
    assert_eq!(params["login_hint"], "u@example.com");
    assert_eq!(params["ext_agent_host_id"], h.host_id);
    // 回流的 callback 不帶 client_id 也可以完成。
    let callback = h.fake.approve(&url, "user-1", "u@example.com");
    assert_eq!(http.get(callback).send().await.unwrap().status(), 200);
    wait_status(&auth, |s| matches!(s, AuthStatus::SignedIn { .. })).await;
    assert_ne!(auth.access_token().await.unwrap(), token);
}

#[tokio::test]
async fn returning_login_with_other_account_is_rejected() {
    let h = Harness::new().await;
    h.seed(3600).await;
    let auth = h.open().await;
    let url = auth.start_login().await.unwrap();
    let callback = h.fake.approve(&url, "someone-else", "x@example.com");
    assert_eq!(reqwest::get(callback).await.unwrap().status(), 400);
    let status = wait_status(&auth, |s| matches!(s, AuthStatus::Error { .. })).await;
    assert!(matches!(status, AuthStatus::Error { message } if message.contains("帳號")));
    // 原本的憑證沒被取代，註冊也還在。
    let stored = h.stored().await;
    assert_eq!(stored.access_token.as_deref(), Some("at-seed"));
    assert_eq!(stored.client_id, ISSUED_CLIENT_ID);
    assert_eq!(stored.subject.as_deref(), Some("user-1"));
    // 別的帳號新發的 refresh token 已撤銷；原本的還有效。
    let revoked = h.fake.with(|inner| inner.revoked.clone());
    assert_eq!(revoked.len(), 1);
    assert_ne!(revoked[0]["token"], "rt-seed");
    assert!(!h.fake.is_refresh_token_valid(&revoked[0]["token"]));
    assert!(h.fake.is_refresh_token_valid("rt-seed"));
}

#[tokio::test]
async fn other_account_after_logout_resets_registration() {
    let h = Harness::new().await;
    h.seed(3600).await;
    let auth = h.open().await;
    assert_eq!(auth.logout().await.unwrap(), LogoutOutcome::Revoked);

    // 登出後在帳號選擇頁選了別的帳號：這次拒絕，但丟掉綁定舊帳號的 issued client。
    let url = auth.start_login().await.unwrap();
    assert_eq!(query(&url)["client_id"], ISSUED_CLIENT_ID);
    let callback = h.fake.approve(&url, "someone-else", "x@example.com");
    assert_eq!(reqwest::get(callback).await.unwrap().status(), 400);
    let status = wait_status(&auth, |s| matches!(s, AuthStatus::Error { .. })).await;
    assert!(matches!(status, AuthStatus::Error { message } if message.contains("重新註冊")));
    let stored = h.stored().await;
    assert_eq!(stored.client_id, "");
    assert_eq!(stored.subject, None);
    assert_eq!(stored.email, None);
    assert_eq!(stored.refresh_token, None);
    // 新發的 refresh token 沒有被採用，已撤銷。
    let revoked = h.fake.with(|inner| inner.revoked.clone());
    assert_eq!(revoked.len(), 2);
    assert!(!h.fake.is_refresh_token_valid(&revoked[1]["token"]));

    // 下一次登入重新註冊，新帳號可以完成。
    let url = auth.start_login().await.unwrap();
    let params = query(&url);
    assert_eq!(params["client_id"], DYNAMIC_CLIENT_ID);
    assert_eq!(params["agent_name_hint"], "Pots");
    assert!(!params.contains_key("id_token_hint"));
    assert!(!params.contains_key("login_hint"));
    let callback = h.fake.approve(&url, "someone-else", "x@example.com");
    assert_eq!(reqwest::get(callback).await.unwrap().status(), 200);
    wait_status(&auth, |s| matches!(s, AuthStatus::SignedIn { .. })).await;
    let stored = h.stored().await;
    assert_eq!(stored.client_id, ISSUED_CLIENT_ID);
    assert_eq!(stored.subject.as_deref(), Some("someone-else"));
    assert_eq!(stored.email.as_deref(), Some("x@example.com"));
}

#[tokio::test]
async fn logout_during_code_exchange_discards_new_tokens() {
    let h = Harness::new().await;
    let auth = h.open().await;
    let url = auth.start_login().await.unwrap();
    let callback = h.fake.approve(&url, "user-1", "u@example.com");
    h.fake
        .with(|inner| inner.code_delay = Duration::from_millis(300));
    let browser = tokio::spawn(reqwest::get(callback));
    // 等到換 token 的請求已經送出。
    tokio::time::timeout(Duration::from_secs(5), async {
        while h.fake.with(|inner| inner.token_requests.is_empty()) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    auth.logout().await.unwrap();
    assert_eq!(browser.await.unwrap().unwrap().status(), 400);

    // 登出贏：新的 token 沒有被存下，而且已撤銷。
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(*auth.status().borrow(), AuthStatus::SignedOut);
    assert_eq!(auth.access_token().await.unwrap_err(), LlmError::NeedsLogin);
    let stored = h.store().load().await.unwrap();
    assert!(stored.is_none_or(|stored| stored.refresh_token.is_none()));
    let revoked = h.fake.with(|inner| inner.revoked.clone());
    assert_eq!(revoked.len(), 1);
    assert!(!h.fake.is_refresh_token_valid(&revoked[0]["token"]));
}

#[tokio::test]
async fn pending_status_debug_hides_url_secrets() {
    let h = Harness::new().await;
    h.seed(3600).await;
    let auth = h.open().await;
    // 登入一次，讓下一次的 URL 帶 id_token_hint。
    let url = auth.start_login().await.unwrap();
    let callback = h.fake.approve(&url, "user-1", "u@example.com");
    assert_eq!(reqwest::get(callback).await.unwrap().status(), 200);
    wait_status(&auth, |s| matches!(s, AuthStatus::SignedIn { .. })).await;

    let url = auth.start_login().await.unwrap();
    let params = query(&url);
    assert!(params.contains_key("id_token_hint"));
    let debug = format!("{:?}", *auth.status().borrow());
    assert!(debug.starts_with("Pending"), "{debug}");
    for key in ["id_token_hint", "state", "nonce", "code_challenge"] {
        assert!(!debug.contains(&params[key]), "{key} 出現在 Debug 裡");
    }
    assert!(!debug.contains("u@example.com") && !debug.contains("u%40example.com"));
    assert!(debug.contains("client_id="));
}

#[tokio::test]
async fn access_denied_and_nonce_mismatch_fail_without_tokens() {
    let h = Harness::new().await;
    let auth = h.open().await;

    let url = auth.start_login().await.unwrap();
    let params = query(&url);
    let mut denied = Url::parse(&params["redirect_uri"]).unwrap();
    denied
        .query_pairs_mut()
        .append_pair("error", "access_denied")
        .append_pair("state", &params["state"]);
    assert_eq!(reqwest::get(denied).await.unwrap().status(), 400);
    let status = wait_status(&auth, |s| matches!(s, AuthStatus::Error { .. })).await;
    assert!(matches!(status, AuthStatus::Error { message } if message.contains("拒絕")));
    assert!(h.fake.with(|inner| inner.token_requests.is_empty()));

    let url = auth.start_login().await.unwrap();
    let callback = h
        .fake
        .approve_with_nonce(&url, "user-1", "u@example.com", Some("evil"));
    assert_eq!(reqwest::get(callback).await.unwrap().status(), 400);
    let status = wait_status(&auth, |s| matches!(s, AuthStatus::Error { .. })).await;
    assert!(matches!(status, AuthStatus::Error { message } if message.contains("nonce")));
    assert!(h.store().load().await.unwrap().is_none());
}

#[tokio::test]
async fn failed_registration_exchange_keeps_issued_client_id() {
    let h = Harness::new().await;
    let auth = h.open().await;
    let url = auth.start_login().await.unwrap();
    let callback = h.fake.approve(&url, "user-1", "u@example.com");
    h.fake.expire_codes();
    assert_eq!(reqwest::get(callback).await.unwrap().status(), 400);
    let status = wait_status(&auth, |s| matches!(s, AuthStatus::Error { .. })).await;
    assert!(matches!(status, AuthStatus::Error { message } if message.contains("授權碼")));

    let stored = h.stored().await;
    assert_eq!(stored.client_id, ISSUED_CLIENT_ID);
    assert_eq!(stored.access_token, None);
    // 下一次登入沿用 issued client，不再註冊。
    let url = auth.start_login().await.unwrap();
    assert_eq!(query(&url)["client_id"], ISSUED_CLIENT_ID);
    assert!(!query(&url).contains_key("agent_name_hint"));
    let callback = h.fake.approve(&url, "user-1", "u@example.com");
    assert_eq!(reqwest::get(callback).await.unwrap().status(), 200);
    wait_status(&auth, |s| matches!(s, AuthStatus::SignedIn { .. })).await;
}

#[tokio::test]
async fn refresh_during_pending_login_keeps_pending_status() {
    let h = Harness::new().await;
    h.seed(-10).await;
    let auth = h.open().await;
    let url = auth.start_login().await.unwrap();
    auth.access_token().await.unwrap();
    assert_eq!(*auth.status().borrow(), AuthStatus::Pending { url });
}

#[tokio::test]
async fn login_timeout_and_cancel() {
    let h = Harness::new().await;
    let auth = h
        .open_with(SiwcConfig {
            login_timeout: Duration::from_millis(200),
            ..h.config()
        })
        .await;
    auth.start_login().await.unwrap();
    let status = wait_status(&auth, |s| matches!(s, AuthStatus::Error { .. })).await;
    assert!(matches!(status, AuthStatus::Error { message } if message.contains("逾時")));

    let auth = h.open().await;
    let first = auth.start_login().await.unwrap();
    auth.cancel_login().await;
    wait_status(&auth, |s| *s == AuthStatus::SignedOut).await;
    // 取消後可以重新開始，而且是新的交易。
    let second = auth.start_login().await.unwrap();
    assert_ne!(query(&first)["state"], query(&second)["state"]);
}

#[tokio::test]
async fn cancel_then_immediate_restart_keeps_new_pending() {
    let h = Harness::new().await;
    let auth = h.open().await;
    for _ in 0..20 {
        auth.start_login().await.unwrap();
        auth.cancel_login().await;
        // 不等舊的流程收尾就開新的：舊的結束時不能蓋掉新的 Pending。
        let url = auth.start_login().await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            *auth.status().borrow(),
            AuthStatus::Pending { url: url.clone() }
        );
        auth.cancel_login().await;
        wait_status(&auth, |s| *s == AuthStatus::SignedOut).await;
    }

    // 新的流程仍可以完成。
    auth.start_login().await.unwrap();
    auth.cancel_login().await;
    let url = auth.start_login().await.unwrap();
    let callback = h.fake.approve(&url, "user-1", "u@example.com");
    assert_eq!(reqwest::get(callback).await.unwrap().status(), 200);
    wait_status(&auth, |s| matches!(s, AuthStatus::SignedIn { .. })).await;
}

#[tokio::test]
async fn concurrent_expired_access_refreshes_once_and_rotates() {
    let h = Harness::new().await;
    h.seed(-10).await;
    h.fake
        .with(|inner| inner.refresh_delay = Duration::from_millis(200));
    let auth = h.open().await;

    let tasks: Vec<_> = (0..10)
        .map(|_| {
            let auth = auth.clone();
            tokio::spawn(async move { auth.access_token().await })
        })
        .collect();
    let mut tokens = Vec::new();
    for task in tasks {
        tokens.push(task.await.unwrap().unwrap());
    }
    assert!(tokens.windows(2).all(|pair| pair[0] == pair[1]));
    assert_eq!(h.fake.with(|inner| inner.refresh_calls), 1);
    let refresh = h.fake.with(|inner| inner.token_requests[0].clone());
    assert_eq!(refresh["grant_type"], "refresh_token");
    assert_eq!(refresh["client_id"], ISSUED_CLIENT_ID);
    assert_eq!(refresh["refresh_token"], "rt-seed");
    assert!(!refresh.contains_key("scope"));

    // 輪替後的 token 已原子保存；舊的 refresh token 作廢。
    let stored = h.stored().await;
    assert_eq!(stored.access_token.as_ref(), Some(&tokens[0].0));
    let rotated = stored.refresh_token.clone().unwrap();
    assert_ne!(rotated, "rt-seed");
    assert!(h.fake.is_refresh_token_valid(&rotated));
    assert!(!h.fake.is_refresh_token_valid("rt-seed"));
    assert!(stored.expires_at.unwrap() > now() + 3000);

    // 重啟後沿用新的 refresh token。
    let restarted = h.open().await;
    assert_eq!(restarted.access_token().await.unwrap(), tokens[0]);
}

#[tokio::test]
async fn token_rejected_forces_one_refresh() {
    let h = Harness::new().await;
    h.seed(3600).await;
    let auth = h.open().await;
    let original = auth.access_token().await.unwrap();
    assert_eq!(original.0, "at-seed");

    let fresh = auth.token_rejected(&original).await.unwrap();
    assert_ne!(fresh, original);
    assert_eq!(h.fake.with(|inner| inner.refresh_calls), 1);
    // 舊 token 再被拒絕一次：已經換過了，直接給新的，不再 refresh。
    assert_eq!(auth.token_rejected(&original).await.unwrap(), fresh);
    assert_eq!(h.fake.with(|inner| inner.refresh_calls), 1);
    assert_eq!(auth.access_token().await.unwrap(), fresh);
    // 錯誤訊息與 Debug 都不含 token。
    assert_eq!(format!("{fresh:?}"), "AccessToken(..)");
    let _: AccessToken = fresh;
}

#[tokio::test]
async fn terminal_refresh_error_needs_login_but_transient_keeps_credentials() {
    let h = Harness::new().await;
    h.seed(-10).await;
    let auth = h.open().await;

    h.fake
        .with(|inner| inner.refresh_mode = RefreshMode::Unavailable);
    assert!(matches!(
        auth.access_token().await,
        Err(LlmError::Unavailable(_))
    ));
    assert_eq!(h.stored().await.refresh_token.as_deref(), Some("rt-seed"));
    assert!(matches!(
        *auth.status().borrow(),
        AuthStatus::SignedIn { .. }
    ));

    h.fake
        .with(|inner| inner.refresh_mode = RefreshMode::Error("invalid_grant"));
    assert_eq!(auth.access_token().await.unwrap_err(), LlmError::NeedsLogin);
    assert!(matches!(*auth.status().borrow(), AuthStatus::Error { .. }));
    let stored = h.stored().await;
    assert_eq!(stored.refresh_token, None);
    assert_eq!(stored.access_token, None);
    assert_eq!(stored.client_id, ISSUED_CLIENT_ID);
    // 之後直接回 NeedsLogin，不再打 token endpoint。
    let calls = h.fake.with(|inner| inner.refresh_calls);
    assert_eq!(auth.access_token().await.unwrap_err(), LlmError::NeedsLogin);
    assert_eq!(h.fake.with(|inner| inner.refresh_calls), calls);
}

#[tokio::test]
async fn garbled_refresh_success_needs_login() {
    let h = Harness::new().await;
    h.seed(-10).await;
    let auth = h.open().await;
    h.fake
        .with(|inner| inner.refresh_mode = RefreshMode::Garbled);
    // 2xx 但讀不到 token：舊的 refresh token 多半已經輪替，不能當成暫時失敗一直重試。
    assert_eq!(auth.access_token().await.unwrap_err(), LlmError::NeedsLogin);
    assert!(matches!(*auth.status().borrow(), AuthStatus::Error { .. }));
    let stored = h.stored().await;
    assert_eq!(stored.refresh_token, None);
    assert_eq!(stored.access_token, None);
    assert_eq!(stored.client_id, ISSUED_CLIENT_ID);
    let calls = h.fake.with(|inner| inner.refresh_calls);
    assert_eq!(auth.access_token().await.unwrap_err(), LlmError::NeedsLogin);
    assert_eq!(h.fake.with(|inner| inner.refresh_calls), calls);
}

#[tokio::test]
async fn logout_revokes_and_keeps_registration() {
    let h = Harness::new().await;
    h.seed(3600).await;
    let auth = h.open().await;
    assert_eq!(auth.logout().await.unwrap(), LogoutOutcome::Revoked);

    let revoked = h.fake.with(|inner| inner.revoked.clone());
    assert_eq!(revoked.len(), 1);
    assert_eq!(revoked[0]["token"], "rt-seed");
    assert_eq!(revoked[0]["token_type_hint"], "refresh_token");
    assert_eq!(revoked[0]["client_id"], ISSUED_CLIENT_ID);
    assert!(!h.fake.is_refresh_token_valid("rt-seed"));

    assert_eq!(*auth.status().borrow(), AuthStatus::SignedOut);
    assert_eq!(auth.access_token().await.unwrap_err(), LlmError::NeedsLogin);
    let stored = h.stored().await;
    assert_eq!(stored.client_id, ISSUED_CLIENT_ID);
    assert_eq!(stored.subject.as_deref(), Some("user-1"));
    assert_eq!(stored.access_token, None);
    assert_eq!(stored.refresh_token, None);
    assert_eq!(stored.id_token, None);
    assert_eq!(
        load_or_create_host_id(&h.dir.path().join("host-id")).unwrap(),
        h.host_id
    );

    // 再登入沿用 issued client，不重新註冊。
    let url = auth.start_login().await.unwrap();
    assert_eq!(query(&url)["client_id"], ISSUED_CLIENT_ID);
    assert!(!query(&url).contains_key("id_token_hint"));
    assert_eq!(auth.logout().await.unwrap(), LogoutOutcome::NothingToRevoke);
}

#[tokio::test]
async fn login_without_plan_scope_is_signed_in_but_not_entitled() {
    let h = Harness::new().await;
    h.fake
        .with(|inner| inner.granted_scope = "openid profile email offline_access".into());
    let auth = h.open().await;
    let url = auth.start_login().await.unwrap();
    let callback = h.fake.approve(&url, "user-1", "u@example.com");
    assert_eq!(reqwest::get(callback).await.unwrap().status(), 200);
    let status = wait_status(&auth, |s| matches!(s, AuthStatus::SignedIn { .. })).await;
    assert_eq!(
        status,
        AuthStatus::SignedIn {
            email: Some("u@example.com".into()),
            plan_usage: false
        }
    );
    assert!(matches!(
        auth.access_token().await,
        Err(LlmError::NotEntitled(_))
    ));
}
