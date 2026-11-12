//! agent／系統生成文字的共用渲染。
//!
//! 這些文字幾乎都是模型寫的 Markdown（Goal brief、task outcome、核准理由、
//! 總管備註……），所以一律經過 [`agent_text`]，不要各自用 `div().child(...)`
//! 當純文字輸出。這裡統一處理：
//!
//! - 元素 id 由呼叫端給（沿用各 view 既有的命名規則，例如
//!   `format!("msg-body-{}", id.0)`；同一畫面上必須唯一，否則兩個 TextView
//!   會共用同一份 keyed state）；
//! - `.selectable(true)`：使用者要能選取複製 agent 說的話；
//! - 字級與行高預設值（[`BODY_SIZE`]、[`LINE_HEIGHT`]）。窄欄位再用
//!   `Styled` 覆蓋：`TextView` 實作 `Styled`，`.text_size()`／`.line_height()`
//!   會 refine 到內部的 div，並經 `window.text_style()` 傳進 Markdown 排版。
//!
//! 顏色由呼叫端決定：gpui-component 安裝的 `TextViewDefaults` 帶
//! `inherit_text_color(true)`，所以外層 `div().text_color(...)` 就會決定內文顏色；
//! 也可以直接對回傳的 `TextView` 呼叫 `.text_color(...)`（淡色處例如
//! `theme::text_muted()`）。
//!
//! 串流中的訊息另外處理游標，見 [`agent_stream`]／[`streaming_markdown`]。

use gpui_kit::component::text::TextView;
use gpui_kit::{Div, ElementId, ParentElement as _, SharedString, Styled as _, div, px, relative};

use crate::theme;

/// 內文預設字級（一般密度的對話流）。
pub const BODY_SIZE: f32 = 15.;

/// 內文預設行高（相對於字級的倍數）。
pub const LINE_HEIGHT: f32 = 1.6;

/// 串流中的游標字元。
pub const CURSOR: &str = "▍";

/// 一段 agent 文字：Markdown 渲染、可選取、預設字級與行高。
///
/// `id` 必須在畫面內唯一。密度不同時（例如右欄 thread 的緊湊尺寸）用
/// `.text_size(px(...))` 覆蓋即可。
pub fn agent_text(id: impl Into<ElementId>, text: impl Into<SharedString>) -> TextView {
    TextView::markdown(id, text)
        .selectable(true)
        .text_size(px(BODY_SIZE))
        .line_height(relative(LINE_HEIGHT))
}

/// 串流中的 agent 文字：收尾未閉合的區塊後把游標接在後面。
pub fn agent_stream(id: impl Into<ElementId>, source: &str) -> TextView {
    agent_text(id, streaming_markdown(source))
}

/// 淡色的 agent 文字（13px）：卡片附註、核准理由、記憶觀察這類次要內文。
pub fn agent_text_muted(id: impl Into<ElementId>, text: impl Into<SharedString>) -> TextView {
    agent_text(id, text)
        .text_size(px(13.))
        .text_color(theme::text_muted())
}

/// 有固定前綴的一段文字：前綴維持純文字，`body` 那段 agent 原文走 Markdown。
///
/// 「需要你：」「失敗 N 次：」「你的理由：」這類前綴是平台自己加的字，不是 Markdown
/// 的一部分；只把後面的原文交給 Markdown 解析，前綴就不會因為鄰近的 `*`／`_` 被吃掉
/// 或誤判成語法。前綴與內文分成兩行（Markdown 是區塊層渲染，接不成同一行）。
///
/// 字級與顏色下在回傳的 `Div` 上：前綴吃得到，`body` 沒有自己指定顏色時也會沿用。
pub fn agent_text_prefixed(prefix: impl Into<SharedString>, body: TextView) -> Div {
    div()
        .flex()
        .flex_col()
        .child(div().child(prefix.into()))
        .child(body)
}

/// `facts` 裡夾帶使用者原話的那一條（「你的理由：…」）。
///
/// 核准與 Inbox 的 facts 是平台組的結構化欄位（PR、sha、動作、時間），本身是純文字，
/// 只有拒絕／回答時附上的理由是使用者自己寫的；那一條拆成「前綴（純文字）」與「原話
/// （Markdown）」。不是這一條就回 `None`，呼叫端照舊當 chip 或淡色文字畫。
pub fn fact_note(fact: &str) -> Option<(String, String)> {
    let (prefix, note) = fact.split_once("你的理由：")?;
    Some((format!("{prefix}你的理由："), note.to_string()))
}

