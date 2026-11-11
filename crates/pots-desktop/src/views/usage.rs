//! 用量：目前實例所有 project 的 token 用量與估計金額（`ClientCommand::CostReport`）。
//!
//! Goal 預設不限預算、每小時熔斷預設關閉，這一頁讓你先觀察實際用量，再決定要不要設上限。

use std::collections::HashMap;

use gpui_kit::assets::IconName;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, Div, FontWeight, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, div, px, relative,
};

use super::channel::empty_state;
use super::widgets::*;
use crate::app::{PotsApp, UsageRange};
use crate::model::*;
use crate::theme;

/// 一個分組（project、模型、Goal、agent）的彙總。
#[derive(Clone, Debug, PartialEq)]
pub struct UsageGroup {
    pub label: String,
    /// 第二行的小字，例如 provider 或 project 名稱。
    pub detail: String,
    pub runs: u64,
    pub tokens: u64,
    /// 估計金額加上 CLI 回報的金額；都沒有時是 `None`。
    pub cost_usd: Option<f64>,
}

/// 依 `key` 分組加總，token 多的在前；`key` 回 `None` 的列不算。
pub fn group_rows(
    rows: &[CostRow],
    key: impl Fn(&CostRow) -> Option<(String, String)>,
) -> Vec<UsageGroup> {
    let mut order: Vec<(String, String)> = Vec::new();
    let mut groups: HashMap<(String, String), UsageGroup> = HashMap::new();
    for row in rows {
        let Some(k) = key(row) else { continue };
        let g = groups.entry(k.clone()).or_insert_with(|| {
            order.push(k.clone());
            UsageGroup {
                label: k.0.clone(),
                detail: k.1.clone(),
                runs: 0,
                tokens: 0,
                cost_usd: None,
            }
        });
        g.runs += row.runs;
        g.tokens += row.input_tokens + row.output_tokens;
        let cost = row.estimated_cost_usd.unwrap_or(0.) + row.reported_cost_usd.unwrap_or(0.);
        if row.estimated_cost_usd.is_some() || row.reported_cost_usd.is_some() {
            g.cost_usd = Some(g.cost_usd.unwrap_or(0.) + cost);
        }
    }
    let mut out: Vec<UsageGroup> = order
        .into_iter()
        .filter_map(|k| groups.remove(&k))
        .collect();
    // 穩定排序：token 一樣時保留原本的順序。
    out.sort_by_key(|g| std::cmp::Reverse(g.tokens));
    out
}

/// token 數用「萬／億」顯示：`8,200`、`12.3 萬`、`1.25 億`。
pub fn fmt_tokens(n: u64) -> String {
    if n >= 100_000_000 {
        format!("{:.2} 億", n as f64 / 100_000_000.)
    } else if n >= 10_000 {
        format!("{:.1} 萬", n as f64 / 10_000.)
    } else {
        let s = n.to_string();
        let mut out = String::new();
        for (i, ch) in s.chars().enumerate() {
            if i > 0 && (s.len() - i) % 3 == 0 {
                out.push(',');
            }
            out.push(ch);
        }
        out
    }
}

pub fn fmt_usd(v: f64) -> String {
    if v > 0. && v < 0.01 {
        "< $0.01".into()
    } else {
        format!("${v:.2}")
    }
}

fn fmt_duration(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs} 秒"),
        60..3600 => format!("{} 分鐘", secs / 60),
        _ => format!("{:.1} 小時", secs as f64 / 3600.),
    }
}

fn pct(part: u64, total: u64) -> f32 {
    if total == 0 {
        0.
    } else {
        part as f32 / total as f32
    }
}

fn card() -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .px(px(18.))
        .py(px(16.))
        .rounded(px(10.))
        .border_1()
        .border_color(theme::border())
        .bg(theme::white())
}

