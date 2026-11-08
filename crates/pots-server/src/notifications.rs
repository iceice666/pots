//! 通知設定（`proposal/approvals-and-notifications.md#通知策略`）。
//!
//! 桌面版的通知由客戶端用作業系統的通知發出（服務是常駐的子行程，沒有自己的視窗）；設定存在實例裡，
//! 客戶端從快照與 `Notifications` 事件拿到。哪些事要推播、靜音時段怎麼合併也由客戶端依這份設定決定。

use pots_proto::{NotificationSettings, ServerEvent, parse_hh_mm};
use sqlx::sqlite::SqliteConnection;

use crate::Error;
use crate::hub::LogTxn;
use crate::store::Store;

/// `instance_settings` 的 key。
const SETTING_KEY: &str = "notifications";

pub async fn load(conn: &mut SqliteConnection) -> Result<NotificationSettings, Error> {
    let text: Option<String> =
        sqlx::query_scalar("SELECT value FROM instance_settings WHERE key = ?")
            .bind(SETTING_KEY)
            .fetch_optional(conn)
            .await?;
    Ok(text
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default())
}

/// 檢查並整理（時間寫成 `HH:MM`）；不合法時回說明。
pub fn check(s: &NotificationSettings) -> Result<NotificationSettings, String> {
    let fmt = |what: &str, v: &str| {
        parse_hh_mm(v)
            .map(|m| format!("{:02}:{:02}", m / 60, m % 60))
            .ok_or_else(|| format!("{what}要是 HH:MM（例如 22:00）"))
    };
    let start = fmt("靜音開始時間", &s.quiet_start)?;
    let end = fmt("靜音結束時間", &s.quiet_end)?;
    if s.quiet && start == end {
        return Err("靜音的開始與結束時間不能一樣".into());
    }
    Ok(NotificationSettings {
        enabled: s.enabled,
        quiet: s.quiet,
        quiet_start: start,
        quiet_end: end,
    })
}

/// 儲存並記一筆 `Notifications`。
pub async fn save(txn: &mut LogTxn<'_>, settings: &NotificationSettings) -> Result<(), Error> {
    Store::set_setting(txn.conn(), SETTING_KEY, &serde_json::to_string(settings)?).await?;
    let settings = settings.clone();
    txn.push(|seq| ServerEvent::Notifications { seq, settings })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_are_checked() {
        let mut s = NotificationSettings {
            quiet: true,
            quiet_start: "7:30".into(),
            ..Default::default()
        };
        assert_eq!(check(&s).unwrap().quiet_start, "07:30");
        s.quiet_end = "25:00".into();
        assert!(check(&s).is_err());
        s.quiet_end = "07:30".into();
        assert!(check(&s).is_err(), "開始等於結束");
        s.quiet = false;
        assert!(check(&s).is_ok());
    }
}