/// 成員列表第二行的平台前綴：「需要你：」後面才是 worker 停下來的原因（agent 原文）。
pub fn status_note(status: &str) -> Option<(&'static str, &str)> {
    status
        .strip_prefix("需要你：")
        .map(|rest| ("需要你：", rest))
}

/// 把 Markdown 原文截到最多 `max_lines` 行，被截掉時在最後補「…」。
///
/// `TextView` 的 `max_lines` 只把版面高度夾住、不會畫省略號，所以截斷在原文這一層
/// 做（按原行數切，行內語法可能因此不完整，Markdown 會照剩下的字面渲染，不會丟字）。
/// 呼叫端通常兩者一起用：這裡給「…」，`max_lines` 保證渲染後的高度上限。
pub fn clamp_markdown(source: &str, max_lines: usize) -> String {
    let max_lines = max_lines.max(1);
    let mut lines: Vec<&str> = source.lines().collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    if lines.len() <= max_lines {
        return lines.join("\n");
    }
    let mut out = lines[..max_lines].join("\n");
    while out.ends_with([' ', '\t']) {
        out.pop();
    }
    out.push('…');
    out
}

/// 把串流中的 Markdown 原文補成可以安全接上游標的樣子。
///
/// 游標不能只是 `format!("{}▍", source)`：模型停在未閉合的 code fence 時，
/// 游標會被吸進程式碼區塊、變成程式碼的一部分；停在未閉合的行內 code span
/// 時也一樣（▍ 會變成 code 的字）。這裡分別處理：
///
/// - **未閉合的 fenced code block**：補一個等長的收尾 fence，游標放到區塊
///   **外面**的新段落。代價是游標暫時在區塊下方而不是緊接最後一行——這比
///   讓它被吃進區塊裡好，而且模型接下來的新內容就出現在那裡。
/// - **未閉合的行內 code span**（最後一段裡最後一串沒有等長收尾的反引號）：
///   補上等長的反引號，游標留在行內緊接其後，換行與行內對齊都不變。
/// - **GFM 表格列**：游標放到表格下方的新段落。接在列尾會變成多出來的 cell，
///   被表格丟掉（游標就暫時看不見了）。
/// - **其他**（未閉合的 `**粗體**`、`_斜體_`、沒寫完的連結……）：游標直接接在
///   原文後面。位置是對的，只是游標會暫時跟著那個樣式跑。
///
/// 已經閉合的區塊與 code span 完全不動，所以游標正常時輸出就是原文加游標。
pub fn streaming_markdown(source: &str) -> String {
    let mut out = String::with_capacity(source.len() + CURSOR.len() + 8);
    out.push_str(source);

    if let Some((fence, len)) = unclosed_fence(source) {
        // 收尾 fence 必須自己一行；游標隔一個空行，才不會被算回區塊裡。
        if !out.ends_with('\n') {
            out.push('\n');
        }
        for _ in 0..len {
            out.push(fence);
        }
        out.push_str("\n\n");
        out.push_str(CURSOR);
        return out;
    }

    if let Some(len) = unclosed_inline_code(source) {
        for _ in 0..len {
            out.push('`');
        }
        out.push_str(CURSOR);
        return out;
    }

    if last_line_is_table_row(source) {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
        out.push_str(CURSOR);
        return out;
    }

    out.push_str(CURSOR);
    out
}

/// 最後一行是不是 GFM 表格列（最多三個空白縮排後以 `|` 開頭）。
fn last_line_is_table_row(source: &str) -> bool {
    let last = source.rsplit('\n').find(|line| !line.trim().is_empty());
    let Some(line) = last else {
        return false;
    };
    let (indent, body) = strip_indent(line);
    indent <= 3 && body.starts_with('|')
}

/// 最多三個空白的縮排（CommonMark 的 fence 縮排上限）。回傳 `(縮排, 去縮排後)`。
fn strip_indent(line: &str) -> (usize, &str) {
    let indent = line.len() - line.trim_start_matches(' ').len();
    (indent, &line[indent..])
}

/// 這一行是不是開啟 fenced code block：`(fence 字元, 開頭長度)`。
///
/// 規則同 CommonMark：至少三個 `` ` `` 或 `~`；反引號 fence 的 info string
/// 不能再有反引號，否則那其實是一串行內 code。
fn opening_fence(line: &str) -> Option<(char, usize)> {
    let (indent, body) = strip_indent(line);
    if indent > 3 {
        return None;
    }
    let fence = body.chars().next()?;
    if fence != '`' && fence != '~' {
        return None;
    }
    let len = body.chars().take_while(|c| *c == fence).count();
    if len < 3 {
        return None;
    }
    if fence == '`' && body[len..].contains('`') {
        return None;
    }
    Some((fence, len))
}

