//! 任務 thread 的「看 diff」「看 trace」對話框。
//!
//! diff 是任務分支相對基準分支的改動（服務從自己的鏡像讀，worker 交出 commit 之後才有）；
//! trace 是這個任務每一次 run（worker 與它叫的 subagent）的模型輪、模型說的話與工具呼叫。

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, Div, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, div, px, relative,
};

use super::text::agent_text;
use super::usage::fmt_tokens;
use super::widgets::{link_button, tab};
use crate::app::{PotsApp, TaskInspect};
use crate::model::*;
use crate::theme;

/// diff 最多畫幾行；再多的請用 PR 或在本機看。
const MAX_DIFF_LINES: usize = 4000;
/// 收起來的工具步驟只顯示這麼多字。
const STEP_PREVIEW_CHARS: usize = 160;
/// 展開的工具步驟最多顯示這麼多字（trace 本身已經截短過，這是保險）。
const STEP_FULL_CHARS: usize = 6000;

/// 截成 `max` 個字，多的換成「…」；換行換成空白。
fn preview(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = flat.chars().take(max).collect();
    if flat.chars().count() > max {
        out.push('…');
    }
    out
}

fn clip(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push_str("\n…（已截短）");
    }
    out
}

/// 已換成本地時間的 `Timestamp` 顯示成 `14:03:27`。
fn clock(at: Timestamp) -> String {
    chrono::DateTime::from_timestamp(at.0, 0)
        .map(|t| t.format("%H:%M:%S").to_string())
        .unwrap_or_default()
}

fn muted(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(12.))
        .text_color(theme::text_muted())
        .child(text.into())
}

fn agent_label(agent: &str) -> String {
    match agent {
        "worker" => "Worker".into(),
        "explorer" => "Explorer subagent".into(),
        "reviewer" => "Reviewer subagent".into(),
        "env" | "env_setup" => "環境 worker".into(),
        other => other.into(),
    }
}

fn run_state_label(state: &str) -> (&'static str, gpui_kit::Hsla) {
    match state {
        "done" => ("完成", theme::ok()),
        "failed" => ("失敗", theme::danger()),
        "cancelled" => ("已取消", theme::text_muted()),
        "running" => ("執行中", theme::accent()),
        "queued" => ("排隊中", theme::text_muted()),
        _ => ("", theme::text_muted()),
    }
}

