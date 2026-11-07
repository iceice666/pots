//! 右欄狀態面板：Goal 任務樹與 Worker 任務 thread。

use gpui_kit::assets::IconName;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::input::Input;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, Div, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, div, px, relative,
};

use super::channel::empty_state;
use super::stream::Density;
use super::widgets::*;
use crate::app::{GoalTab, Panel, PotsApp};
use crate::model::*;
use crate::theme;

fn panel_frame() -> Div {
    div()
        .flex_none()
        .w(px(380.))
        .h_full()
        .bg(theme::panel_bg())
        .border_l_1()
        .border_color(theme::border())
        .flex()
        .flex_col()
}

fn task_marker(state: &TaskStateView) -> AnyElement {
    match state.bucket() {
        TaskBucket::Running => div()
            .mt(px(6.))
            .child(dot(theme::accent(), 8.))
            .into_any_element(),
        TaskBucket::Waiting => div()
            .mt(px(6.))
            .child(dot(theme::warn_strong(), 8.))
            .into_any_element(),
        TaskBucket::Done => div()
            .mt(px(3.))
            .child(icon(IconName::CircleCheck, 14., theme::ok()))
            .into_any_element(),
        TaskBucket::Stopped => div()
            .mt(px(3.))
            .child(icon(IconName::CircleStop, 14., theme::text_muted()))
            .into_any_element(),
    }
}

fn task_state_pill(state: &TaskStateView) -> Div {
    match state {
        TaskStateView::Planned => pill("已規劃", theme::text_muted(), theme::track()),
        TaskStateView::Running => pill("進行中", theme::accent_text(), theme::accent_soft()),
        TaskStateView::Waiting { on } => {
            pill(format!("等待 {on}"), theme::warn(), theme::warn_soft())
        }
        TaskStateView::NeedsHuman { .. } => pill("需要你", theme::warn_text(), theme::warn_soft()),
        TaskStateView::Done { .. } => pill("完成", theme::ok(), theme::ok_soft()),
        TaskStateView::Failed { attempts, .. } => pill(
            format!("失敗 · {attempts} 次"),
            theme::danger(),
            theme::danger_soft(),
        ),
        TaskStateView::Cancelled => pill("已取消", theme::text_muted(), theme::track()),
    }
}

fn team_post(post: &TeamPost) -> Div {
    let (fg, bg) = match post.kind {
        TeamPostKind::Decision => (theme::accent_text(), theme::accent_soft()),
        TeamPostKind::Question | TeamPostKind::Blocker => (theme::warn(), theme::warn_soft()),
        TeamPostKind::Finding => (theme::violet(), theme::violet_soft()),
        TeamPostKind::Handoff => (theme::ok(), theme::ok_soft()),
    };
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .text_size(px(12.))
                .child(tag(post.kind.label(), fg, bg))
                .child(div().text_color(theme::text_muted()).child(format!(
                    "{} · {}",
                    post.from,
                    post.at.hh_mm()
                ))),
        )
        .child(div().line_height(relative(1.5)).child(post.body.clone()))
}

fn white_box() -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(10.))
        .p(px(12.))
        .rounded(px(8.))
        .border_1()
        .border_color(theme::border())
        .bg(theme::white())
}

fn note_box(input: AnyElement, send: AnyElement) -> Div {
    div()
        .mx(px(20.))
        .mb(px(20.))
        .flex()
        .items_center()
        .gap(px(8.))
        .pl(px(4.))
        .pr(px(6.))
        .py(px(6.))
        .rounded(px(10.))
        .border_1()
        .border_color(theme::input_border())
        .bg(theme::white())
        .child(div().flex_1().min_w_0().child(input))
        .child(send)
}

impl PotsApp {
    pub fn render_panel(&self, panel: Panel, cx: &mut Context<Self>) -> Option<AnyElement> {
        match panel {
            Panel::Goal(goal, tab) => {
                let g = self.store.goal(self.instance, goal)?;
                Some(self.render_goal_panel(g, tab, cx).into_any_element())
            }
            Panel::Task(task) => {
                let (g, t) = self.store.task(self.instance, task)?;
                Some(self.render_task_panel(g, t, cx).into_any_element())
            }
        }
    }