fn stat(label: &'static str, value: String, note: String) -> Div {
    card()
        .flex_1()
        .min_w(px(180.))
        .child(
            div()
                .text_size(px(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::text_muted())
                .child(label),
        )
        .child(
            div()
                .text_size(px(24.))
                .font_weight(FontWeight::BOLD)
                .child(value),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(theme::text_muted())
                .child(note),
        )
}

/// 分成幾段的長條與圖例（計費方式、主要工作與記憶整理）。
fn split(title: &'static str, parts: Vec<(&'static str, u64, Hsla)>) -> Div {
    let total: u64 = parts.iter().map(|p| p.1).sum();
    let mut bar = div()
        .flex()
        .h(px(10.))
        .rounded_full()
        .overflow_hidden()
        .bg(theme::track());
    for (_, n, color) in &parts {
        if *n > 0 {
            bar = bar.child(div().h_full().w(relative(pct(*n, total))).bg(*color));
        }
    }
    let legend = div()
        .flex()
        .flex_wrap()
        .gap(px(16.))
        .children(parts.iter().map(|(label, n, color)| {
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .text_size(px(13.))
                .child(dot(*color, 8.))
                .child(*label)
                .child(div().text_color(theme::text_muted()).child(format!(
                    "{} · {:.0}%",
                    fmt_tokens(*n),
                    pct(*n, total) * 100.
                )))
        }));
    card()
        .flex_1()
        .min_w(px(280.))
        .gap(px(10.))
        .child(
            div()
                .text_size(px(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::text_muted())
                .child(title),
        )
        .child(bar)
        .child(legend)
}

/// 一張分組表：名稱、run 數、token（含佔比長條）、金額。
fn group_table(
    title: &'static str,
    note: String,
    groups: &[UsageGroup],
    total: u64,
    limit: usize,
) -> Div {
    let head = div()
        .flex()
        .items_center()
        .px(px(16.))
        .py(px(10.))
        .bg(theme::subtle_bg())
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::text_muted())
        .child(div().flex_1().min_w_0().child("名稱"))
        .child(div().flex_none().w(px(64.)).child("Run"))
        .child(div().flex_none().w(px(220.)).child("Token"))
        .child(div().flex_none().w(px(80.)).child("金額"));
    let mut table = div()
        .flex()
        .flex_col()
        .rounded(px(10.))
        .border_1()
        .border_color(theme::border())
        .bg(theme::white())
        .overflow_hidden()
        .child(head);
    if groups.is_empty() {
        table = table.child(
            div()
                .px(px(16.))
                .py(px(14.))
                .border_t_1()
                .border_color(theme::border())
                .text_size(px(13.))
                .text_color(theme::text_muted())
                .child("這段期間沒有用量。"),
        );
    }
    for g in groups.iter().take(limit) {
        let share = pct(g.tokens, total);
        table = table.child(
            div()
                .flex()
                .items_center()
                .px(px(16.))
                .py(px(10.))
                .border_t_1()
                .border_color(theme::border())
                .text_size(px(13.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .pr(px(12.))
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .truncate()
                                .child(g.label.clone()),
                        )
                        .when(!g.detail.is_empty(), |d| {
                            d.child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(theme::text_muted())
                                    .truncate()
                                    .child(g.detail.clone()),
                            )
                        }),
                )
                .child(div().flex_none().w(px(64.)).child(g.runs.to_string()))
                .child(
                    div()
                        .flex_none()
                        .w(px(220.))
                        .pr(px(16.))
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .child(div().flex_none().w(px(72.)).child(fmt_tokens(g.tokens)))
                        .child(
                            div()
                                .flex_1()
                                .h(px(6.))
                                .rounded_full()
                                .bg(theme::track())
                                .child(
                                    div()
                                        .h_full()
                                        .rounded_full()
                                        .w(relative(share))
                                        .bg(theme::accent()),
                                ),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .w(px(80.))
                        .text_color(theme::text_muted())
                        .child(g.cost_usd.map_or("—".into(), fmt_usd)),
                ),
        );
    }
    let hidden = groups.len().saturating_sub(limit);
    if hidden > 0 {
        table = table.child(
            div()
                .px(px(16.))
                .py(px(10.))
                .border_t_1()
                .border_color(theme::border())
                .text_size(px(12.))
                .text_color(theme::text_muted())
                .child(format!("還有 {hidden} 項用量較少的沒列出。")),
        );
    }
    div()
        .flex()
        .flex_col()
        .child(
            div()
                .flex()
                .items_baseline()
                .gap(px(10.))
                .pb(px(10.))
                .child(
                    div()
                        .text_size(px(16.))
                        .font_weight(FontWeight::BOLD)
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme::text_muted())
                        .child(note),
                ),
        )
        .child(table)
}

