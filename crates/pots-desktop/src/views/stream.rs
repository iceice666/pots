//! 對話流的訊息與內嵌卡片；project 頻道、總管與任務 thread 共用。

use gpui_kit::assets::IconName;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, Div, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, div, px, relative,
};

use super::widgets::*;
use crate::app::{GoalTab, Panel, PotsApp};
use crate::model::*;
use crate::theme;

/// 對話流的密度：頻道與總管用一般尺寸，右欄 thread 用緊湊尺寸。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Density {
    Regular,
    Compact,
}

impl Density {
    fn avatar(self) -> f32 {
        match self {
            Self::Regular => 36.,
            Self::Compact => 28.,
        }
    }

    fn body(self) -> f32 {
        match self {
            Self::Regular => 15.,
            Self::Compact => 14.,
        }
    }

    fn pad_x(self) -> f32 {
        match self {
            Self::Regular => 24.,
            Self::Compact => 0.,
        }
    }
}

fn author_avatar(author: &Author, size: f32) -> Div {
    match author {
        Author::Human => avatar("你", theme::accent(), size),
        Author::Agent {
            role: AgentRole::Project,
            ..
        } => avatar("A", theme::text(), size),
        Author::Agent {
            role: AgentRole::Worker,
            ..
        } => avatar("W", theme::violet(), size),
        Author::Agent {
            role: AgentRole::Overseer,
            ..
        } => div()
            .flex_none()
            .size(px(size))
            .rounded(px(size * 0.22))
            .bg(theme::violet_soft())
            .flex()
            .items_center()
            .justify_center()
            .child(icon(IconName::Sparkles, size * 0.5, theme::violet())),
    }
}

fn card_frame(max_w: f32) -> Div {
    div()
        .max_w(px(max_w))
        .rounded(px(12.))
        .border_1()
        .border_color(theme::border())
        .bg(theme::white())
        .flex()
        .flex_col()
}

fn muted(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(13.))
        .text_color(theme::text_muted())
        .child(text.into())
}

/// 進度條：完成（綠）、進行中（藍）、等待（橘）依比例排開。
fn goal_progress(goal: &GoalView) -> Div {
    let total = goal.tasks.len().max(1) as f32;
    let seg = |n: usize, color| div().h_full().w(relative(n as f32 / total)).bg(color);
    div()
        .flex()
        .h(px(8.))
        .w_full()
        .rounded_full()
        .overflow_hidden()
        .bg(theme::track())
        .child(seg(goal.count(TaskBucket::Done), theme::ok()))
        .child(seg(goal.count(TaskBucket::Running), theme::accent()))
        .child(seg(goal.count(TaskBucket::Waiting), theme::warn_strong()))
}

pub fn goal_counts(goal: &GoalView) -> Div {
    let running = goal.count(TaskBucket::Running);
    let ci = goal.waiting_ci();
    let waiting = goal.count(TaskBucket::Waiting) - ci;
    let done = goal.count(TaskBucket::Done);
    div()
        .flex()
        .flex_wrap()
        .gap(px(8.))
        .when(running > 0, |d| {
            d.child(pill(
                format!("{running} 進行中"),
                theme::accent_text(),
                theme::accent_soft(),
            ))
        })
        .when(ci > 0, |d| {
            d.child(pill(
                format!("{ci} 等待 CI"),
                theme::warn(),
                theme::warn_soft(),
            ))
        })
        .when(waiting > 0, |d| {
            d.child(pill(
                format!("{waiting} 等待中"),
                theme::warn(),
                theme::warn_soft(),
            ))
        })
        .when(done > 0, |d| {
            d.child(pill(format!("{done} 完成"), theme::ok(), theme::ok_soft()))
        })
}

