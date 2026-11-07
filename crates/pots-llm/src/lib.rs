//! Pots 的模型推論層。
//!
//! - [`provider`]：與模型無關的請求、串流事件與 [`ModelProvider`] trait。
//! - [`responses`]：OpenAI Responses API 的 provider，給 ChatGPT Plan（SIWC）用。
//! - [`siwc`]：Sign in with ChatGPT 的登入、憑證儲存與單一 refresh owner。
//! - [`agent`]：所有 agent profile 共用的多輪工具迴圈。
//! - `testing`（feature `test-util`）：給其他 crate 測試用的腳本化 provider 與工具。
//!
//! 不依賴 rig：SIWC 對 Responses 請求的限制多（見 `proposal/agent-runtime.md`），
//! 用自有的 provider 介面與 loop 比較好控制。之後要換 provider 只需實作 [`ModelProvider`]。

pub mod agent;
pub mod provider;
pub mod responses;
pub mod siwc;
#[cfg(any(test, feature = "test-util"))]
pub mod testing;

pub use provider::*;

/// 重新匯出，讓呼叫端不必自己對齊 reqwest 版本就能建立 [`reqwest::Client`]。
pub use reqwest;
