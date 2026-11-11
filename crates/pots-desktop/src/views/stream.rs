//! 對話流的訊息與內嵌卡片；project 頻道、總管與任務 thread 共用。

use gpui_kit::assets::IconName;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::text::TextView;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, Div, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, div, px, relative,
};

use super::widgets::*;
use crate::app::{AutomationSection, GoalTab, Panel, PotsApp};
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

/// Goal 卡片最後一行：Planner 備註（結束的 Goal 是結果），有預算時加上已用比例。
pub fn goal_note(goal: &GoalView) -> String {
    let mut note = match &goal.state {
        GoalState::Active => goal.planner_note.clone(),
        GoalState::Done { summary } => format!("已完成：{summary}"),
        GoalState::Cancelled { reason } => format!("已取消：{reason}"),
    };
    if goal.budget_used_pct > 0 {
        if !note.is_empty() {
            note.push_str(" · ");
        }
        note.push_str(&format!("預算已用 {}%", goal.budget_used_pct));
    }
    note
}

pub fn goal_counts(goal: &GoalView) -> Div {
    let running = goal.count(TaskBucket::Running);
    let ci = goal.waiting_ci();
    let waiting = goal.count(TaskBucket::Waiting) - ci;
    let done = goal.count(TaskBucket::Done);
    let failed = goal
        .tasks
        .iter()
        .filter(|t| matches!(t.state, TaskStateView::Failed { .. }))
        .count();
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
        .when(failed > 0, |d| {
            d.child(pill(
                format!("{failed} 失敗"),
                theme::danger(),
                theme::danger_soft(),
            ))
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
            // 對話 agent 是這個 project 的負責人：一律顯示它現在的名字，改名前的訊息（包括舊版的
            // 「Pots」）也一樣。
            Author::Agent {
                role: AgentRole::Project,
                label,
            } => (
                self.current_project()
                    .map(|p| p.agent_name.clone())
                    .unwrap_or_else(|| label.clone()),
                true,
            ),
            Author::Agent { label, .. } => (label.clone(), false),
        };
        let streaming = msg.status == MsgStatus::Streaming;
        let time = if streaming {
            "正在輸入…".to_string()
        } else {
            item.at.hh_mm()
        };

        // GPUI 的 items_baseline 實際上是頂端對齊；名字、AGENT chip、時間字級不同，
        // 改成垂直置中並固定行高，三者才會在同一條中線上。
        let header = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                div()
                    .text_size(px(density.body()))
                    .line_height(px(20.))
                    .font_weight(FontWeight::BOLD)
                    .child(name),
            )
            .when(is_project_agent, |d| d.child(role_chip("AGENT")))
            .child(muted(time).text_size(px(12.)).line_height(px(20.)))
            .when(msg.steer, |d| {
                d.child(tag("改方向", theme::warn_text(), theme::warn_soft()))
            })
            .when(msg.status == MsgStatus::Queued, |d| {
                d.child(tag("排隊中", theme::accent_text(), theme::accent_soft()))
            })
            .when(msg.status == MsgStatus::Failed, |d| {
                d.child(tag("失敗", theme::danger(), theme::danger_soft()))
            });

        // 旁白：agent 在工具呼叫之間說的話，只在串流中由伺服器帶來，用淡色顯示它正在做什麼。
        let narration = (!msg.narration.is_empty()).then(|| {
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .pl(px(10.))
                .border_l_2()
                .border_color(theme::border())
                .text_size(px(density.body() - 1.))
                .line_height(relative(1.5))
                .text_color(theme::text_muted())
                .children(msg.narration.iter().map(|line| div().child(line.clone())))
        });

        let body = (!msg.text.is_empty()).then(|| {
            div()
                .text_size(px(density.body()))
                .line_height(relative(1.6))
                // 訊息內文是 Markdown；串流中的訊息尾端接一個游標字元。
                .child(
                    TextView::markdown(
                        SharedString::from(format!("msg-body-{}", id.0)),
                        if streaming {
                            format!("{}▍", msg.text)
                        } else {
                            msg.text.clone()
                        },
                    )
                    .selectable(true),
                )
        });

        let row = div()
            .id(SharedString::from(format!("msg-{}", id.0)))
            .flex()
            .gap(px(12.))
            .px(px(density.pad_x()))
            .py(px(8.))
            .rounded(px(8.))
            .hover(|s| s.bg(theme::subtle_bg()))
            .child(author_avatar(&msg.author, density.avatar()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(header)
                    .children(narration)
                    .children(body)
                    .children(msg.cards.iter().map(|c| self.render_card(id, c, cx))),
            );

        // 只有 agent 訊息能問「為什麼這樣回答」；右鍵叫出選單。
        if matches!(msg.author, Author::Agent { .. }) {
            let app = cx.entity().downgrade();
            let tools = msg.tools.clone();
            row.context_menu(move |menu, _, _| {
                let app = app.clone();
                let tools = tools.clone();
                menu.item(
                    PopupMenuItem::new("為什麼這樣回答").on_click(move |_, _, cx| {
                        app.update(cx, |this, cx| this.request_why(id, tools.clone(), cx))
                            .ok();
                    }),
                )
            })
            .into_any_element()
        } else {
            row.into_any_element()
        }
    }

    pub fn render_card(&self, item: ItemId, card: &Card, cx: &mut Context<Self>) -> AnyElement {
        match card {
            Card::GoalCreated { goal } => match self.store.goal(self.instance, *goal) {
                Some(g) => self.render_goal_card(g, cx).into_any_element(),
                None => muted("Goal 已不存在").into_any_element(),
            },
            // listener 依外部事件開的 Goal：上面註明是哪個事件觸發的。
            Card::Triggered { goal, event, .. } => div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(muted(format!("listener 觸發：{event}")))
                .child(match self.store.goal(self.instance, *goal) {
                    Some(g) => self.render_goal_card(g, cx).into_any_element(),
                    None => muted("Goal 已不存在").into_any_element(),
                })
                .into_any_element(),
            Card::Approval { id } => match self.store.approval(*id) {
                Some(a) => self.render_approval_card(a, cx).into_any_element(),
                None => muted("核准請求已不存在").into_any_element(),
            },
            Card::Artifact {
                id,
                kind,
                title,
                note,
            } => {
                let artifact = *id;
                let open = matches!(self.panel, Some(Panel::Artifact(a)) if a == artifact);
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
                            .child(if open { "在右側開啟中" } else { "開啟" }),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_artifact(artifact, window, cx)
                    }))
                    .into_any_element()
            }
            Card::Milestone { goal, text } => {
                let gid = *goal;
                let title = self
                    .store
                    .goal(self.instance, gid)
                    .map_or_else(|| "Goal 已不存在".to_string(), |g| g.title.clone());
                card_frame(520.)
                    .id(SharedString::from(format!("milestone-{}", item.0)))
                    .flex_row()
                    .items_start()
                    .gap(px(10.))
                    .px(px(14.))
                    .py(px(10.))
                    .rounded(px(10.))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::subtle_bg()))
                    .child(icon(IconName::Flag, 16., theme::accent()).mt(px(2.)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(muted(format!("Planner · {title}")))
                            .child(div().child(text.clone())),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.store.goal(this.instance, gid).is_some() {
                            this.open_panel(Some(Panel::Goal(gid, GoalTab::Tasks)), cx)
                        }
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
                        // 已經恢復就不再顯示「立即恢復」。
                        .children(
                            actions
                                .iter()
                                .enumerate()
                                .filter(|(_, a)| {
                                    **a != NoticeAction::ResumeAutomations
                                        || self.current_instance().automation_pause.is_some()
                                })
                                .map(|(i, &action)| {
                                    link_button(
                                        SharedString::from(format!("notice-{}-{i}", item.0)),
                                        action.label(),
                                    )
                                    .px(px(0.))
                                    .mr(px(12.))
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| match action {
                                            NoticeAction::ViewAutomations => {
                                                this.open_automations(
                                                    AutomationSection::Schedules,
                                                    cx,
                                                )
                                            }
                                            NoticeAction::ResumeAutomations => {
                                                this.resume_automations(cx)
                                            }
                                        },
                                    ))
                                }),
                        ),
                )
                .into_any_element(),
        }
    }

    pub fn render_goal_card(&self, goal: &GoalView, cx: &mut Context<Self>) -> impl IntoElement {
        self.goal_card(goal, true, cx)
    }

    /// Goal 卡片。`hint` 是對話裡的「在右側開啟」提示；右欄的 Goals 列表本身就在右側，不顯示。
    pub(super) fn goal_card(
        &self,
        goal: &GoalView,
        hint: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
                    .child(tag("GOAL", theme::accent(), theme::accent_soft()).flex_none())
                    // 標題可以縮、換行：右欄的 Goals 列表只有 380px，長標題不能撑破卡片。
                    .child(
                        div()
                            .min_w_0()
                            .text_size(px(16.))
                            .font_weight(FontWeight::BOLD)
                            .child(goal.title.clone()),
                    )
                    .when(goal.quick, |d| {
                        d.child(tag("快速修改", theme::violet(), theme::violet_soft()).flex_none())
                    })
                    .when(!goal.state.is_active(), |d| {
                        d.child(
                            tag(goal.state.label(), theme::text_muted(), theme::track())
                                .flex_none(),
                        )
                    })
                    .when(hint, |d| {
                        d.child(
                            div()
                                .ml_auto()
                                .flex_none()
                                .text_size(px(13.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme::accent())
                                .child(if open {
                                    "在右側開啟中"
                                } else {
                                    "在右側開啟"
                                }),
                        )
                    }),
            )
            .child(goal_progress(goal))
            .child(goal_counts(goal))
            .child(muted(goal_note(goal)))
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
                "已核准 · 平台正在執行".to_string(),
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
                    .when(!a.detail.is_empty() && a.approval.is_some(), |d| {
                        d.child(div().line_height(relative(1.5)).child(a.detail.clone()))
                    })
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .children(a.facts.iter().map(|f| muted(f.clone()))),
                    )
                    .when_some(super::preferences::approval_diff(a), |d, diff| {
                        d.child(super::preferences::diff_block(
                            diff,
                            cx.theme().mono_font_family.clone(),
                        ))
                    })
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