impl PotsApp {
    pub fn render_task_inspect(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let inspect = self.task_inspect.as_ref()?;
        let task = inspect.task();
        let is_diff = inspect.is_diff();
        let found = self.store.task(self.instance, task);
        let title = found
            .map(|(_, t)| t.title.clone())
            .unwrap_or_else(|| "任務".into());
        let pr_url = found.and_then(|(_, t)| t.pr_url.clone());

        let header = div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .px(px(20.))
            .pt(px(16.))
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(16.))
                            .font_weight(FontWeight::BOLD)
                            .child(title),
                    )
                    .when_some(pr_url, |d, url| {
                        d.child(
                            link_button("task-inspect-pr", "開 PR")
                                .text_size(px(13.))
                                .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&url))),
                        )
                    })
                    .child(
                        link_button("task-inspect-refresh", "重新整理")
                            .text_size(px(13.))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.request_task_inspect(task, is_diff, cx)
                            })),
                    )
                    .child(
                        link_button("task-inspect-close", "關閉")
                            .text_size(px(13.))
                            .on_click(cx.listener(|this, _, _, cx| this.close_task_inspect(cx))),
                    ),
            )
            .child(self.inspect_subtitle(inspect))
            .child(
                div()
                    .flex()
                    .gap(px(18.))
                    .mt(px(4.))
                    .child(
                        tab("task-inspect-tab-diff", "Diff", is_diff).on_click(cx.listener(
                            move |this, _, _, cx| {
                                if !is_diff {
                                    this.request_task_inspect(task, true, cx)
                                }
                            },
                        )),
                    )
                    .child(
                        tab("task-inspect-tab-trace", "Trace", !is_diff).on_click(cx.listener(
                            move |this, _, _, cx| {
                                if is_diff {
                                    this.request_task_inspect(task, false, cx)
                                }
                            },
                        )),
                    ),
            );

        let body = match inspect {
            TaskInspect::Loading { diff, .. } => div()
                .p(px(20.))
                .child(muted(if *diff {
                    "正在讀取任務分支…"
                } else {
                    "正在讀取 trace…"
                }))
                .into_any_element(),
            TaskInspect::Diff(d) => self.render_diff_body(d, cx).into_any_element(),
            TaskInspect::Trace(t) => self.render_trace_body(t, cx).into_any_element(),
        };

        let dialog = div()
            .id("task-inspect-dialog")
            .w(px(960.))
            .max_w(relative(0.92))
            .h(relative(0.86))
            .flex()
            .flex_col()
            .rounded(px(12.))
            .bg(theme::white())
            .shadow_lg()
            .overflow_hidden()
            .child(header)
            .child(
                div()
                    .id("task-inspect-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(body),
            )
            .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
                cx.stop_propagation()
            })
            .on_click(|_, _, cx| cx.stop_propagation());
        Some(
            div()
                .id("task-inspect-backdrop")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui_kit::hsla(0., 0., 0., 0.35))
                .on_click(cx.listener(|this, _, _, cx| this.close_task_inspect(cx)))
                .child(dialog)
                .into_any_element(),
        )
    }

    fn inspect_subtitle(&self, inspect: &TaskInspect) -> Div {
        let text = match inspect {
            TaskInspect::Loading { .. } => String::new(),
            TaskInspect::Diff(d) => {
                let mut parts = vec![];
                if !d.branch.is_empty() {
                    if d.base.is_empty() {
                        parts.push(d.branch.clone());
                    } else {
                        parts.push(format!("{} → {}", d.branch, d.base));
                    }
                }
                if let Some(sha) = &d.sha {
                    parts.push(sha.chars().take(7).collect());
                }
                parts.join(" · ")
            }
            TaskInspect::Trace(t) => {
                let steps: usize = t.runs.iter().map(|r| r.steps.len()).sum();
                let input: u64 = t.runs.iter().map(|r| r.input_tokens).sum();
                let output: u64 = t.runs.iter().map(|r| r.output_tokens).sum();
                format!(
                    "{} 次 run · {steps} 個步驟 · 輸入 {} / 輸出 {} tokens",
                    t.runs.len(),
                    fmt_tokens(input),
                    fmt_tokens(output)
                )
            }
        };
        muted(text)
    }

    fn render_diff_body(&self, d: &TaskDiffView, cx: &mut Context<Self>) -> Div {
        let body = div().flex().flex_col().gap(px(12.)).p(px(20.));
        if d.sha.is_none() {
            return body.child(muted(if d.branch.is_empty() {
                "這個任務還沒有分支：worker 開始做之後才會建立。"
            } else {
                "worker 還沒交出 commit。任務分支在 worker 收工或停下等待時才從容器取回，\
                 之後這裡就看得到改動。"
            }));
        }
        if d.diff.trim().is_empty() {
            return body.child(muted("任務分支和基準分支沒有差異。"));
        }
        let mono = cx.theme().mono_font_family.clone();
        let lines: Vec<&str> = d.diff.lines().collect();
        let hidden = lines.len().saturating_sub(MAX_DIFF_LINES);
        body.when(!d.stat.trim().is_empty(), |b| {
            b.child(
                div()
                    .px(px(12.))
                    .py(px(8.))
                    .rounded(px(6.))
                    .bg(theme::subtle_bg())
                    .border_1()
                    .border_color(theme::border())
                    .font_family(mono.clone())
                    .text_size(px(12.))
                    .line_height(relative(1.45))
                    .text_color(theme::text_muted())
                    .children(d.stat.lines().map(|l| div().child(l.to_string()))),
            )
        })
        .child(
            div()
                .flex()
                .flex_col()
                .rounded(px(6.))
                .border_1()
                .border_color(theme::border())
                .overflow_hidden()
                .font_family(mono)
                .text_size(px(12.))
                .line_height(relative(1.45))
                .children(lines.into_iter().take(MAX_DIFF_LINES).filter_map(diff_line)),
        )
        .when(hidden > 0 || d.truncated, |b| {
            b.child(muted(if d.truncated {
                "diff 太長，後面的部分沒有顯示；完整內容請開 PR 或在本機看任務分支。".to_string()
            } else {
                format!("還有 {hidden} 行沒有顯示；完整內容請開 PR 或在本機看任務分支。")
            }))
        })
    }

    fn render_trace_body(&self, t: &TaskTraceView, cx: &mut Context<Self>) -> Div {
        let body = div().flex().flex_col().gap(px(16.)).p(px(20.));
        if t.runs.is_empty() {
            return body.child(muted("這個任務還沒有 run。"));
        }
        let mono = cx.theme().mono_font_family.clone();
        body.children(t.runs.iter().enumerate().map(|(ri, run)| {
            let (state, state_color) = run_state_label(&run.state);
            let range = match run.finished_at {
                Some(end) => format!("{} – {}", clock(run.started_at), clock(end)),
                None => format!("{} 開始", clock(run.started_at)),
            };
            let head = div()
                .flex()
                .items_center()
                .flex_wrap()
                .gap(px(8.))
                .child(
                    div()
                        .text_size(px(14.))
                        .font_weight(FontWeight::BOLD)
                        .child(agent_label(&run.agent)),
                )
                .when(!state.is_empty(), |d| {
                    d.child(
                        div()
                            .text_size(px(12.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(state_color)
                            .child(state),
                    )
                })
                .child(muted(format!(
                    "{} · {range} · 輸入 {} / 輸出 {} tokens",
                    run.model.clone().unwrap_or_else(|| "未知模型".into()),
                    fmt_tokens(run.input_tokens),
                    fmt_tokens(run.output_tokens)
                )));
            // 模型輪只有 token 數，總數已經在標題列：不逐列顯示。
            let steps = div().flex().flex_col().gap(px(2.)).children(
                run.steps
                    .iter()
                    .enumerate()
                    .filter(|(_, step)| step.kind != "model")
                    .map(|(si, step)| self.trace_step(ri, si, step, mono.clone(), cx)),
            );
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(head)
                .when_some(run.error.clone(), |d, e| {
                    d.child(
                        div()
                            .text_size(px(13.))
                            .text_color(theme::danger())
                            .child(e),
                    )
                })
                .when(run.steps.is_empty(), |d| {
                    d.child(muted("這次 run 沒有留下 trace。"))
                })
                .child(steps)
        }))
    }

    fn trace_step(
        &self,
        ri: usize,
        si: usize,
        step: &TraceStepView,
        mono: SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let time = div()
            .flex_none()
            .w(px(62.))
            .text_size(px(12.))
            .text_color(theme::text_muted())
            .font_family(mono.clone())
            .child(clock(step.at));
        if step.kind == "text" {
            // 模型在工具呼叫之間說的話：照一般內文顯示成 Markdown。
            return div()
                .flex()
                .gap(px(8.))
                .py(px(4.))
                .child(time)
                .child(
                    div().flex_1().min_w_0().child(
                        agent_text(
                            SharedString::from(format!("trace-text-{ri}-{si}")),
                            step.detail.clone(),
                        )
                        .text_size(px(13.))
                        .line_height(relative(1.5)),
                    ),
                )
                .into_any_element();
        }
        let key = (ri, si);
        let expanded = self.expanded_trace.contains(&key);
        let (label, color) = match step.kind.as_str() {
            "model" => (format!("模型 · {}", step.name), theme::text_muted()),
            _ if !step.ok => (format!("{}（失敗）", step.name), theme::danger()),
            _ => (step.name.clone(), theme::accent_text()),
        };
        let tool = ToolDetail::parse(&step.detail);
        div()
            .id(SharedString::from(format!("trace-step-{ri}-{si}")))
            .flex()
            .gap(px(8.))
            .px(px(6.))
            .py(px(3.))
            .rounded(px(4.))
            .cursor_pointer()
            .hover(|s| s.bg(theme::subtle_bg()))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_trace_step(key, cx)))
            .child(time)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .text_size(px(13.))
                            .child(
                                div()
                                    .flex_none()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(color)
                                    .child(label),
                            )
                            .when(!expanded, |d| {
                                d.child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(px(12.))
                                        .text_color(theme::text_muted())
                                        .child(preview(&tool.summary(), STEP_PREVIEW_CHARS)),
                                )
                            }),
                    )
                    .when(expanded, |d| {
                        let block = |label: &'static str, text: String| {
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(2.))
                                .child(muted(label))
                                .child(
                                    div()
                                        .px(px(10.))
                                        .py(px(6.))
                                        .rounded(px(4.))
                                        .bg(theme::subtle_bg())
                                        .font_family(mono.clone())
                                        .text_size(px(12.))
                                        .line_height(relative(1.45))
                                        .text_color(theme::text())
                                        .child(clip(&text, STEP_FULL_CHARS)),
                                )
                        };
                        match (&tool.args, &tool.output) {
                            (None, None) => d.child(block("內容", pretty(&step.detail))),
                            (args, output) => d
                                .when_some(args.clone(), |d, a| d.child(block("參數", a)))
                                .when_some(output.clone(), |d, o| d.child(block("結果", o))),
                        }
                    }),
            )
            .into_any_element()
    }
}

