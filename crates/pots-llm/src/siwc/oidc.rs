//! OIDC discovery、token endpoint、撤銷與 ID token 驗證。
//!
//! 所有端點都從 issuer 的 discovery 取得，不信任 token 或 callback 裡帶來的 URL。

use std::time::Duration;

use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;
use url::Url;

use crate::provider::LlmError;

/// 單一 HTTP 請求的逾時；token endpoint 不應該花更久。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// ID token 時間欄位的容許誤差。
const CLOCK_LEEWAY_SECS: u64 = 5;

/// refresh 失敗時，代表 refresh token 已經不能用、要重新登入的錯誤碼。
const TERMINAL_REFRESH_ERRORS: &[&str] = &[
    "invalid_grant",
    "invalid_refresh_token",
    "token_expired",
    "refresh_token_expired",
    "refresh_token_invalidated",
    "refresh_token_reused",
];

/// discovery 文件中用到的部分。
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Discovery {
    pub issuer: String,
    pub authorization_endpoint: Url,
    pub token_endpoint: Url,
    pub revocation_endpoint: Option<Url>,
    pub jwks_uri: Url,
}

/// token endpoint 的成功回應。`Debug` 不印 token。
#[derive(Clone, Deserialize)]
pub(crate) struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub expires_in: Option<i64>,
    /// 實際授予的 scope（空白分隔）；entitlement 以這個為準，不看 callback 的 `scope`。
    pub scope: Option<String>,
    /// 格式未經文件確認，原樣保存。
    pub earliest_refresh_at: Option<Value>,
}

/// 驗證過的 ID token 內容。
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct IdClaims {
    pub sub: String,
    pub email: Option<String>,
    pub nonce: Option<String>,
    pub sid: Option<Value>,
}

/// token endpoint 失敗的分類。
#[derive(Debug)]
pub(crate) enum TokenError {
    /// refresh token／授權碼已失效，要重新登入。
    Terminal(String),
    /// client 設定錯誤（`invalid_client`）。
    Client(String),
    /// 網路或伺服器暫時失敗；保留憑證。
    Transient(String),
    /// 其他 OAuth 錯誤；保留憑證。
    Other(String),
}

impl TokenError {
    pub fn message(&self) -> &str {
        match self {
            Self::Terminal(m) | Self::Client(m) | Self::Transient(m) | Self::Other(m) => m,
        }
    }
}

/// discovery 與 JWKS 的快取。
pub(crate) struct OidcClient {
    issuer: Url,
    http: reqwest::Client,
    discovery: Mutex<Option<Discovery>>,
    jwks: Mutex<Option<JwkSet>>,
}

/// 去掉結尾的 `/`，用來比較 issuer。
pub(crate) fn issuer_string(issuer: &Url) -> String {
    issuer.as_str().trim_end_matches('/').to_owned()
}

impl OidcClient {
    pub fn new(issuer: Url, http: reqwest::Client) -> Self {
        Self {
            issuer,
            http,
            discovery: Mutex::new(None),
            jwks: Mutex::new(None),
        }
    }