impl PotsApp {
    pub fn render_stream_item(
        &self,
        item: &StreamItemView,
        density: Density,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pad = density.pad_x();
        match &item.kind {
            StreamItemKind::DayDivider(label) => div()
                .flex()
                .items_center()
                .gap(px(12.))
                .px(px(pad))
                .py(px(4.))
                .text_size(px(12.))
                .text_color(theme::text_muted())
                .child(div().flex_1().h(px(1.)).bg(theme::border()))
                .child(label.clone())
                .child(div().flex_1().h(px(1.)).bg(theme::border()))
                .into_any_element(),
            StreamItemKind::Event { text } => div()
                .flex()
                .items_center()
                .gap(px(8.))
                .pl(px(pad + density.avatar() + 12.))
                .pr(px(pad))
                .py(px(6.))
                .text_size(px(13.))
                .text_color(theme::text_muted())
                .child(icon(IconName::GitPullRequest, 14., theme::text_muted()))
                .child(format!("{text} · {}", item.at.hh_mm()))
                .into_any_element(),
            StreamItemKind::Card(card) => div()
                .flex()
                .gap(px(12.))
                .px(px(pad))
                .py(px(8.))
                .child(div().flex_none().w(px(density.avatar())))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(self.render_card(item.id, card, cx)),
                )
                .into_any_element(),
            StreamItemKind::Human(msg) | StreamItemKind::Agent(msg) => {
                self.render_msg(item, msg, density, cx)
            }
        }
    }

    fn render_msg(
        &self,
        item: &StreamItemView,
        msg: &Msg,
        density: Density,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = item.id;
        let (name, is_project_agent) = match &msg.author {
            Author::Human => ("你".to_string(), false),
            Author::Agent { label, role } => (label.clone(), *role == AgentRole::Project),
        };
        let streaming = msg.status == MsgStatus::Streaming;
        let time = if streaming {
            "正在輸入…".to_string()
        } else {
            item.at.hh_mm()
        };

        let header = div()
            .flex()
            .items_baseline()
            .gap(px(8.))
            .child(
                div()
                    .text_size(px(density.body()))
                    .font_weight(FontWeight::BOLD)
                    .child(name),
            )
            .when(is_project_agent, |d| {
                d.child(
                    div()
                        .text_size(px(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::text_muted())
                        .border_1()
                        .border_color(theme::border_strong())
                        .px(px(5.))
                        .rounded(px(4.))
                        .child("AGENT"),
                )
            })
            .child(muted(time).text_size(px(12.)))
            .when(msg.steer, |d| {
                d.child(tag("改方向", theme::warn_text(), theme::warn_soft()))
            })
            .when(msg.status == MsgStatus::Queued, |d| {
                d.child(tag("排隊中", theme::accent_text(), theme::accent_soft()))
            })
            .when(msg.status == MsgStatus::Failed, |d| {
                d.child(tag("失敗", theme::danger(), theme::danger_soft()))
            });

        let body = (!msg.text.is_empty()).then(|| {
            div()
                .text_size(px(density.body()))
                .line_height(relative(1.6))
                // 串流中的訊息尾端接一個游標字元。
                .child(if streaming {
                    format!("{}▍", msg.text)
                } else {
                    msg.text.clone()
                })
        });

        let tools = msg.tools.as_ref().map(|t| {
            let expanded = self.expanded_tools.contains(&id);
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(
                    div()
                        .id(SharedString::from(format!("tools-{}", id.0)))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .self_start()
                        .px(px(10.))
                        .py(px(4.))
                        .rounded(px(6.))
                        .border_1()
                        .border_color(theme::border())
                        .bg(theme::subtle_bg())
                        .text_size(px(13.))
                        .text_color(theme::text_muted())
                        .cursor_pointer()
                        .hover(|s| s.bg(theme::track()))
                        .child(icon(IconName::Wrench, 13., theme::text_muted()))
                        .child(t.text.clone())
                        .child(icon(
                            if expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            },
                            13.,
                            theme::text_muted(),
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| this.toggle_tools(id, cx))),
                )
                .when(expanded, |d| {
                    d.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .pl(px(12.))
                            .border_l_2()
                            .border_color(theme::border())
                            .text_size(px(12.))
                            .text_color(theme::text_muted())
                            .children(t.details.iter().map(|line| div().child(line.clone()))),
                    )
                })
        });

        div()
            .flex()
            .gap(px(12.))
            .px(px(density.pad_x()))
            .py(px(8.))
            .child(author_avatar(&msg.author, density.avatar()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(header)
                    .children(body)
                    .children(tools)
                    .children(msg.cards.iter().map(|c| self.render_card(id, c, cx))),
            )
            .into_any_element()
    }

    pub fn render_card(&self, item: ItemId, card: &Card, cx: &mut Context<Self>) -> AnyElement {
        match card {
            Card::GoalCreated { goal } => match self.store.goal(self.instance, *goal) {
                Some(g) => self.render_goal_card(g, cx).into_any_element(),
                None => muted("Goal 已不存在").into_any_element(),
            },
            Card::Approval { id } => match self.store.approval(*id) {
                Some(a) => self.render_approval_card(a, cx).into_any_element(),
                None => muted("核准請求已不存在").into_any_element(),
            },
            Card::Artifact {
                kind, title, note, ..
            } => {
                let what = format!("{} · {title}", kind.label());
                card_frame(480.)
                    .id(SharedString::from(format!("artifact-{}", item.0)))
                    .flex_row()
                    .items_center()
                    .gap(px(12.))
                    .px(px(14.))
                    .py(px(12.))
                    .rounded(px(10.))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::subtle_bg()))
                    .child(
                        div()
                            .flex_none()
                            .size(px(36.))
                            .rounded(px(8.))
                            .bg(theme::violet_soft())
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(icon(IconName::FileText, 18., theme::violet())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(what.clone()))
                            .child(muted(note.clone())),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::accent())
                            .child("開啟"),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.notify_unimplemented(&format!("開啟 {what}"), cx)
                    }))
                    .into_any_element()
            }
            Card::RemoteGoal {
                project,
                title,
                note,
            } => card_frame(520.)
                .gap(px(6.))
                .px(px(16.))
                .py(px(12.))
                .border_color(theme::accent())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(theme::text_muted())
                                .child(format!("# {project}")),
                        )
                        .child(tag("GOAL", theme::accent(), theme::accent_soft()))
                        .child(div().font_weight(FontWeight::BOLD).child(title.clone())),
                )
                .child(muted(note.clone()))
                .into_any_element(),
            Card::CatchUp { summary } => card_frame(560.)
                .gap(px(6.))
                .px(px(16.))
                .py(px(12.))
                .bg(theme::subtle_bg())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .font_weight(FontWeight::BOLD)
                        .child(icon(IconName::Clock, 15., theme::text_muted()))
                        .child("你不在期間"),
                )
                .child(div().line_height(relative(1.5)).child(summary.clone()))
                .into_any_element(),
            Card::Digest {
                title,
                rows,
                footer,
            } => card_frame(560.)
                .gap(px(10.))
                .px(px(16.))
                .py(px(14.))
                .child(
                    div()
                        .text_size(px(16.))
                        .font_weight(FontWeight::BOLD)
                        .child(title.clone()),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .children(rows.iter().map(|(k, v)| {
                            div()
                                .flex()
                                .gap(px(12.))
                                .child(
                                    div()
                                        .w(px(120.))
                                        .flex_none()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(k.clone()),
                                )
                                .child(div().text_color(theme::text_muted()).child(v.clone()))
                        })),
                )
                .child(muted(footer.clone()))
                .into_any_element(),
            Card::Notice {
                title,
                body,
                actions,
            } => card_frame(560.)
                .gap(px(8.))
                .px(px(16.))
                .py(px(12.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .font_weight(FontWeight::BOLD)
                        .child(icon(IconName::CircleCheck, 16., theme::ok()))
                        .child(title.clone()),
                )
                .child(muted(body.clone()))
                .child(
                    div()
                        .flex()
                        .gap(px(4.))
                        .children(actions.iter().enumerate().map(|(i, label)| {
                            let label = label.clone();
                            link_button(
                                SharedString::from(format!("notice-{}-{i}", item.0)),
                                label.clone(),
                            )
                            .px(px(0.))
                            .mr(px(12.))
                            .on_click(cx.listener(
                                move |this, _, _, cx| this.notify_unimplemented(&label, cx),
                            ))
                        })),
                )
                .into_any_element(),
        }
    }

    pub fn render_goal_card(&self, goal: &GoalView, cx: &mut Context<Self>) -> impl IntoElement {
        let gid = goal.id;
        let open = matches!(self.panel, Some(Panel::Goal(g, _)) if g == gid);
        card_frame(560.)
            .id(SharedString::from(format!("goal-card-{}", gid.0)))
            .gap(px(12.))
            .px(px(16.))
            .py(px(14.))
            .border_2()
            .border_color(theme::accent())
            .cursor_pointer()
            .hover(|s| s.bg(theme::subtle_bg()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(tag("GOAL", theme::accent(), theme::accent_soft()))
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::BOLD)
                            .child(goal.title.clone()),
                    )
                    .child(
                        div()
                            .ml_auto()
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::accent())
                            .child(if open {
                                "在右側開啟中"
                            } else {
                                "在右側開啟"
                            }),
                    ),
            )
            .child(goal_progress(goal))
            .child(goal_counts(goal))
            .child(muted(format!(
                "{} · 預算已用 {}%",
                goal.planner_note, goal.budget_used_pct
            )))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.open_panel(Some(Panel::Goal(gid, GoalTab::Tasks)), cx)
            }))
    }

    pub fn render_approval_card(&self, a: &InboxItem, cx: &mut Context<Self>) -> impl IntoElement {
        let id = a.id;
        let (banner, banner_fg, banner_bg, border) = match &a.decision {
            Decision::Pending => (
                "需要你核准".to_string(),
                theme::warn_text(),
                theme::warn_soft(),
                theme::warn_border(),
            ),
            Decision::Approved => (
                "已核准 · 平台正在執行 merge".to_string(),
                theme::ok(),
                theme::ok_soft(),
                theme::border(),
            ),
            Decision::Rejected => (
                "已拒絕".to_string(),
                theme::text_muted(),
                theme::track(),
                theme::border(),
            ),
            Decision::Resolved(s) => (
                s.clone(),
                theme::text_muted(),
                theme::track(),
                theme::border(),
            ),
        };
        let pending = a.is_open();

        card_frame(560.)
            .overflow_hidden()
            .border_color(border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(16.))
                    .py(px(8.))
                    .bg(banner_bg)
                    .text_color(banner_fg)
                    .text_size(px(13.))
                    .font_weight(FontWeight::BOLD)
                    .child(banner)
                    .when_some(a.expiry.clone().filter(|_| pending), |d, e| {
                        d.child(div().ml_auto().font_weight(FontWeight::MEDIUM).child(e))
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .px(px(16.))
                    .py(px(14.))
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::BOLD)
                            .child(a.title.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .children(a.facts.iter().map(|f| muted(f.clone()))),
                    )
                    .when(pending, |d| {
                        d.child(self.render_actions(id, &a.actions, "card", cx))
                    }),
            )
    }

    /// 核准／處理按鈕列；第一個動作用主要樣式。
    pub fn render_actions(
        &self,
        id: ApprovalId,
        actions: &[InboxAction],
        scope: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .flex()
            .flex_wrap()
            .gap(px(8.))
            .children(actions.iter().enumerate().map(|(i, &action)| {
                let key = SharedString::from(format!("{scope}-{}-{i}", id.0));
                let button = match (i, action) {
                    (0, _) => primary_button(key, action.label()),
                    (_, InboxAction::Reject | InboxAction::CancelTask) => {
                        secondary_button(key, action.label())
                    }
                    (1, _) => secondary_button(key, action.label()),
                    _ => link_button(key, action.label()),
                };
                button.on_click(
                    cx.listener(move |this, _, window, cx| this.act(id, action, window, cx)),
                )
            }))
    }
}