/// 工具步驟的 detail（`{call_id, arguments, output}`，`arguments` 本身是 JSON 字串）拆成參數與結果。
struct ToolDetail {
    /// 排版過的參數。
    args: Option<String>,
    args_value: Option<serde_json::Value>,
    output: Option<String>,
}

impl ToolDetail {
    fn parse(detail: &str) -> Self {
        let value: serde_json::Value = serde_json::from_str(detail).unwrap_or_default();
        let args_value = match value.get("arguments") {
            Some(serde_json::Value::String(raw)) => {
                Some(serde_json::from_str(raw).unwrap_or_else(|_| raw.clone().into()))
            }
            Some(other) => Some(other.clone()),
            None => None,
        };
        let args = args_value.as_ref().map(|v| match v {
            serde_json::Value::String(s) => s.clone(),
            v => serde_json::to_string_pretty(v).unwrap_or_default(),
        });
        let output = value.get("output").map(|o| match o {
            serde_json::Value::String(s) => s.clone(),
            o => serde_json::to_string_pretty(o).unwrap_or_default(),
        });
        Self {
            args,
            args_value,
            output,
        }
    }

    /// 收起時的一行：參數裡最能說明在做什麼的欄位（指令、路徑、搜尋字串…），沒有就是整份參數。
    fn summary(&self) -> String {
        const KEYS: [&str; 8] = [
            "command", "path", "pattern", "title", "text", "query", "summary", "task_id",
        ];
        if let Some(obj) = self.args_value.as_ref().and_then(|v| v.as_object()) {
            if let Some(s) = KEYS
                .iter()
                .find_map(|k| obj.get(*k).and_then(|v| v.as_str()))
            {
                return s.to_string();
            }
            if obj.is_empty() {
                return self.output.clone().unwrap_or_default();
            }
            return serde_json::Value::Object(obj.clone()).to_string();
        }
        self.args
            .clone()
            .or_else(|| self.output.clone())
            .unwrap_or_default()
    }
}

