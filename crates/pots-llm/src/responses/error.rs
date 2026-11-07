//! 把 Responses／SIWC 的錯誤碼對應到 [`LlmError`]。
//!
//! 錯誤碼表見 `target/tmp/siwc-research.md` 第 6 節（官方 errors-and-recovery 頁）。
//! 訊息保留錯誤碼、`param` 與 request ID 方便診斷；不會包含 token。

use serde_json::Value;

use crate::provider::LlmError;

/// 從錯誤回應整理出來的資訊。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApiError {
    /// HTTP 狀態碼；串流中的 `response.failed` 沒有。
    pub status: Option<u16>,
    pub code: Option<String>,
    pub message: Option<String>,
    pub param: Option<String>,
    pub request_id: Option<String>,
}

impl ApiError {
    /// 解析 `{"error": {...}}`、`{"detail": "..."}` 或 error 物件本身。
    pub fn from_json(value: &Value) -> Self {
        let error = value
            .get("error")
            .filter(|e| e.is_object())
            .unwrap_or(value);
        let text = |key: &str| error.get(key).and_then(Value::as_str).map(str::to_owned);
        let message = text("message").or_else(|| {
            value
                .get("detail")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
        Self {
            status: None,
            code: text("code").or_else(|| text("type").filter(|t| t != "error")),
            message,
            param: text("param"),
            request_id: None,
        }
    }

    /// 解析 HTTP 錯誤回應的 body；不是 JSON 時把（截斷的）文字當作訊息。
    pub fn from_http(status: u16, body: &str, request_id: Option<String>) -> Self {
        let mut error = match serde_json::from_str::<Value>(body) {
            Ok(value) => Self::from_json(&value),
            Err(_) => Self {
                message: Some(truncate(body.trim(), 300)).filter(|s| !s.is_empty()),
                ..Self::default()
            },
        };
        error.status = Some(status);
        error.request_id = request_id;
        error
    }

    fn detail(&self) -> String {
        let mut parts = Vec::new();
        if let Some(status) = self.status {
            parts.push(format!("HTTP {status}"));
        }
        if let Some(code) = &self.code {
            parts.push(code.clone());
        }
        let mut detail = parts.join(" ");
        if let Some(message) = &self.message {
            if !detail.is_empty() {
                detail.push('：');
            }
            detail.push_str(message);
        }
        if let Some(param) = &self.param {
            detail.push_str(&format!("（param: {param}）"));
        }
        if let Some(request_id) = &self.request_id {
            detail.push_str(&format!("（request id: {request_id}）"));
        }
        if detail.is_empty() {
            "未知錯誤".into()
        } else {
            detail
        }
    }

    /// 對應到 [`LlmError`]。先看錯誤碼，沒有已知錯誤碼時才看 HTTP 狀態。
    pub fn into_llm_error(self) -> LlmError {
        let detail = self.detail();
        match self.code.as_deref() {
            Some(
                "subscription_sharing_usage_limit_exceeded"
                | "usage_limit_exceeded"
                | "usage_limit_reached"
                | "insufficient_quota",
            ) => return LlmError::UsageLimit,
            Some(
                "subscription_sharing_user_not_eligible"
                | "subscription_sharing_route_not_supported"
                | "chatpass_v2_scope_not_authorized"
                | "chatpass_v2_invalid_authorization_context",
            ) => return LlmError::NotEntitled(detail),
            Some(
                "subscription_sharing_usage_unavailable"
                | "subscription_sharing_user_unavailable"
                | "server_error"
                | "server_is_overloaded"
                | "rate_limit_exceeded"
                | "slow_down",
            ) => return LlmError::Unavailable(detail),
            Some(
                "subscription_sharing_unsupported_capability"
                | "context_length_exceeded"
                | "invalid_prompt"
                | "unsupported_parameter"
                | "unsupported_value",
            ) => return LlmError::Unsupported(detail),
            Some("subscription_sharing_invalid_user" | "invalid_api_key") => {
                return LlmError::NeedsLogin;
            }
            _ => {}
        }
        match self.status {
            Some(401) => LlmError::NeedsLogin,
            Some(403) => LlmError::NotEntitled(detail),
            Some(408 | 429 | 500..=599) => LlmError::Unavailable(detail),
            Some(400..=499) => LlmError::Unsupported(detail),
            _ => LlmError::Other(detail),
        }
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((index, _)) => format!("{}…", &text[..index]),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn http(status: u16, body: Value) -> LlmError {
        ApiError::from_http(status, &body.to_string(), Some("req_1".into())).into_llm_error()
    }

    #[test]
    fn known_codes() {
        let usage =
            json!({"error": {"code": "subscription_sharing_usage_limit_exceeded", "message": "x"}});
        assert_eq!(http(429, usage), LlmError::UsageLimit);
        let not_eligible =
            json!({"error": {"code": "subscription_sharing_user_not_eligible", "message": "no"}});
        assert!(matches!(http(403, not_eligible), LlmError::NotEntitled(m) if m.contains("req_1")));
        let scope = json!({"error": {"code": "chatpass_v2_scope_not_authorized"}});
        assert!(matches!(http(403, scope), LlmError::NotEntitled(_)));
        let unavailable = json!({"error": {"code": "subscription_sharing_usage_unavailable"}});
        assert!(matches!(http(503, unavailable), LlmError::Unavailable(_)));
        let unsupported = json!({"error": {"code": "subscription_sharing_unsupported_capability", "param": "tools"}});
        assert!(matches!(http(400, unsupported), LlmError::Unsupported(m) if m.contains("tools")));
        let invalid_user = json!({"error": {"code": "subscription_sharing_invalid_user"}});
        assert_eq!(http(401, invalid_user), LlmError::NeedsLogin);
    }

    #[test]
    fn falls_back_to_status_and_detail() {
        assert!(
            matches!(http(403, json!({"detail": "region"})), LlmError::NotEntitled(m) if m.contains("region"))
        );
        assert!(matches!(
            http(503, json!({"detail": "busy"})),
            LlmError::Unavailable(_)
        ));
        assert_eq!(http(401, json!({"detail": "nope"})), LlmError::NeedsLogin);
        let plain = ApiError::from_http(502, "<html>bad gateway</html>", None).into_llm_error();
        assert!(matches!(plain, LlmError::Unavailable(m) if m.contains("bad gateway")));
    }

    #[test]
    fn stream_failure_without_status() {
        let failed =
            ApiError::from_json(&json!({"code": "weird", "message": "boom"})).into_llm_error();
        assert_eq!(failed, LlmError::Other("weird：boom".into()));
    }
}
