//! token 數的估算，給 Observational Memory 的閾值用。
//!
//! 不依賴任何 tokenizer，各家 provider 一致：CJK（漢字、假名、諺文、全形標點）一個字約一個
//! token，其他字元約四個一個 token。對 OpenAI 與 Anthropic 都只是近似，閾值本來就是粗估。

use pots_llm::Item;

/// 每筆歷史項目額外算的 token（角色標記等）。
pub const ITEM_OVERHEAD: u64 = 4;

/// 一段文字的估計 token 數。
pub fn estimate(text: &str) -> u64 {
    let mut wide = 0u64;
    let mut other = 0u64;
    for c in text.chars() {
        if is_wide(c) {
            wide += 1;
        } else {
            other += 1;
        }
    }
    wide + other.div_ceil(4)
}

/// 一筆歷史項目的估計 token 數。
pub fn item(item: &Item) -> u64 {
    ITEM_OVERHEAD
        + match item {
            Item::User { text } | Item::Assistant { text } => estimate(text),
            Item::ToolCall(call) => estimate(&call.name) + estimate(&call.arguments),
            Item::ToolResult { output, .. } => estimate(output),
            Item::Opaque { value, .. } => estimate(&value.to_string()),
        }
}

fn is_wide(c: char) -> bool {
    matches!(
        u32::from(c),
        0x1100..=0x11FF       // 諺文字母
        | 0x2E80..=0x2FDF     // CJK 部首
        | 0x3000..=0x303F     // CJK 標點
        | 0x3040..=0x30FF     // 平假名、片假名
        | 0x3100..=0x31FF     // 注音等
        | 0x3200..=0x4DBF     // 圈字、擴充 A
        | 0x4E00..=0x9FFF     // 基本漢字
        | 0xA960..=0xA97F
        | 0xAC00..=0xD7FF     // 諺文音節
        | 0xF900..=0xFAFF     // 相容漢字
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFFEF     // 全形
        | 0x20000..=0x3FFFF // 擴充 B 之後
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_text() {
        assert_eq!(estimate(""), 0);
        assert_eq!(estimate("abcd"), 1);
        assert_eq!(estimate("abcde"), 2);
        assert_eq!(estimate("你好，世界"), 5);
        assert_eq!(estimate("用 SQLite"), 1 + 2);
        assert_eq!(
            item(&Item::User {
                text: "你好".into()
            }),
            ITEM_OVERHEAD + 2
        );
    }
}
