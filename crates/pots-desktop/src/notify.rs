//! 桌面通知（`proposal/approvals-and-notifications.md#通知策略`）。
//!
//! 桌面版的服務是常駐的子行程、沒有自己的視窗，所以通知由客戶端用作業系統的通知（`notify-rust`）
//! 發出；要推播的事由 `sync` 判斷（新的核准、需要你處理的任務、Goal 完成），這裡依實例的通知設定
//! 決定送不送：
//!
//! - 關掉通知時都不送。
//! - 靜音時段內先累積，時段結束後合併成一則。
//! - 視窗在前景、而且正看著那個 project 時不送（你已經看到了）。
//!
//! 點通知的處理依平台而異（macOS 的 `NSUserNotificationCenter` 要阻塞主執行緒才收得到點擊），所以
//! 不依賴它：app 記下最後一則通知，視窗在之後幾分鐘內被叫到前景時開啟那一筆（`PotsApp`）。

use std::sync::{Arc, Mutex};

use pots_proto::NotificationSettings;

use crate::sync::Notice;

/// 實際送出通知的地方（測試換成 [`Recorder`]）。
pub trait Sender: Send + Sync {
    fn send(&self, title: &str, body: &str);
}

/// 作業系統的通知。送出可能要一點時間，在另一個執行緒做。
pub struct Os;

impl Sender for Os {
    fn send(&self, title: &str, body: &str) {
        let (title, body) = (title.to_string(), body.to_string());
        std::thread::spawn(move || {
            #[cfg(target_os = "macos")]
            set_macos_application();
            let result = notify_rust::Notification::new()
                .appname("Pots")
                .summary(&title)
                .body(&body)
                .show();
            if let Err(e) = result {
                tracing::warn!(error = %e, "無法送出桌面通知");
            }
        });
    }
}

/// macOS 的通知要掛在某個 app 的 bundle id 底下（`appname` 在 macOS 沒有作用）。沒先設定的話，
/// `mac-notification-sys` 第一次送通知時會跑 AppleScript `get id of application "use_default"`，
/// 找不到這個 app，系統就跳出「Where is use_default?」的選擇 App 視窗。桌面版是沒有 bundle 的
/// 執行檔，所以直接指定它原本的退路 Finder，跳過那段查詢。
#[cfg(target_os = "macos")]
fn set_macos_application() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if let Err(e) = notify_rust::set_application("com.apple.Finder") {
            tracing::warn!(error = %e, "無法設定送出通知的 app");
        }
    });
}

/// 只記下來（測試與截圖用）。
#[derive(Default)]
pub struct Recorder(pub Mutex<Vec<(String, String)>>);

impl Sender for Recorder {
    fn send(&self, title: &str, body: &str) {
        self.0
            .lock()
            .unwrap()
            .push((title.to_string(), body.to_string()));
    }
}

/// 本機時間是一天裡的第幾分鐘。
pub fn local_minute() -> u32 {
    use chrono::Timelike as _;
    let now = chrono::Local::now();
    now.hour() * 60 + now.minute()
}

pub struct Notifier {
    sender: Arc<dyn Sender>,
    /// 靜音時段累積的。
    held: Vec<Notice>,
}

impl Notifier {
    pub fn new(sender: Arc<dyn Sender>) -> Self {
        Self {
            sender,
            held: Vec::new(),
        }
    }

    /// 一件可能要推播的事。`focused` 是視窗在前景、而且正看著它的 project。回傳有沒有送出。
    pub fn notice(
        &mut self,
        n: Notice,
        settings: &NotificationSettings,
        minute: u32,
        focused: bool,
    ) -> bool {
        if !settings.enabled {
            return false;
        }
        if settings.is_quiet_at(minute) {
            self.held.push(n);
            return false;
        }
        if focused {
            return false;
        }
        self.sender.send(&n.title, &shorten(&n.body));
        true
    }

    /// 定期呼叫：靜音時段結束（或通知被關掉、靜音被取消）時，把累積的合併成一則送出。回傳合併後的
    /// 那一則（點了要開的是第一件）。
    pub fn tick(&mut self, settings: &NotificationSettings, minute: u32) -> Option<Notice> {
        if self.held.is_empty() || settings.is_quiet_at(minute) {
            return None;
        }
        let held = std::mem::take(&mut self.held);
        if !settings.enabled {
            return None;
        }
        let first = held[0].clone();
        let mut titles: Vec<&str> = held.iter().take(3).map(|n| n.title.as_str()).collect();
        if held.len() > 3 {
            titles.push("…");
        }
        let merged = Notice {
            title: format!("靜音期間有 {} 件事需要你", held.len()),
            body: titles.join("、"),
            ..first
        };
        self.sender.send(&merged.title, &shorten(&merged.body));
        Some(merged)
    }

    /// 靜音期間累積了幾件。
    pub fn held(&self) -> usize {
        self.held.len()
    }
}

/// 通知只帶一句摘要（不放 diff 或長內容）。
fn shorten(text: &str) -> String {
    let one: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > 140 {
        let mut s: String = one.chars().take(140).collect();
        s.push('…');
        s
    } else {
        one
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pots_proto::{InstanceId, ProjectId};

    fn notice(title: &str) -> Notice {
        Notice {
            instance: InstanceId::from_u64(1),
            project: Some(ProjectId::from_u64(2)),
            item: None,
            title: title.into(),
            body: "內容\n第二行".into(),
        }
    }

    #[test]
    fn quiet_hours_hold_and_merge() {
        let rec = Arc::new(Recorder::default());
        let mut n = Notifier::new(rec.clone());
        let mut s = NotificationSettings::default();
        assert!(n.notice(notice("a"), &s, 600, false));
        assert!(!n.notice(notice("b"), &s, 600, true), "正看著就不送");
        assert_eq!(
            rec.0.lock().unwrap().clone(),
            vec![("a".to_string(), "內容 第二行".to_string())]
        );

        s.quiet = true; // 22:00–08:00
        assert!(!n.notice(notice("c"), &s, 23 * 60, false));
        assert!(!n.notice(notice("d"), &s, 2 * 60, false));
        assert_eq!(n.held(), 2);
        assert!(n.tick(&s, 3 * 60).is_none(), "還在靜音");
        let merged = n.tick(&s, 8 * 60).unwrap();
        assert_eq!(merged.title, "靜音期間有 2 件事需要你");
        assert_eq!(merged.body, "c、d");
        assert_eq!(n.held(), 0);
        assert!(n.tick(&s, 9 * 60).is_none());

        s.enabled = false;
        assert!(!n.notice(notice("e"), &s, 600, false));
        assert_eq!(rec.0.lock().unwrap().len(), 2);
    }
}
