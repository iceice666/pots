//! 登入流程的零件：PKCE、授權 URL、loopback callback listener。

use std::collections::HashMap;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Html;
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use url::Url;

use super::{CALLBACK_PATH, DYNAMIC_CLIENT_ID, SCOPES};
use crate::provider::LlmError;

/// 32 bytes 的密碼學亂數，base64url（無 padding）。
pub(crate) fn random_token() -> Result<String, LlmError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| LlmError::Other(format!("無法產生亂數：{e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// PKCE S256 的 code challenge。
pub(crate) fn code_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// 一次授權嘗試的一次性資料。`Debug` 不印內容。
pub(crate) struct LoginTransaction {
    pub state: String,
    pub nonce: String,
    pub verifier: String,
    pub redirect_uri: String,
    /// 授權請求用的 client ID：首次是 [`DYNAMIC_CLIENT_ID`]，之後是 issued ID。
    pub client_id: String,
    /// 上一次登入驗證過的 `sub`；回流登入時新身分必須相同。
    pub previous_subject: Option<String>,
}

impl LoginTransaction {
    pub fn is_registration(&self) -> bool {
        self.client_id == DYNAMIC_CLIENT_ID
    }
}

/// 回流登入的提示（同一個帳號）。
pub(crate) struct LoginHints<'a> {
    pub id_token: Option<&'a str>,
    pub email: Option<&'a str>,
}

/// 組出授權 URL。首次註冊帶 `agent_name_hint`；回流登入帶 `id_token_hint`／`login_hint`。
pub(crate) fn authorize_url(
    endpoint: &Url,
    transaction: &LoginTransaction,
    app_name: &str,
    host_id: &str,
    resource: &str,
    hints: &LoginHints<'_>,
) -> Url {
    let mut url = endpoint.clone();
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("client_id", &transaction.client_id);
        if transaction.is_registration() {
            query.append_pair("agent_name_hint", app_name);
        }
        query
            .append_pair("ext_agent_host_id", host_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &transaction.redirect_uri)
            .append_pair("scope", SCOPES)
            .append_pair("resource", resource)
            .append_pair("state", &transaction.state)
            .append_pair("nonce", &transaction.nonce)
            .append_pair("code_challenge_method", "S256")
            .append_pair("code_challenge", &code_challenge(&transaction.verifier));
        if !transaction.is_registration() {
            if let Some(id_token) = hints.id_token {
                query.append_pair("id_token_hint", id_token);
            }
            if let Some(email) = hints.email {
                query.append_pair("login_hint", email);
            }
        }
    }
    url
}

/// 綁 `127.0.0.1`（絕不綁所有介面）。偏好的 port 被占用時改用隨機 port。
pub(crate) async fn bind_loopback(preferred: Option<u16>) -> Result<TcpListener, LlmError> {
    if let Some(port) = preferred {
        match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => return Ok(listener),
            Err(error) => tracing::info!(port, %error, "callback port 被占用，改用隨機 port"),
        }
    }
    TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| LlmError::Other(format!("無法開啟 loopback callback：{e}")))
}

/// 瀏覽器打到 callback 的一次請求；登入流程處理完後用 `reply` 回頁面。
pub(crate) struct CallbackRequest {
    pub params: HashMap<String, String>,
    pub reply: oneshot::Sender<(StatusCode, String)>,
}

/// 啟動 callback server，回傳收請求的 channel。`shutdown` 觸發後停止。
pub(crate) fn serve_callback(
    listener: TcpListener,
    shutdown: CancellationToken,
) -> mpsc::Receiver<CallbackRequest> {
    let (sender, receiver) = mpsc::channel(8);
    let app = Router::new()
        .route(CALLBACK_PATH, get(callback))
        .with_state(sender);
    tokio::spawn(async move {
        let server = axum::serve(listener, app).with_graceful_shutdown(shutdown.cancelled_owned());
        if let Err(error) = server.await {
            tracing::warn!(%error, "loopback callback server 結束時發生錯誤");
        }
    });
    receiver
}

async fn callback(
    State(sender): State<mpsc::Sender<CallbackRequest>>,
    Query(params): Query<HashMap<String, String>>,
) -> (StatusCode, Html<String>) {
    let (reply, response) = oneshot::channel();
    let ended = || {
        (
            StatusCode::GONE,
            Html(page("這個登入流程已經結束，請回到 Pots 重新登入。")),
        )
    };
    if sender
        .send(CallbackRequest { params, reply })
        .await
        .is_err()
    {
        return ended();
    }
    match response.await {
        Ok((status, message)) => (status, Html(page(&message))),
        Err(_) => ended(),
    }
}

/// callback 的回應頁。訊息是固定文字，不含任何 token 或使用者輸入。
fn page(message: &str) -> String {
    format!(
        "<!doctype html><html lang=\"zh-Hant\"><head><meta charset=\"utf-8\"><title>Pots</title></head>\
         <body style=\"font-family: system-ui; margin: 3rem\"><h1>Pots</h1><p>{message}</p></body></html>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_rfc7636_example() {
        // RFC 7636 附錄 B。
        assert_eq!(
            code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(random_token().unwrap().len(), 43);
    }

    fn transaction(client_id: &str) -> LoginTransaction {
        LoginTransaction {
            state: "st".into(),
            nonce: "no".into(),
            verifier: "ve".into(),
            redirect_uri: "http://127.0.0.1:1455/auth/callback".into(),
            client_id: client_id.into(),
            previous_subject: None,
        }
    }

    fn params(url: &Url) -> HashMap<String, String> {
        url.query_pairs().into_owned().collect()
    }

    #[test]
    fn registration_url() {
        let endpoint = Url::parse("https://auth.example/authorize").unwrap();
        let hints = LoginHints {
            id_token: Some("idt"),
            email: Some("a@b"),
        };
        let url = authorize_url(
            &endpoint,
            &transaction(DYNAMIC_CLIENT_ID),
            "Pots",
            "urn:uuid:1",
            "https://api.openai.com/v1",
            &hints,
        );
        let p = params(&url);
        assert_eq!(p["client_id"], DYNAMIC_CLIENT_ID);
        assert_eq!(p["agent_name_hint"], "Pots");
        assert_eq!(p["ext_agent_host_id"], "urn:uuid:1");
        assert_eq!(p["response_type"], "code");
        assert_eq!(p["redirect_uri"], "http://127.0.0.1:1455/auth/callback");
        assert_eq!(p["scope"], SCOPES);
        assert_eq!(p["resource"], "https://api.openai.com/v1");
        assert_eq!(p["code_challenge_method"], "S256");
        assert_eq!(p["code_challenge"], code_challenge("ve"));
        assert!(!p.contains_key("id_token_hint"));
        assert!(!p.contains_key("login_hint"));
    }

    #[test]
    fn returning_url_uses_hints() {
        let endpoint = Url::parse("https://auth.example/authorize").unwrap();
        let hints = LoginHints {
            id_token: Some("idt"),
            email: Some("a@b"),
        };
        let url = authorize_url(
            &endpoint,
            &transaction("oaiapp_1"),
            "Pots",
            "urn:uuid:1",
            "r",
            &hints,
        );
        let p = params(&url);
        assert_eq!(p["client_id"], "oaiapp_1");
        assert!(!p.contains_key("agent_name_hint"));
        assert_eq!(p["id_token_hint"], "idt");
        assert_eq!(p["login_hint"], "a@b");
    }
}
