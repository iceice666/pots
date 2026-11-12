//! 與模型無關的請求與串流事件。
//!
//! [`Item`] 是對話歷史的單位：使用者訊息、模型回覆、工具呼叫與結果。伺服器把
//! 每個 `Item` 序列化成 JSON 存進 `om_raw`，下一輪原樣放回 [`ModelRequest::input`]，
//! 所以 provider 專屬、需要原樣回送的內容（例如 reasoning）放在 [`Item::Opaque`]。
//!
//! [`Item::Image`] 也只存附件參照（id + MIME）：base64 在組請求時才由呼叫端讀檔產生
//! （見 [`crate::image`]），不會進 `om_raw`、資料庫或 events 日誌。

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
    /// 使用者附上的圖片。**只存參照**：`attachment_id` 是本機附件檔的 ID，`mime` 是它的型別。
    ///
    /// 這是持久化格式，所以不要在這裡放 base64：base64 在組請求時才由呼叫端從本機檔案產生
    /// （見 [`crate::image::ImageLoader`]）。不支援圖片的模型由 provider 回錯並轉成白話說明。
    Image { attachment_id: String, mime: String },
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
    /// 這次對話的穩定 ID，給需要 session affinity 的 provider（例如 OpenCode 的
    /// `x-opencode-session`）。沒有設定時不送。
    pub session_id: Option<String>,
    /// 推理強度；`None` 不送，用模型預設值。
    pub reasoning: Option<Reasoning>,
}

/// 要求模型推理（thinking）的強度。各 provider 依 API 轉換：
/// Chat Completions 的 `reasoning_effort`、Responses 的 `reasoning.effort`、
/// Anthropic Messages 的 `thinking` 與 `output_config.effort`。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reasoning {
    /// 明確關掉推理。
    Off,
    Low,
    Medium,
    High,
}

impl Reasoning {
    /// OpenAI 的 effort 字串（`none`、`low`、`medium`、`high`）。
    pub fn openai_effort(self) -> &'static str {
        match self {
            Self::Off => "none",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
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
    /// 模型不支援圖片輸入（非 vision 模型）。provider 的原始錯誤已經轉成白話說明，
    /// 顯示時直接用它，不要把 API 錯誤原樣丟給使用者。
    #[error("目前選用的模型不支援圖片輸入")]
    UnsupportedImage,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_item_round_trips_and_only_holds_a_reference() {
        let item = Item::Image {
            attachment_id: "01a1ce3f-464e-70e0-8867-8efd52f1d10d".into(),
            mime: "image/png".into(),
        };
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "type": "image",
                "attachment_id": "01a1ce3f-464e-70e0-8867-8efd52f1d10d",
                "mime": "image/png",
            })
        );
        assert_eq!(serde_json::from_value::<Item>(json).unwrap(), item);
        // 歷史是持久化格式：裡面不該出現 base64 或 data URL。
        let text = serde_json::to_string(&item).unwrap();
        assert!(
            !text.contains("base64") && !text.contains("data:"),
            "{text}"
        );
    }

    #[test]
    fn existing_items_keep_their_serialized_shape() {
        // 已經存進 `om_raw` 的舊項目要能原樣讀回來。
        assert_eq!(
            serde_json::from_value::<Item>(serde_json::json!({
                "type": "tool_result", "call_id": "c1", "output": "12:00",
            }))
            .unwrap(),
            Item::ToolResult {
                call_id: "c1".into(),
                output: "12:00".into(),
            }
        );
    }
}