    /// 取 discovery；成功才快取。discovery 的 `issuer` 必須等於設定的 issuer。
    pub async fn discovery(&self) -> Result<Discovery, LlmError> {
        let mut cached = self.discovery.lock().await;
        if let Some(discovery) = cached.as_ref() {
            return Ok(discovery.clone());
        }
        let expected = issuer_string(&self.issuer);
        let url = format!("{expected}/.well-known/openid-configuration");
        let response = self
            .http
            .get(&url)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|e| {
                LlmError::Unavailable(format!("無法取得 OIDC discovery：{}", e.without_url()))
            })?;
        if !response.status().is_success() {
            return Err(LlmError::Unavailable(format!(
                "OIDC discovery 回 HTTP {}",
                response.status().as_u16()
            )));
        }
        let discovery: Discovery = response
            .json()
            .await
            .map_err(|e| LlmError::Other(format!("OIDC discovery 格式不符：{e}")))?;
        if discovery.issuer.trim_end_matches('/') != expected {
            return Err(LlmError::Other(format!(
                "OIDC discovery 的 issuer `{}` 與設定的 `{expected}` 不符",
                discovery.issuer
            )));
        }
        *cached = Some(discovery.clone());
        Ok(discovery)
    }

    /// 送出 token endpoint 請求（form、沒有 client secret）。
    pub async fn token(
        &self,
        discovery: &Discovery,
        form: &[(&str, &str)],
    ) -> Result<TokenResponse, TokenError> {
        let response = self
            .http
            .post(discovery.token_endpoint.clone())
            .timeout(REQUEST_TIMEOUT)
            .form(form)
            .send()
            .await
            .map_err(|e| {
                TokenError::Transient(format!("連線 token endpoint 失敗：{}", e.without_url()))
            })?;
        let status = response.status();
        if status.is_success() {
            return response
                .json()
                .await
                .map_err(|e| TokenError::Transient(format!("token 回應格式不符：{e}")));
        }
        let body: Value = response.json().await.unwrap_or(Value::Null);
        let code = body.get("error").and_then(|error| match error {
            Value::String(code) => Some(code.clone()),
            // 有些實作把錯誤包成物件。
            Value::Object(object) => object
                .get("code")
                .and_then(Value::as_str)
                .map(str::to_owned),
            _ => None,
        });
        let description = body
            .get("error_description")
            .or_else(|| body.pointer("/error/message"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let message = format!(
            "token endpoint 回 HTTP {}{}{}",
            status.as_u16(),
            code.as_deref().map(|c| format!(" {c}")).unwrap_or_default(),
            if description.is_empty() {
                String::new()
            } else {
                format!("：{description}")
            }
        );
        Err(match code.as_deref() {
            Some(code) if TERMINAL_REFRESH_ERRORS.contains(&code) => TokenError::Terminal(message),
            Some("invalid_client" | "unauthorized_client") => TokenError::Client(message),
            _ if status.is_server_error() || status.as_u16() == 429 => {
                TokenError::Transient(message)
            }
            _ => TokenError::Other(message),
        })
    }

    /// 撤銷 refresh token。HTTP 200 即成功（即使 token 本來就失效）。
    pub async fn revoke(
        &self,
        discovery: &Discovery,
        client_id: &str,
        refresh_token: &str,
    ) -> Result<(), TokenError> {
        let Some(endpoint) = discovery.revocation_endpoint.clone() else {
            return Err(TokenError::Other(
                "discovery 沒有 revocation_endpoint".into(),
            ));
        };
        let response = self
            .http
            .post(endpoint)
            .timeout(REQUEST_TIMEOUT)
            .form(&[
                ("token", refresh_token),
                ("token_type_hint", "refresh_token"),
                ("client_id", client_id),
            ])
            .send()
            .await
            .map_err(|e| {
                TokenError::Transient(format!(
                    "連線 revocation endpoint 失敗：{}",
                    e.without_url()
                ))
            })?;
        let status = response.status();
        if status.is_success() {
            Ok(())
        } else if status.is_server_error() || status.as_u16() == 429 {
            Err(TokenError::Transient(format!(
                "撤銷回 HTTP {}",
                status.as_u16()
            )))
        } else {
            Err(TokenError::Other(format!(
                "撤銷回 HTTP {}",
                status.as_u16()
            )))
        }
    }

    /// 驗證 ID token：RS256 簽章（JWKS）、issuer、audience 含 `client_id`、`exp`、
    /// `nonce`（有給 `expected_nonce` 時）、非空的 `sub`。
    pub async fn verify_id_token(
        &self,
        discovery: &Discovery,
        id_token: &str,
        client_id: &str,
        expected_nonce: Option<&str>,
    ) -> Result<IdClaims, LlmError> {
        let invalid = |reason: String| LlmError::Other(format!("ID token 驗證失敗：{reason}"));
        let header = jsonwebtoken::decode_header(id_token).map_err(|e| invalid(e.to_string()))?;
        if header.alg != Algorithm::RS256 {
            return Err(invalid(format!("不接受的演算法 {:?}", header.alg)));
        }
        let key = self.decoding_key(discovery, header.kid.as_deref()).await?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[discovery.issuer.as_str()]);
        validation.set_audience(&[client_id]);
        validation.set_required_spec_claims(&["exp", "iat", "sub", "iss", "aud"]);
        validation.leeway = CLOCK_LEEWAY_SECS;
        let claims = jsonwebtoken::decode::<IdClaims>(id_token, &key, &validation)
            .map_err(|e| invalid(e.to_string()))?
            .claims;
        if claims.sub.is_empty() {
            return Err(invalid("sub 是空的".into()));
        }
        if let Some(expected) = expected_nonce
            && claims.nonce.as_deref() != Some(expected)
        {
            return Err(invalid("nonce 不符".into()));
        }
        if let Some(sid) = &claims.sid
            && sid.as_str().is_none_or(str::is_empty)
        {
            return Err(invalid("sid 不是非空字串".into()));
        }
        Ok(claims)
    }

    /// 找 `kid` 對應的公鑰；快取裡沒有就重新抓一次 JWKS。
    async fn decoding_key(
        &self,
        discovery: &Discovery,
        kid: Option<&str>,
    ) -> Result<DecodingKey, LlmError> {
        let mut jwks = self.jwks.lock().await;
        for attempt in 0..2 {
            if attempt == 1 || jwks.is_none() {
                *jwks = Some(self.fetch_jwks(discovery).await?);
            }
            if let Some(set) = jwks.as_ref() {
                let jwk = match kid {
                    Some(kid) => set.find(kid),
                    None if set.keys.len() == 1 => set.keys.first(),
                    None => None,
                };
                if let Some(jwk) = jwk {
                    return DecodingKey::from_jwk(jwk)
                        .map_err(|e| LlmError::Other(format!("JWKS 金鑰無法使用：{e}")));
                }
            }
        }
        Err(LlmError::Other(
            "ID token 驗證失敗：JWKS 裡沒有對應的金鑰".into(),
        ))
    }

    async fn fetch_jwks(&self, discovery: &Discovery) -> Result<JwkSet, LlmError> {
        let response = self
            .http
            .get(discovery.jwks_uri.clone())
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|e| LlmError::Unavailable(format!("無法取得 JWKS：{}", e.without_url())))?;
        if !response.status().is_success() {
            return Err(LlmError::Unavailable(format!(
                "JWKS 回 HTTP {}",
                response.status().as_u16()
            )));
        }
        response
            .json()
            .await
            .map_err(|e| LlmError::Other(format!("JWKS 格式不符：{e}")))
    }
}