    fn close_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        icon_button("panel-close", IconName::X)
            .ml_auto()
            .tip("關閉面板")
            .on_click(cx.listener(|this, _, _, cx| this.open_panel(None, cx)))
    }

    fn render_goal_panel(
        &self,
        g: &GoalView,
        tab: GoalTab,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let gid = g.id;
        let tab_item = |id: &'static str, label: String, value: GoalTab, cx: &mut Context<Self>| {
            super::widgets::tab(id, label, tab == value)
                .pt(px(4.))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.open_panel(Some(Panel::Goal(gid, value)), cx)
                }))
        };

        let header = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .px(px(20.))
            .pt(px(14.))
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(div().text_color(theme::text_muted()).child("Goal"))
                    .child(icon(IconName::ChevronRight, 14., theme::text_muted()))
                    .child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .truncate()
                            .child(g.title.clone()),
                    )
                    .child(self.close_button(cx)),
            )
            .child(
                div()
                    .flex()
                    .gap(px(18.))
                    .child(tab_item(
                        "goal-tab-tasks",
                        "任務".into(),
                        GoalTab::Tasks,
                        cx,
                    ))
                    .child(tab_item(
                        "goal-tab-brief",
                        "Brief".into(),
                        GoalTab::Brief,
                        cx,
                    ))
                    .child(tab_item(
                        "goal-tab-ledger",
                        "Ledger".into(),
                        GoalTab::Ledger,
                        cx,
                    ))
                    .child(tab_item(
                        "goal-tab-team",
                        format!("Team {}", g.team.len()),
                        GoalTab::Team,
                        cx,
                    )),
            );

        let body: AnyElement = match tab {
            GoalTab::Tasks => {
                let mut col = div().flex().flex_col().gap(px(8.));
                for bucket in [
                    TaskBucket::Running,
                    TaskBucket::Waiting,
                    TaskBucket::Done,
                    TaskBucket::Stopped,
                ] {
                    let tasks: Vec<_> = g
                        .tasks
                        .iter()
                        .filter(|t| t.state.bucket() == bucket)
                        .collect();
                    if tasks.is_empty() {
                        continue;
                    }
                    col = col.child(section_label(format!(
                        "{} · {}",
                        bucket.label(),
                        tasks.len()
                    )));
                    for t in tasks {
                        let tid = t.id;
                        let finished = matches!(bucket, TaskBucket::Done | TaskBucket::Stopped);
                        let elapsed = (bucket == TaskBucket::Running).then(|| {
                            div()
                                .ml_auto()
                                .flex_none()
                                .text_size(px(12.))
                                .text_color(theme::text_muted())
                                .child(format!("{} 分", t.elapsed_min))
                        });
                        col = col.child(
                            div()
                                .id(SharedString::from(format!("task-{}", tid.0)))
                                .flex()
                                .items_start()
                                .gap(px(10.))
                                .p(px(12.))
                                .rounded(px(8.))
                                .border_1()
                                .border_color(theme::border())
                                .bg(theme::white())
                                .cursor_pointer()
                                .hover(|s| s.border_color(theme::border_strong()))
                                .child(task_marker(&t.state))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .flex()
                                        .flex_col()
                                        .gap(px(2.))
                                        .child(
                                            div()
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .child(t.title.clone()),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(13.))
                                                .text_color(theme::text_muted())
                                                .child(format!("{} · {}", t.assignee, t.summary)),
                                        ),
                                )
                                .children(elapsed)
                                .when(finished, |d| d.text_color(theme::text_muted()))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_panel(Some(Panel::Task(tid)), cx)
                                })),
                        );
                    }
                }
                if !g.team.is_empty() {
                    col = col
                        .child(section_label("最新 Team 貼文").pt(px(20.)))
                        .child(white_box().children(g.team.iter().take(3).map(team_post)));
                }
                col.into_any_element()
            }
            GoalTab::Brief => white_box()
                .child(
                    div()
                        .text_size(px(12.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::text_muted())
                        .child("Planner 交接用的 Goal brief"),
                )
                .child(div().line_height(relative(1.6)).child(g.brief.clone()))
                .into_any_element(),
            GoalTab::Ledger => empty_state(
                "Ledger",
                "Planner 的工作帳本：決定、嘗試與花費。M2 隨 Artifact 一起實作。",
            )
            .into_any_element(),
            GoalTab::Team => {
                if g.team.is_empty() {
                    empty_state(
                        "還沒有 Team 貼文",
                        "成員的發現、問題、決定、阻礙與交接會出現在這裡。",
                    )
                    .into_any_element()
                } else {
                    white_box()
                        .gap(px(14.))
                        .children(g.team.iter().map(team_post))
                        .into_any_element()
                }
            }
        };

        panel_frame()
            .child(header)
            .child(
                div()
                    .id("goal-panel-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(20.))
                    .py(px(16.))
                    .child(body),
            )
            .child(note_box(
                Input::new(&self.goal_note)
                    .appearance(false)
                    .into_any_element(),
                send_button("goal-note-send", 32.)
                    .on_click(cx.listener(|this, _, window, cx| this.send_goal_note(window, cx)))
                    .into_any_element(),
            ))
    }

    fn render_task_panel(
        &self,
        g: &GoalView,
        t: &TaskView,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let gid = g.id;
        let tid = t.id;
        let cancellable = !matches!(
            t.state,
            TaskStateView::Done { .. } | TaskStateView::Cancelled | TaskStateView::Failed { .. }
        );

        // 第一行：狀態與執行者；第二行：分支、耗時與花費。
        let meta = div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .text_size(px(13.))
            .text_color(theme::text_muted())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(task_state_pill(&t.state))
                    .child(t.assignee.clone()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .when_some(t.branch.clone(), |d, branch| {
                        d.child(
                            div()
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_size(px(12.))
                                .child(branch),
                        )
                        .child("·")
                    })
                    .child(format!(
                        "{} 分鐘 · ${}.{:02}",
                        t.elapsed_min,
                        t.cost_cents / 100,
                        t.cost_cents % 100
                    )),
            );

        let header = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .px(px(20.))
            .pt(px(14.))
            .pb(px(14.))
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        div()
                            .id("task-back")
                            .text_color(theme::accent())
                            .cursor_pointer()
                            .truncate()
                            .hover(|s| s.opacity(0.8))
                            .child(g.title.clone())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_panel(Some(Panel::Goal(gid, GoalTab::Tasks)), cx)
                            })),
                    )
                    .child(icon(IconName::ChevronRight, 14., theme::text_muted()))
                    .child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .truncate()
                            .child(t.title.clone()),
                    )
                    .child(self.close_button(cx)),
            )
            .child(meta)
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(
                        secondary_button("task-diff", "看 diff")
                            .text_size(px(13.))
                            .px(px(12.))
                            .py(px(5.))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.notify_unimplemented("看 diff", cx)
                            })),
                    )
                    .child(
                        secondary_button("task-trace", "看 trace")
                            .text_size(px(13.))
                            .px(px(12.))
                            .py(px(5.))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.notify_unimplemented("看 trace", cx)
                            })),
                    )
                    .when(cancellable, |d| {
                        d.child(
                            secondary_button("task-cancel", "取消任務")
                                .text_size(px(13.))
                                .px(px(12.))
                                .py(px(5.))
                                .text_color(theme::danger())
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.cancel_task(tid, cx)),
                                ),
                        )
                    }),
            );

        let steps = div()
            .flex()
            .flex_col()
            .children(t.steps.iter().enumerate().map(|(i, s)| {
                let last = i + 1 == t.steps.len();
                let marker = match s.at {
                    Some(_) => icon(IconName::Check, 12., theme::ok()).into_any_element(),
                    None => dot(theme::accent(), 8.).into_any_element(),
                };
                div()
                    .flex()
                    .gap(px(10.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .w(px(14.))
                            .child(div().h(px(18.)).flex().items_center().child(marker))
                            .when(!last, |d| {
                                d.child(div().flex_1().w(px(1.)).bg(theme::border()))
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .gap(px(8.))
                            .pb(px(12.))
                            .child(
                                div()
                                    .flex_1()
                                    .line_height(relative(1.4))
                                    .child(s.text.clone()),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(12.))
                                    .text_color(if s.at.is_some() {
                                        theme::text_muted()
                                    } else {
                                        theme::accent()
                                    })
                                    .child(s.at.map_or("進行中".to_string(), |t| t.hh_mm())),
                            ),
                    )
            }));

        let thread: AnyElement = if t.thread.is_empty() {
            div()
                .text_size(px(13.))
                .text_color(theme::text_muted())
                .child("還沒有對話。你可以直接對這個 worker 說話。")
                .into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .children(
                    t.thread
                        .iter()
                        .map(|item| self.render_stream_item(item, Density::Compact, cx)),
                )
                .into_any_element()
        };

        panel_frame()
            .child(header)
            .child(
                div()
                    .id("task-panel-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(20.))
                    .py(px(12.))
                    .child(section_label("進度").pt(px(4.)).pb(px(10.)))
                    .child(steps)
                    .child(section_label("這個任務的對話").pt(px(12.)))
                    .child(thread),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        note_box(
                            Input::new(&self.worker_note)
                                .appearance(false)
                                .into_any_element(),
                            send_button("worker-note-send", 32.)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.send_worker_note(window, cx)
                                }))
                                .into_any_element(),
                        )
                        .mb(px(0.)),
                    )
                    .child(
                        div()
                            .px(px(20.))
                            .pb(px(16.))
                            .text_size(px(12.))
                            .text_color(theme::text_muted())
                            .child("訊息只送給這個 worker，Planner 會在 Ledger 看到摘要。"),
                    ),
            )
    }
}