impl PotsApp {
    /// 實例層級的用量頁（左欄「用量」）。
    pub fn render_usage(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let range = self.usage_range;
        let mut ranges = segment_group();
        for r in UsageRange::ALL {
            ranges = ranges.child(
                segment(format!("usage-range-{}", r.secs()), r.label(), r == range)
                    .on_click(cx.listener(move |this, _, _, cx| this.set_usage_range(r, cx))),
            );
        }
        let header = div()
            .flex()
            .items_center()
            .gap(px(12.))
            .px(px(28.))
            .py(px(18.))
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .text_size(px(20.))
                            .font_weight(FontWeight::BOLD)
                            .child("用量"),
                    )
                    .child(
                        div()
                            .text_size(px(14.))
                            .text_color(theme::text_muted())
                            .child("Goal 預設不限預算、也不熔斷，只記用量。先觀察一陣子，再決定要不要設上限。"),
                    ),
            )
            .child(ranges)
            .child(
                secondary_button("usage-refresh", "重新整理")
                    .child(icon(IconName::RefreshCw, 14., theme::text_muted()))
                    .on_click(cx.listener(|this, _, _, cx| this.request_usage(cx))),
            );

        let body: AnyElement = match &self.usage {
            None => empty_state(
                if self.usage_loading {
                    "正在讀取用量…"
                } else {
                    "還沒有用量報表"
                },
                "報表依 project、Goal、agent 與模型彙總 provider 回報的 token。",
            )
            .into_any_element(),
            Some(report) => self.render_usage_report(report, cx).into_any_element(),
        };

        div()
            .flex_1()
            .min_w(px(420.))
            .h_full()
            .flex()
            .flex_col()
            .bg(theme::white())
            .child(header)
            .child(body)
    }

    fn render_usage_report(
        &self,
        report: &CostReport,
        cx: &mut Context<Self>,
    ) -> gpui_kit::Stateful<Div> {
        let t = &report.totals;
        let total = t.input_tokens + t.output_tokens;
        let rows = &report.rows;

        let by_project = group_rows(rows, |r| {
            Some((format!("# {}", r.project_name), String::new()))
        });
        let by_model = group_rows(rows, |r| Some((r.model.clone(), r.provider.clone())));
        let by_goal = group_rows(rows, |r| {
            let title = r.goal_title.clone()?;
            Some((title, format!("# {}", r.project_name)))
        });
        let by_agent = group_rows(rows, |r| Some((r.agent.clone(), r.category.label().into())));

        let cost = t.estimated_cost_usd + t.reported_cost_usd;
        let stats = div()
            .flex()
            .flex_wrap()
            .gap(px(12.))
            .child(stat(
                "總 token",
                fmt_tokens(total),
                format!(
                    "輸入 {} · 輸出 {}",
                    fmt_tokens(t.input_tokens),
                    fmt_tokens(t.output_tokens)
                ),
            ))
            .child(stat(
                "Run",
                t.runs.to_string(),
                format!("合計執行 {}", fmt_duration(t.duration_secs)),
            ))
            .child(stat(
                "金額（估計）",
                fmt_usd(cost),
                format!(
                    "單價估計 {} · CLI 回報 {}",
                    fmt_usd(t.estimated_cost_usd),
                    fmt_usd(t.reported_cost_usd)
                ),
            ))
            .child(stat(
                "平均每天",
                fmt_tokens(total / (self.usage_range.secs() as u64 / 86_400).max(1)),
                format!("過去 {}", self.usage_range.label()),
            ));

        let splits = div()
            .flex()
            .flex_wrap()
            .gap(px(12.))
            .child(split(
                "計費方式",
                vec![
                    (
                        Billing::Subscription.label(),
                        t.subscription_tokens,
                        theme::accent(),
                    ),
                    (Billing::Api.label(), t.api_tokens, theme::violet()),
                    (Billing::Cli.label(), t.cli_tokens, theme::warn_strong()),
                ],
            ))
            .child(split(
                "用途",
                vec![
                    (CostCategory::Work.label(), t.work_tokens, theme::ok()),
                    (CostCategory::Om.label(), t.om_tokens, theme::text_muted()),
                ],
            ));

        let pricing = &report.pricing;
        let pricing_note = if pricing.source.is_empty() {
            "還沒有單價表，金額只算 CLI 回報的部分。".to_string()
        } else {
            let updated = pricing.updated_at.map_or(String::new(), |at| {
                format!("，{}更新", at.relative_to(Timestamp::now()))
            });
            format!(
                "單價來自 {}（{} 個模型{}）",
                pricing.source, pricing.models, updated
            )
        };
        let note = div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap(px(10.))
            .px(px(16.))
            .py(px(10.))
            .rounded(px(10.))
            .bg(theme::subtle_bg())
            .text_size(px(13.))
            .text_color(theme::text_muted())
            .child(icon(IconName::Info, 14., theme::text_muted()))
            .child(div().flex_1().min_w(px(240.)).child(format!(
                "{pricing_note}。{} token 沒有單價（訂閱用量或表裡沒有的模型），不計入估計金額。",
                fmt_tokens(t.unpriced_tokens)
            )))
            .child(
                link_button("usage-refresh-pricing", "更新單價")
                    .text_size(px(13.))
                    .py(px(2.))
                    .on_click(cx.listener(|this, _, _, cx| this.refresh_pricing(cx))),
            );

        div()
            .id("usage-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(28.))
            .py(px(20.))
            .flex()
            .flex_col()
            .gap(px(24.))
            .when(self.usage_loading, |d| {
                d.child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme::text_muted())
                        .child("正在更新…"),
                )
            })
            .child(stats)
            .child(splits)
            .child(note)
            .child(group_table(
                "依 project",
                format!("{} 個", by_project.len()),
                &by_project,
                total,
                20,
            ))
            .child(group_table(
                "依 Goal",
                "Goal 預設不限預算；用量最多的在前".into(),
                &by_goal,
                total,
                15,
            ))
            .child(group_table(
                "依模型",
                format!("{} 個", by_model.len()),
                &by_model,
                total,
                20,
            ))
            .child(group_table(
                "依 agent",
                "對話、Planner、worker 與記憶整理".into(),
                &by_agent,
                total,
                20,
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        project: &str,
        goal: Option<&str>,
        model: &str,
        input: u64,
        cost: Option<f64>,
    ) -> CostRow {
        CostRow {
            project: ProjectId::from_u64(1),
            project_name: project.into(),
            goal: goal.map(|_| GoalId::from_u64(2)),
            goal_title: goal.map(Into::into),
            agent: "worker".into(),
            category: CostCategory::Work,
            provider: "OpenAI".into(),
            billing: Billing::Api,
            runtime: "builtin".into(),
            model: model.into(),
            runs: 1,
            input_tokens: input,
            output_tokens: 10,
            duration_secs: 5,
            reported_cost_usd: None,
            estimated_cost_usd: cost,
            priced_tokens: 0,
            unpriced_tokens: 0,
        }
    }

    #[test]
    fn groups_sum_and_sort_by_tokens() {
        let rows = vec![
            row("a", Some("g1"), "m1", 100, Some(0.5)),
            row("b", None, "m2", 1000, None),
            row("a", Some("g1"), "m2", 200, Some(0.25)),
        ];
        let by_project = group_rows(&rows, |r| Some((r.project_name.clone(), String::new())));
        assert_eq!(by_project.len(), 2);
        assert_eq!(by_project[0].label, "b");
        assert_eq!(by_project[0].cost_usd, None);
        assert_eq!(by_project[1].tokens, 320);
        assert_eq!(by_project[1].runs, 2);
        assert_eq!(by_project[1].cost_usd, Some(0.75));

        let by_goal = group_rows(&rows, |r| Some((r.goal_title.clone()?, String::new())));
        assert_eq!(by_goal.len(), 1, "沒有 Goal 的列不算");
    }

    #[test]
    fn token_and_money_formatting() {
        assert_eq!(fmt_tokens(8200), "8,200");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(123_456), "12.3 萬");
        assert_eq!(fmt_tokens(4_000_000), "400.0 萬");
        assert_eq!(fmt_tokens(125_000_000), "1.25 億");
        assert_eq!(fmt_usd(0.001), "< $0.01");
        assert_eq!(fmt_usd(0.), "$0.00");
        assert_eq!(fmt_usd(12.346), "$12.35");
    }
}
