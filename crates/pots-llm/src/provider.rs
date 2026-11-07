//! 與模型無關的請求與串流事件。
//!
//! [`Item`] 是對話歷史的單位：使用者訊息、模型回覆、工具呼叫與結果。伺服器把
//! 每個 `Item` 序列化成 JSON 存進 `om_raw`，下一輪原樣放回 [`ModelRequest::input`]，
//! 所以 provider 專屬、需要原樣回送的內容（例如 reasoning）放在 [`Item::Opaque`]。

use std::pin::Pin;

use futures::Stream;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// 對話歷史的一個單位。序列化格式要穩定：伺服器會持久化它。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Item {
    /// 人類訊息，或平台注入給模型看的觸發訊息。
    User { text: String },
    /// 模型的文字回覆。
    Assistant { text: String },
    /// 模型要求呼叫工具。`arguments` 是模型給的 JSON 字串。
    ToolCall(ToolCall),
    /// 工具結果，送回給模型。
    ToolResult { call_id: String, output: String },
    /// provider 專屬、要原樣回送的項目（例如 Responses 的 reasoning item）。
    /// `provider` 不符時，其他 provider 可以略過它。
    Opaque {
        provider: String,
        value: serde_json::Value,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub call_id: String,
    /// 工具名稱，不含 namespace。
    pub name: String,
    pub arguments: String,
}

/// 提供給模型的工具。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema（object）。
    pub parameters: serde_json::Value,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelRequest {
    /// provider 的模型 ID，例如 Responses 的 model slug。
    pub model: String,
    /// 系統層級的指示；Responses 放在頂層 `instructions`，不會變成 system item。
    pub instructions: Option<String>,
    /// 完整的必要歷史，依時間排序。
    pub input: Vec<Item>,
    pub tools: Vec<ToolSpec>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, other: Self) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
    }
}

/// 串流中的事件。成功的串流以 [`StreamEvent::Completed`] 結束；
/// 沒收到它就斷掉的串流要回報 [`LlmError::Interrupted`]，不能當作成功。
#[derive(Clone, Debug, PartialEq)]
pub enum StreamEvent {
    TextDelta(String),
    /// 一個輸出項目完整結束（文字、工具呼叫或 opaque）。依模型輸出順序送出。
    ItemDone(Item),
    Completed {
        usage: Usage,
    },
}

pub type EventStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, LlmError>> + Send>>;

/// 模型推論後端。
#[async_trait::async_trait]
pub trait ModelProvider: Send + Sync {
    /// provider 名稱，用於 [`Item::Opaque`]。
    fn name(&self) -> &str;

    /// 開始一次串流推論。`cancel` 觸發時串流要盡快以 [`LlmError::Cancelled`] 結束。
    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, LlmError>;
}

/// 推論失敗的分類。訊息不得包含 token。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LlmError {
    /// 沒有登入，或憑證已失效，需要重新登入。
    #[error("需要登入 ChatGPT")]
    NeedsLogin,
    /// 已登入但沒有 ChatGPT plan 推論權限（未授予 scope、帳號或 workspace 不符資格）。
    #[error("這個帳號不能使用 ChatGPT plan 推論：{0}")]
    NotEntitled(String),
    /// 額度用完；不自動改用其他計費方式。
    #[error("ChatGPT plan 額度已用完")]
    UsageLimit,
    /// 暫時無法使用，可以稍後重試。
    #[error("模型暫時無法使用：{0}")]
    Unavailable(String),
    /// 請求內容不被支援（參數、工具、模型）；重試同一個請求沒有用。
    #[error("請求不被支援：{0}")]
    Unsupported(String),
    /// 串流在完成前中斷。
    #[error("串流中斷：{0}")]
    Interrupted(String),
    #[error("已取消")]
    Cancelled,
    /// 還沒設定（或選擇）模型 provider。
    #[error("還沒設定模型 provider")]
    NotConfigured,
    /// API key 無效、過期或沒有權限（API provider 的 401／403）。
    #[error("API key 無效或沒有權限：{0}")]
    InvalidCredentials(String),
    #[error("{0}")]
    Other(String),
}

impl LlmError {
    /// 稍後重試同一個請求可能成功。
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Unavailable(_) | Self::Interrupted(_))
    }
}