/// 這一行是不是收尾 fence：同字元、不短於開頭、後面只能有空白。
fn closing_fence(line: &str, fence: char, len: usize) -> bool {
    let (indent, body) = strip_indent(line);
    if indent > 3 {
        return false;
    }
    let count = body.chars().take_while(|c| *c == fence).count();
    count >= len && body[count..].chars().all(|c| c == ' ')
}

/// 到結尾還沒收尾的 fenced code block：`(fence 字元, 開頭長度)`。
fn unclosed_fence(source: &str) -> Option<(char, usize)> {
    let mut open: Option<(char, usize)> = None;
    for line in source.split('\n') {
        match open {
            Some((fence, len)) => {
                if closing_fence(line, fence, len) {
                    open = None;
                }
            }
            None => {
                if let Some(opening) = opening_fence(line) {
                    open = Some(opening);
                }
            }
        }
    }
    open
}

/// 原文只留行內語法看得到的部分，換行保留（段落結構不變）。
///
/// 去掉 fenced code block 與縮排程式碼區塊（四空白縮排、前面是空行或接續上一行
/// 縮排程式碼）的內容——那幾行裡的反引號是字面文字，不是行內 code。
fn prose_lines(source: &str) -> String {
    let mut prose = String::with_capacity(source.len());
    let mut open: Option<(char, usize)> = None;
    let mut indented_code = false;
    let mut prev_blank = true;
    for line in source.split('\n') {
        if let Some((fence, len)) = open {
            if closing_fence(line, fence, len) {
                open = None;
            }
            prose.push('\n');
            continue;
        }
        if let Some(opening) = opening_fence(line) {
            open = Some(opening);
            prose.push('\n');
            prev_blank = false;
            continue;
        }
        if line.trim().is_empty() {
            indented_code = false;
            prev_blank = true;
            prose.push('\n');
            continue;
        }
        let (indent, _) = strip_indent(line);
        if indent >= 4 && (indented_code || prev_blank) {
            indented_code = true;
            prose.push('\n');
            continue;
        }
        indented_code = false;
        prev_blank = false;
        prose.push_str(line);
        prose.push('\n');
    }
    prose
}

