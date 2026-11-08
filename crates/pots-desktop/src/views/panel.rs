//! 右欄狀態面板：Goal 任務樹與 Worker 任務 thread。
//!
//! 任務列依狀態說明現在怎樣了（進行中是 worker 最後一句進度、需要你的原因、完成的結果、
//! 失敗的錯誤），下面是執行者、分支與 PR（點了用系統瀏覽器開）。Team 分頁唯讀，新的在上面。

use gpui_kit::assets::IconName;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::input::Input;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, Div, FontWeight, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, div, px,
    relative,
};

use super::channel::empty_state;
use super::stream::Density;
use super::widgets::*;
use crate::app::{GoalTab, Panel, PotsApp};
use crate::model::*;
use crate::theme;

pub(super) fn panel_frame() -> Div {
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

/// 任務現在怎樣了：進行中是 worker 最後一句進度，其他狀態是等什麼、為什麼停、結果是什麼。
pub(super) fn task_detail(t: &TaskView) -> (String, Hsla) {
    let or = |text: &str, fallback: &str| {
        if text.trim().is_empty() {
            fallback.to_string()
        } else {
            text.trim().to_string()
        }
    };
    match &t.state {
        TaskStateView::Running => (
            or(&t.progress, &or(&t.summary, "worker 開始處理")),
            theme::text(),
        ),
        TaskStateView::Planned => (or(&t.summary, "還沒開始"), theme::text_muted()),
        TaskStateView::Waiting { on } => (format!("等待 {on}"), theme::warn()),
        TaskStateView::NeedsHuman { reason } => (
            format!("需要你：{}", or(reason, "worker 停下來了")),
            theme::warn_text(),
        ),
        TaskStateView::Done { outcome } => (or(outcome, "完成"), theme::ok()),
        TaskStateView::Failed { attempts, error } => (
            format!("失敗 {attempts} 次：{}", or(error, "沒有說明")),
            theme::danger(),
        ),
        TaskStateView::Cancelled => ("已取消".into(), theme::text_muted()),
    }
}

/// Team 貼文的來源與對象：沒有指定對象的問題是問 Planner。
fn team_route(post: &TeamPost) -> String {
    match (&post.to, post.kind) {
        (Some(to), _) => format!("{} → {to}", post.from),
        (None, TeamPostKind::Question) if post.from != "Planner" => {
            format!("{} → Planner", post.from)
        }
        (None, _) => post.from.clone(),
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
                .child(
                    div()
                        .min_w_0()
                        .text_color(theme::text_muted())
                        .child(format!("{} · {}", team_route(post), post.at.hh_mm())),
                ),
        )
        .child(div().line_height(relative(1.5)).child(post.body.clone()))
}

pub(super) fn white_box() -> Div {
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
            Panel::Artifact(id) => {
                let (_, a) = self.store.artifact(self.instance, id)?;
                let editor = self.artifact_editor.as_ref().filter(|e| e.artifact == id)?;
                Some(self.render_artifact_panel(a, editor, cx).into_any_element())
            }
            Panel::Schedule(id) => {
                let s = self.store.schedule(self.instance, id)?;
                Some(self.render_schedule_panel(s, cx).into_any_element())
            }
            Panel::Listener(id) => {
                let l = self.store.listener(self.instance, id)?;
                Some(self.render_listener_panel(l, cx).into_any_element())
            }
            Panel::NewListener => self.render_listener_form(cx),
        }
    }

    /// Goal 上指定種類的 artifact（Brief、Ledger）。
    fn goal_artifact(&self, goal: GoalId, kind: ArtifactKind) -> Option<ArtifactId> {
        self.current_project()?
            .artifacts
            .iter()
            .find(|a| a.goal == Some(goal) && a.kind == kind)
            .map(|a| a.id)
    }

    fn open_artifact_button(
        &self,
        id: &'static str,
        label: &'static str,
        artifact: ArtifactId,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        secondary_button(id, label)
            .self_start()
            .text_size(px(13.))
            .px(px(12.))
            .py(px(5.))
            .on_click(
                cx.listener(move |this, _, window, cx| this.open_artifact(artifact, window, cx)),
            )
    }

    /// 用系統瀏覽器開 PR。假資料的網址是假的，只提示。
    pub fn open_link(&mut self, url: String, cx: &mut Context<Self>) {
        if self.live.is_some() {
            cx.open_url(&url);
        } else {
            self.toast = Some(format!("假資料模式不開啟連結：{url}"));
        }
        cx.notify();
    }

    /// 任務的 PR：有網址時是連結。`key` 讓同一個 PR 在不同地方有不同的元素 ID。
    fn pr_link(&self, key: &str, t: &TaskView, cx: &mut Context<Self>) -> Option<AnyElement> {
        let pr = t.pr.clone()?;
        let label = pr.strip_prefix("github.com/").unwrap_or(&pr).to_string();
        let el = div()
            .id(SharedString::from(format!("{key}-pr-{}", t.id.0)))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(4.))
            .child(icon(IconName::GitPullRequest, 12., theme::accent()))
            .child(label);
        Some(match t.pr_url.clone() {
            Some(url) => el
                .text_color(theme::accent())
                .cursor_pointer()
                .hover(|s| s.opacity(0.8))
                .on_click(cx.listener(move |this, _, _, cx| {
                    // 不要同時打開任務 thread。
                    cx.stop_propagation();
                    this.open_link(url.clone(), cx);
                }))
                .into_any_element(),
            None => el.into_any_element(),
        })
    }

    /// 執行者 · 分支 · PR。
    fn task_meta(&self, key: &str, t: &TaskView, cx: &mut Context<Self>) -> Div {
        let mono = cx.theme().mono_font_family.clone();
        let mut parts: Vec<AnyElement> = vec![div().child(t.assignee.clone()).into_any_element()];
        if let Some(branch) = t.branch.clone() {
            parts.push(
                div()
                    .min_w_0()
                    .truncate()
                    .font_family(mono)
                    .child(branch)
                    .into_any_element(),
            );
        }
        parts.extend(self.pr_link(key, t, cx));
        // 換行時分隔點跟著後一段走，不會留在行尾。
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_x(px(6.))
            .gap_y(px(2.))
            .text_size(px(12.))
            .text_color(theme::text_muted())
            .children(parts.into_iter().enumerate().map(|(i, part)| {
                div()
                    .flex()
                    .min_w_0()
                    .items_center()
                    .gap(px(6.))
                    .when(i > 0, |d| d.child("·"))
                    .child(part)
            }))
    }

    pub(super) fn close_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                    .when(g.quick, |d| {
                        d.child(tag("快速修改", theme::violet(), theme::violet_soft()).flex_none())
                    })
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
                        // 進行中的 Goal 裡需要你決定或失敗的任務可以直接重試（不打開任務 thread）。
                        let retry = (g.state.is_active() && retryable(&t.state)).then(|| {
                            secondary_button(
                                SharedString::from(format!("task-retry-{}", tid.0)),
                                "重試",
                            )
                            .flex_none()
                            .text_size(px(12.))
                            .px(px(10.))
                            .py(px(3.))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.retry_task(tid, cx);
                            }))
                        });
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
                                        .gap(px(3.))
                                        .child(
                                            div()
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .child(t.title.clone()),
                                        )
                                        .child({
                                            let (detail, color) = task_detail(t);
                                            div()
                                                .text_size(px(13.))
                                                .text_color(color)
                                                .line_height(relative(1.4))
                                                .line_clamp(3)
                                                .child(detail)
                                        })
                                        .child(self.task_meta("row", t, cx)),
                                )
                                .children(elapsed)
                                .children(retry)
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
                        .child(white_box().children(g.team.iter().rev().take(3).map(team_post)));
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
                .children(
                    self.goal_artifact(gid, ArtifactKind::Brief)
                        .map(|a| self.open_artifact_button("goal-open-brief", "在編輯器開啟", a, cx)),
                )
                .into_any_element(),
            GoalTab::Ledger => match self.goal_artifact(gid, ArtifactKind::Ledger) {
                Some(a) => white_box()
                    .child(
                        div()
                            .line_height(relative(1.6))
                            .child("Planner 的工作帳本：拆了哪些任務、各自結果與待辦。你在裡面取消勾選某項，Planner 下一輪會看到。"),
                    )
                    .child(self.open_artifact_button("goal-open-ledger", "開啟 Ledger", a, cx))
                    .into_any_element(),
                None => empty_state(
                    "還沒有 Ledger",
                    "Planner 拆完任務後會建立 Ledger，記錄決定、嘗試與花費。",
                )
                .into_any_element(),
            },
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
                        .children(g.team.iter().rev().map(team_post))
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

        // 第一行：狀態與耗時、花費；第二行：現在怎樣了；第三行：執行者、分支與 PR。
        let (detail, detail_color) = task_detail(t);
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
                    .child(format!(
                        "{} 分鐘 · ${}.{:02}",
                        t.elapsed_min,
                        t.cost_cents / 100,
                        t.cost_cents % 100
                    )),
            )
            .child(
                div()
                    .text_color(detail_color)
                    .line_height(relative(1.4))
                    .child(detail),
            )
            .child(self.task_meta("thread", t, cx));

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
                    .when(g.state.is_active() && retryable(&t.state), |d| {
                        d.child(
                            secondary_button("task-retry", "重試")
                                .text_size(px(13.))
                                .px(px(12.))
                                .py(px(5.))
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.retry_task(tid, cx)),
                                ),
                        )
                    })
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
