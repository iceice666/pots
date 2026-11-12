//! 圖片附件的本機檔案位置。
//!
//! 歷史裡的 [`pots_llm::Item::Image`] 只存附件參照（attachment id + MIME）；檔案本體放在
//! 資料目錄的 `attachments/` 下、檔名就是 attachment id，base64 只在送出模型請求的那一刻
//! 由 [`pots_llm::image::DirectoryImages`] 讀出來（**不進資料庫、`om_raw` 或 events 日誌**）。
//!
//! 這個目錄是 task 2（pots-llm）與附件上傳端點之間的契約：上傳端點把 bytes 寫成
//! `{data_dir}/attachments/{attachment_id}`，這裡就讀得到。上傳端點的實作另外負責去重與上限。

use std::path::{Path, PathBuf};

/// 資料目錄下放附件的子目錄。
pub const DIR: &str = "attachments";

/// 附件目錄。`data_dir` 是服務的資料目錄（`Config::data_dir`）。
pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join(DIR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachments_live_under_the_data_dir() {
        assert_eq!(
            dir(Path::new("/data/pots")),
            PathBuf::from("/data/pots/attachments")
        );
    }
}
