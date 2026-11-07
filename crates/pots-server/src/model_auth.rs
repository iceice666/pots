//! 模型 provider 授權的控制介面。
//!
//! 伺服器只透過 [`ModelAuthControl`] 操作授權，不知道背後是 SIWC 還是其他 provider；
//! 實際的 `pots_llm::siwc::SiwcAuth` 由組裝端（桌面 app 或 bin）包成這個 trait 傳進來。
//! 狀態一律是 [`ModelAuthView`]，不含任何 token。

use std::sync::Mutex;

use pots_llm::LlmError;
use pots_proto::ModelAuthView;
use tokio::sync::watch;

#[async_trait::async_trait]
pub trait ModelAuthControl: Send + Sync {
    /// 目前狀態與之後的變化。伺服器把每次變化記成 `ModelAuth` 事件推給客戶端。
    fn status(&self) -> watch::Receiver<ModelAuthView>;

    /// 開始瀏覽器登入，回傳要開啟的網址。實作要在回傳前把狀態設成
    /// [`ModelAuthView::Pending`]，客戶端靠狀態事件拿到網址。
    async fn start_login(&self) -> Result<String, LlmError>;

    async fn cancel_login(&self);

    async fn logout(&self) -> Result<(), LlmError>;
}

/// 假的授權控制，給測試與尚未接上 SIWC 的 bin 用。
///
/// `start_login` 只把狀態設成 `Pending { url }`，不會真的登入；測試可以用
/// [`set`](Self::set) 模擬登入完成或失效。
pub struct FakeModelAuth {
    tx: watch::Sender<ModelAuthView>,
    login_url: String,
    last: Mutex<ModelAuthView>,
}

impl FakeModelAuth {
    pub fn new(initial: ModelAuthView) -> Self {
        let (tx, _) = watch::channel(initial.clone());
        Self {
            tx,
            login_url: "https://example.invalid/pots-fake-login".into(),
            last: Mutex::new(initial),
        }
    }

    pub fn signed_out() -> Self {
        Self::new(ModelAuthView::SignedOut)
    }

    pub fn signed_in() -> Self {
        Self::new(ModelAuthView::SignedIn { account: None })
    }

    pub fn set(&self, view: ModelAuthView) {
        *self.last.lock().unwrap() = view.clone();
        self.tx.send_replace(view);
    }

    pub fn current(&self) -> ModelAuthView {
        self.last.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl ModelAuthControl for FakeModelAuth {
    fn status(&self) -> watch::Receiver<ModelAuthView> {
        self.tx.subscribe()
    }

    async fn start_login(&self) -> Result<String, LlmError> {
        self.set(ModelAuthView::Pending {
            url: self.login_url.clone(),
        });
        Ok(self.login_url.clone())
    }

    async fn cancel_login(&self) {
        if matches!(self.current(), ModelAuthView::Pending { .. }) {
            self.set(ModelAuthView::SignedOut);
        }
    }

    async fn logout(&self) -> Result<(), LlmError> {
        self.set(ModelAuthView::SignedOut);
        Ok(())
    }
}