/// trace 的 detail 多半是一行 JSON；展開時排版成多行好讀。
fn pretty(detail: &str) -> String {
    serde_json::from_str::<serde_json::Value>(detail)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| detail.to_string())
}

/// diff 的一行。`index` 行不顯示；檔案標頭換成一列檔名。
fn diff_line(line: &str) -> Option<Div> {
    if line.starts_with("index ")
        || line.starts_with("--- ")
        || line.starts_with("+++ ")
        || line.starts_with("new file mode")
        || line.starts_with("deleted file mode")
    {
        return None;
    }
    let row = div().px(px(10.));
    Some(if let Some(rest) = line.strip_prefix("diff --git ") {
        // `a/path b/path`：取 b 那邊的路徑。
        let path = rest.rsplit_once(" b/").map_or(rest, |(_, b)| b).to_string();
        row.py(px(4.))
            .bg(theme::track())
            .border_t_1()
            .border_color(theme::border())
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme::text())
            .child(path)
    } else if line.starts_with("@@") {
        row.bg(theme::accent_soft())
            .text_color(theme::accent_text())
            .child(line.to_string())
    } else if line.starts_with('+') {
        row.bg(theme::ok_soft())
            .text_color(theme::ok())
            .child(line.to_string())
    } else if line.starts_with('-') {
        row.bg(theme::danger_soft())
            .text_color(theme::danger())
            .child(line.to_string())
    } else {
        row.text_color(theme::text()).child(line.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::{ToolDetail, pretty, preview};

    #[test]
    fn preview_flattens_and_cuts() {
        assert_eq!(preview("a\n  b", 10), "a b");
        assert_eq!(preview("abcdef", 3), "abc…");
    }

    #[test]
    fn tool_detail_splits_arguments_and_output() {
        let t = ToolDetail::parse(
            r#"{"call_id":"c1","arguments":"{\"command\":\"cargo test\"}","output":"ok\n1 passed","seq":3}"#,
        );
        assert_eq!(t.summary(), "cargo test");
        assert_eq!(
            t.args.as_deref(),
            Some("{\n  \"command\": \"cargo test\"\n}")
        );
        assert_eq!(t.output.as_deref(), Some("ok\n1 passed"));
        let empty = ToolDetail::parse(r#"{"call_id":"c2","arguments":"{}","output":"now"}"#);
        assert_eq!(empty.summary(), "now");
    }

    #[test]
    fn pretty_formats_json_only() {
        assert_eq!(pretty("{\"a\":1}"), "{\n  \"a\": 1\n}");
        assert_eq!(pretty("plain"), "plain");
    }
}