/// 最後一個段落裡沒有收尾的行內 code span 的反引號長度。
///
/// 只看最後一個段落（最後一個空行之後）：更早的段落已被空行斷開，那邊的反引號
/// 不可能還開著一個跨段落的 code span。配對規則同 CommonMark——只有等長的
/// 反引號串會互相收尾；被反斜線轉義的不算。
fn unclosed_inline_code(source: &str) -> Option<usize> {
    let prose = prose_lines(source);
    let start = prose.rfind("\n\n").map_or(0, |ix| ix + "\n\n".len());
    let bytes = &prose.as_bytes()[start..];
    let mut open: Option<usize> = None;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'`' {
            i += 1;
            continue;
        }
        let mut backslashes = 0;
        let mut j = i;
        while j > 0 && bytes[j - 1] == b'\\' {
            backslashes += 1;
            j -= 1;
        }
        let mut run = 0;
        while i + run < bytes.len() && bytes[i + run] == b'`' {
            run += 1;
        }
        if backslashes % 2 == 0 {
            match open {
                None => open = Some(run),
                Some(len) if len == run => open = None,
                Some(_) => {}
            }
        }
        i += run;
    }
    open
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_gets_the_cursor_at_the_end() {
        assert_eq!(streaming_markdown("hello"), "hello▍");
        assert_eq!(streaming_markdown("hello\n"), "hello\n▍");
    }

    #[test]
    fn closed_fence_is_left_alone() {
        let source = "before\n\n```rust\nfn a() {}\n```\n\nafter";
        assert_eq!(streaming_markdown(source), format!("{source}▍"));
    }

    #[test]
    fn unclosed_fence_is_closed_and_the_cursor_goes_outside_it() {
        let source = "正在寫：\n\n```rust\nfn a() {\n";
        assert_eq!(
            streaming_markdown(source),
            "正在寫：\n\n```rust\nfn a() {\n```\n\n▍"
        );
    }

    #[test]
    fn unclosed_fence_without_trailing_newline_is_closed() {
        assert_eq!(streaming_markdown("```sh\nls"), "```sh\nls\n```\n\n▍");
    }

    #[test]
    fn longer_and_tilde_fences_close_with_their_own_length() {
        assert_eq!(streaming_markdown("````\nx"), "````\nx\n````\n\n▍");
        assert_eq!(streaming_markdown("~~~\nx"), "~~~\nx\n~~~\n\n▍");
    }

    #[test]
    fn four_space_indent_is_not_a_fence() {
        assert_eq!(streaming_markdown("    ```\nx"), "    ```\nx▍");
    }

    #[test]
    fn unclosed_inline_code_is_closed_before_the_cursor() {
        assert_eq!(streaming_markdown("用 `cargo"), "用 `cargo`▍");
        assert_eq!(streaming_markdown("用 ``a`b"), "用 ``a`b``▍");
    }

    #[test]
    fn closed_inline_code_is_left_alone() {
        assert_eq!(
            streaming_markdown("用 `cargo test` 跑"),
            "用 `cargo test` 跑▍"
        );
    }

    #[test]
    fn inline_code_span_across_lines_is_closed() {
        assert_eq!(streaming_markdown("`a\nb"), "`a\nb`▍");
    }

    #[test]
    fn inline_code_in_an_earlier_paragraph_does_not_count() {
        assert_eq!(
            streaming_markdown("`舊段落\n\n新段落"),
            "`舊段落\n\n新段落▍"
        );
    }

    #[test]
    fn backticks_inside_a_closed_fence_do_not_count() {
        let source = "```\nlet a = `x;\n```\n\n新段落";
        assert_eq!(streaming_markdown(source), format!("{source}▍"));
    }

    #[test]
    fn cursor_leaves_the_table_body_so_it_is_not_dropped() {
        let source = "| 欄位 | 值 |\n| --- | --- |\n| a | b |";
        assert_eq!(streaming_markdown(source), format!("{source}\n\n▍"));
    }

    #[test]
    fn cursor_stays_in_a_plain_paragraph_after_a_table() {
        let source = "| a | b |\n| --- | --- |\n\n說明";
        assert_eq!(streaming_markdown(source), format!("{source}▍"));
    }

    #[test]
    fn escaped_backtick_is_not_an_opening_delimiter() {
        assert_eq!(streaming_markdown("不是 \\`code"), "不是 \\`code▍");
    }

    #[test]
    fn short_markdown_is_left_alone_by_the_clamp() {
        assert_eq!(clamp_markdown("第一行\n第二行", 3), "第一行\n第二行");
        assert_eq!(clamp_markdown("第一行\n第二行\n第三行", 3), "第一行\n第二行\n第三行");
    }

    #[test]
    fn the_clamp_appends_an_ellipsis_to_the_last_kept_line() {
        assert_eq!(
            clamp_markdown("第一行\n第二行\n第三行\n第四行", 2),
            "第一行\n第二行…"
        );
    }

    #[test]
    fn the_clamp_drops_trailing_blank_lines_before_counting() {
        assert_eq!(clamp_markdown("第一行\n第二行\n\n\n", 2), "第一行\n第二行");
        // 只有空行也一樣：不會變成「…」。
        assert_eq!(clamp_markdown("\n\n", 2), "");
    }

    #[test]
    fn the_clamp_never_leaves_a_space_before_the_ellipsis() {
        assert_eq!(clamp_markdown("a\nb   \nc", 2), "a\nb…");
    }

    #[test]
    fn the_clamp_treats_zero_lines_as_one() {
        assert_eq!(clamp_markdown("a\nb", 0), "a…");
    }

    #[test]
    fn only_the_user_note_part_of_a_fact_is_split_out() {
        assert_eq!(
            fact_note("你的理由：先補測試"),
            Some(("你的理由：".to_string(), "先補測試".to_string()))
        );
        assert_eq!(
            fact_note("你申請 · 動作 abc · 你的理由：**不要**動 UI"),
            Some((
                "你申請 · 動作 abc · 你的理由：".to_string(),
                "**不要**動 UI".to_string()
            ))
        );
        assert_eq!(fact_note("PR #42"), None);
    }

    #[test]
    fn the_needs_you_prefix_is_kept_out_of_the_markdown() {
        assert_eq!(status_note("需要你：選按鈕樣式"), Some(("需要你：", "選按鈕樣式")));
        assert_eq!(status_note("等待 CI"), None);
    }
}
