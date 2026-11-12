//! Pots 桌面客戶端。
//!
//! 目前只有 UI：資料來自 [`mock`]，按鈕會改變本地狀態並把要送出的指令記在
//! [`model::Store::outbox`]，還沒有連到任何服務。

pub mod app;
pub mod connection;
pub mod members;
pub mod mock;
pub mod model;
pub mod notify;
pub mod sync;
mod theme;
mod views;

/// 訊息附圖的取圖快取（`GET /attachments/{id}`）；整合測試也會直接用它打真的服務。
pub use views::images;
