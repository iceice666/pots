//! 對話流的訊息與內嵌卡片；project 頻道、總管與任務 thread 共用。

use gpui_kit::assets::IconName;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, Div, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, div, px, relative,
};

use super::text::{
    agent_stream, agent_text, agent_text_muted, agent_text_prefixed, fact_note, milestone_parts,
};
use super::widgets::*;
use crate::app::{AutomationSection, CardKey, GoalTab, Panel, PotsApp};
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

/// Goal 卡片最後一行：Planner 備註（結束的 Goal 是結果）的「前綴」與「原文」。
///
/// 「已完成：」「已取消：」是平台的字，後面的 summary／reason 才是模型或使用者寫的
/// Markdown；前綴留純文字，只有原文交給 Markdown 渲染（見 [`agent_text_prefixed`]）。
/// 預算用量那一行由呼叫端另外接（純數字，沒有 Markdown）。
pub fn goal_note_parts(goal: &GoalView) -> (&'static str, &str) {
    match &goal.state {
        GoalState::Draft | GoalState::Active => ("", goal.planner_note.as_str()),
        GoalState::Done { summary } => ("已完成：", summary.as_str()),
        GoalState::Cancelled { reason } => ("已取消：", reason.as_str()),
    }
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
                        .child(self.render_card(item.id, 0, card, cx)),
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
        // 每一行都是模型自己寫的一句話（可能含行內 Markdown），逐行渲染；顏色沿用外層的淡色。
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
                .children(msg.narration.iter().enumerate().map(|(i, line)| {
                    agent_text(
                        SharedString::from(format!("msg-narration-{}-{i}", id.0)),
                        line.clone(),
                    )
                    .text_size(px(density.body() - 1.))
                    .line_height(relative(1.5))
                }))
        });

        let body = (!msg.text.is_empty()).then(|| {
            // 訊息內文是 Markdown。串流中由 helper 收尾未閉合的區塊再接游標，
            // 游標才不會被吸進 code fence／行內 code span 裡。
            let text_id = SharedString::from(format!("msg-body-{}", id.0));
            let text = if streaming {
                agent_stream(text_id, &msg.text)
            } else {
                agent_text(text_id, msg.text.clone())
            };
            // 右欄 thread 的密度比頻道緊湊，字級沿用這裡的密度設定。
            text.text_size(px(density.body()))
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
                    .children(
                        msg.cards
                            .iter()
                            .enumerate()
                            .map(|(i, c)| self.render_card(id, i, c, cx)),
                    ),
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

    /// 收成一行的卡片：右箭頭加上一行摘要，點一下展開。`content` 是橫向排開的摘要。
    fn collapsed_card(
        &self,
        scope: &str,
        key: CardKey,
        max_w: f32,
        content: Div,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        card_frame(max_w)
            .id(SharedString::from(format!("collapsed-{scope}-{key:?}")))
            .flex_row()
            .items_center()
            .gap(px(8.))
            .px(px(14.))
            .py(px(8.))
            .rounded(px(10.))
            .text_size(px(13.))
            .cursor_pointer()
            .hover(|s| s.bg(theme::subtle_bg()))
            .child(icon(IconName::ChevronRight, 14., theme::text_muted()))
            .child(content.flex_1().min_w_0().flex().items_center().gap(px(8.)))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_card(key, cx)))
            .into_any_element()
    }

    /// 展開後的收合卡片右上角的「收起」。
    fn collapse_button(&self, scope: &str, key: CardKey, cx: &mut Context<Self>) -> Stateful<Div> {
        div()
            .id(SharedString::from(format!("collapse-{scope}-{key:?}")))
            .flex_none()
            .size(px(22.))
            .rounded(px(4.))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .hover(|s| s.bg(theme::track()))
            .child(icon(IconName::ChevronDown, 14., theme::text_muted()))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.toggle_card(key, cx)
            }))
            .tip("收起")
    }

    /// 渲染一張卡片。`index` 是它在同一則訊息裡的序號：同一則訊息可能帶多張卡片
    /// （例如 milestone + 核准），卡片內部的元素 id 只靠訊息 id 會撞在一起。
    pub fn render_card(
        &self,
        item: ItemId,
        index: usize,
        card: &Card,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // 同一個 Goal 可能同時出現在對話卡片與右欄列表，元素 id 要加上來源才唯一。
        let scope = SharedString::from(format!("msg-{}-{index}", item.0));
        match card {
            Card::GoalCreated { goal } => match self.store.goal(self.instance, *goal) {
                Some(g) => self.render_goal_card(&scope, g, cx).into_any_element(),
                None => muted("Goal 已不存在").into_any_element(),
            },
            // listener 依外部事件開的 Goal：上面註明是哪個事件觸發的。
            Card::Triggered { goal, event, .. } => div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(muted(format!("listener 觸發：{event}")))
                .child(match self.store.goal(self.instance, *goal) {
                    Some(g) => self.render_goal_card(&scope, g, cx).into_any_element(),
                    None => muted("Goal 已不存在").into_any_element(),
                })
                .into_any_element(),
            Card::Approval { id } => match self.store.approval(*id) {
                Some(a) => self.render_approval_card(a, &scope, cx).into_any_element(),
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
                    .id(SharedString::from(format!("artifact-{}", scope)))
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
                    .id(SharedString::from(format!("milestone-{}", scope)))
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
                            .child({
                                // 「Goal 完成：」是伺服器加的前綴，summary 才是 Planner 寫的
                                // Markdown；其他里程碑整段都是它的原文。卡片原本吃 14px。
                                let (prefix, body) = milestone_parts(text);
                                let body = agent_text(
                                    SharedString::from(format!("milestone-text-{}", scope)),
                                    body.to_string(),
                                )
                                .text_size(px(14.));
                                div()
                                    .text_size(px(14.))
                                    .child(agent_text_prefixed(prefix, body))
                            }),
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
                // 原本是 14px／1.5 行高的摘要內文。
                .child(
                    agent_text(
                        SharedString::from(format!("catchup-summary-{}", scope)),
                        summary.clone(),
                    )
                    .text_size(px(14.))
                    .line_height(relative(1.5)),
                )
                .into_any_element(),
            Card::Digest {
                title,
                rows,
                footer,
            } => {
                card_frame(560.)
                    .gap(px(10.))
                    .px(px(16.))
                    .py(px(14.))
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::BOLD)
                            .child(title.clone()),
                    )
                    .child(div().flex().flex_col().gap(px(6.)).children(
                        rows.iter().enumerate().map(|(i, (k, v))| {
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
                                // 右邊是 agent 寫的一句話，可能帶行內 Markdown；原本是
                                // 14px 的淡色字（`TextView` 預設 15px，這裡壓回去）。
                                .child(
                                    agent_text_muted(
                                        SharedString::from(format!("digest-row-{}-{i}", scope)),
                                        v.clone(),
                                    )
                                    .text_size(px(14.)),
                                )
                        }),
                    ))
                    .child(agent_text_muted(
                        SharedString::from(format!("digest-footer-{}", scope)),
                        footer.clone(),
                    ))
                    .into_any_element()
            }
            Card::Notice {
                title,
                body,
                actions,
            } => {
                // 已經恢復就不再顯示「立即恢復」。
                let actions: Vec<(usize, NoticeAction)> = actions
                    .iter()
                    .copied()
                    .enumerate()
                    .filter(|(_, a)| {
                        *a != NoticeAction::ResumeAutomations
                            || self.current_instance().automation_pause.is_some()
                    })
                    .collect();
                // 沒有可按的動作只是一則回報，預設收成一行。
                let key = CardKey::Notice(item, index);
                let collapsible = actions.is_empty();
                if collapsible && !self.expanded_cards.contains(&key) {
                    return self.collapsed_card(
                        &scope,
                        key,
                        560.,
                        div()
                            .child(icon(IconName::CircleCheck, 14., theme::ok()))
                            .child(
                                div()
                                    .flex_none()
                                    .max_w(relative(0.6))
                                    .truncate()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(title.clone()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(theme::text_muted())
                                    .child(body.clone()),
                            ),
                        cx,
                    );
                }
                card_frame(560.)
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
                            .child(title.clone())
                            .when(collapsible, |d| {
                                d.child(self.collapse_button(&scope, key, cx).ml_auto())
                            }),
                    )
                    .child(agent_text_muted(
                        SharedString::from(format!("notice-body-{}", scope)),
                        body.clone(),
                    ))
                    .child(div().flex().gap(px(4.)).children(actions.into_iter().map(
                        |(i, action)| {
                            link_button(
                                SharedString::from(format!("notice-{}-{i}", scope)),
                                action.label(),
                            )
                            .px(px(0.))
                            .mr(px(12.))
                            .on_click(cx.listener(
                                move |this, _, _, cx| match action {
                                    NoticeAction::ViewAutomations => {
                                        this.open_automations(AutomationSection::Schedules, cx)
                                    }
                                    NoticeAction::ResumeAutomations => this.resume_automations(cx),
                                },
                            ))
                        },
                    )))
                    .into_any_element()
            }
        }
    }

    /// 對話裡的 Goal 卡片：已結束的 Goal 預設收成一行（標題與結果）。
    pub fn render_goal_card(
        &self,
        scope: &str,
        goal: &GoalView,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = CardKey::Goal(goal.id);
        if goal.state.is_open() || self.expanded_cards.contains(&key) {
            return self.goal_card(scope, goal, true, cx).into_any_element();
        }
        let outcome = match &goal.state {
            GoalState::Done { summary } => summary.clone(),
            GoalState::Cancelled { reason } => reason.clone(),
            GoalState::Draft | GoalState::Active => String::new(),
        };
        self.collapsed_card(
            scope,
            key,
            560.,
            div()
                .child(tag("GOAL", theme::accent(), theme::accent_soft()))
                .child(tag(goal.state.label(), theme::text_muted(), theme::track()))
                .child(
                    div()
                        .flex_none()
                        .max_w(relative(0.6))
                        .truncate()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(goal.title.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(theme::text_muted())
                        .child(outcome),
                ),
            cx,
        )
    }

    /// Goal 卡片。`hint` 是對話裡的「在右側開啟」提示；右欄的 Goals 列表本身就在右側，不顯示。
    /// `scope` 讓同一個 Goal 出現在不同容器時（對話卡片、右欄列表）元素 id 不撞。
    /// 對話裡已結束的 Goal 是使用者點開的，右上角多一個「收起」。
    pub(super) fn goal_card(
        &self,
        scope: &str,
        goal: &GoalView,
        hint: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let gid = goal.id;
        let open = matches!(self.panel, Some(Panel::Goal(g, _)) if g == gid);
        card_frame(560.)
            // 同一個 Goal 可能同時出現在對話卡片與右欄 Goals 列表，`scope` 讓兩張卡的
            // 元素 id 不撞（原本只有 `goal-card-{gid}`，兩個容器同畫面時會重複）。
            .id(SharedString::from(format!("goal-card-{scope}-{}", gid.0)))
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
                    .when(goal.env, |d| {
                        d.child(tag("環境分析", theme::ok(), theme::ok_soft()).flex_none())
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
                    })
                    .when(hint && !goal.state.is_open(), |d| {
                        d.child(self.collapse_button(scope, CardKey::Goal(gid), cx))
                    }),
            )
            .when(!goal.state.is_draft(), |d| {
                d.child(goal_progress(goal)).child(goal_counts(goal))
            })
            .child({
                // 「已完成：」「已取消：」是平台的前綴，只有後面的 summary／reason 是
                // agent 或使用者寫的 Markdown；有預算時多一行淡色的用量。
                let (prefix, body) = goal_note_parts(goal);
                div()
                    .flex()
                    .flex_col()
                    .text_size(px(13.))
                    .text_color(theme::text_muted())
                    .child(agent_text_prefixed(
                        prefix,
                        agent_text(
                            SharedString::from(format!("goal-card-note-{scope}-{}", gid.0)),
                            body.to_string(),
                        )
                        .text_size(px(13.))
                        .text_color(theme::text_muted()),
                    ))
                    .when(goal.budget_used_pct > 0, |d| {
                        d.child(format!("預算已用 {}%", goal.budget_used_pct))
                    })
            })
            .when(goal.state.is_draft(), |d| {
                d.child(self.render_draft_actions("card", &format!("card-{scope}"), gid, cx))
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                let tab = if this
                    .store
                    .goal(this.instance, gid)
                    .is_some_and(|g| g.state.is_draft())
                {
                    GoalTab::Brief
                } else {
                    GoalTab::Tasks
                };
                this.open_panel(Some(Panel::Goal(gid, tab)), cx)
            }))
    }

    /// 草稿 Goal 的按鈕：開始（交給 Planner）、看 Brief（只在卡片上）、捨棄。
    ///
    /// `scope` 決定要不要顯示「看 Brief」（只有卡片需要，右欄 Goal 面板本身就有那個分頁）；
    /// `key` 是元素 id 的前綴，同一個 Goal 同時出現在對話卡片與右欄列表時靠它區分。
    pub fn render_draft_actions(
        &self,
        scope: &str,
        key: &str,
        gid: GoalId,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                primary_button(
                    SharedString::from(format!("{key}-goal-start-{}", gid.0)),
                    "開始",
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.start_goal(gid, cx);
                })),
            )
            // 右欄的 Goal 面板本身就有 Brief 分頁，只有卡片上需要這個按鈕。
            .when(scope == "card", |d| {
                d.child(
                    secondary_button(
                        SharedString::from(format!("{key}-goal-brief-{}", gid.0)),
                        "看 Brief",
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.open_panel(Some(Panel::Goal(gid, GoalTab::Brief)), cx)
                    })),
                )
            })
            .child(
                secondary_button(
                    SharedString::from(format!("{key}-goal-discard-{}", gid.0)),
                    "捨棄",
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.discard_goal(gid, cx);
                })),
            )
    }

    /// agent 問題的選項：一個選項一個按鈕，按下就回答。不是問題、或已經回答時是 `None`。
    pub fn render_question_options(
        &self,
        a: &InboxItem,
        scope: &str,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let (_, options, _) = super::preferences::question_of(a)?;
        if !a.is_open() || options.is_empty() {
            return None;
        }
        let id = a.id;
        Some(div().flex().flex_col().items_start().gap(px(8.)).children(
            options.iter().enumerate().map(|(i, o)| {
                let key = SharedString::from(format!("{scope}-opt-{}-{i}", id.0));
                let answer = o.clone();
                super::widgets::option_button(key, o.clone()).on_click(
                    cx.listener(move |this, _, _, cx| this.answer_option(id, answer.clone(), cx)),
                )
            }),
        ))
    }

    /// 核准／ask_user 卡片：處理完的預設收成一行（結果與標題），點開才看說明與 diff。`scope` 是它在畫面上的位置（`render_card` 給的
    /// `msg-{訊息}-{序號}`）：同一筆核准可能同時出現在 thread 與頻道、或同一則訊息裡
    /// 兩張卡，`TextView` 的 id 只靠 approval id 會讓 `use_keyed_state` 共用同一份 state。
    pub fn render_approval_card(
        &self,
        a: &InboxItem,
        scope: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = a.id;
        let question = super::preferences::question_of(a);
        let (banner, banner_fg, banner_bg, border) = match &a.decision {
            Decision::Pending if question.is_some() => (
                a.approval
                    .as_ref()
                    .map(|v| format!("{}問你", v.requested_by))
                    .unwrap_or_else(|| "需要你回答".into()),
                theme::accent_text(),
                theme::accent_soft(),
                theme::accent_text(),
            ),
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
        let title = question
            .map(|(q, _, _)| q.to_string())
            .unwrap_or_else(|| a.title.clone());
        let key = CardKey::Approval(id);
        if !pending && !self.expanded_cards.contains(&key) {
            return self.collapsed_card(
                scope,
                key,
                560.,
                div()
                    .child(
                        div()
                            .flex_none()
                            .max_w(relative(0.6))
                            .truncate()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(banner_fg)
                            .child(banner),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(title)),
                cx,
            );
        }

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
                    })
                    .when(!pending, |d| {
                        d.child(self.collapse_button(scope, key, cx).ml_auto())
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .px(px(16.))
                    .py(px(14.))
                    .child(match question {
                        // ask_user 的問題是 agent 寫的 Markdown：字級與粗體沿用標題樣式。
                        Some((q, _, _)) => agent_text(
                            SharedString::from(format!("approval-question-{scope}-{}", id.0)),
                            q.to_string(),
                        )
                        .text_size(px(16.))
                        .font_weight(FontWeight::BOLD)
                        .line_height(relative(1.4))
                        .into_any_element(),
                        None => div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::BOLD)
                            .line_height(relative(1.4))
                            .child(a.title.clone())
                            .into_any_element(),
                    })
                    // ask_user 的問題與核准的理由都是 agent 寫的 Markdown。
                    .when(!a.detail.is_empty() && a.approval.is_some(), |d| {
                        // 原本是 14px／1.5 行高的理由內文。
                        d.child(
                            agent_text(
                                SharedString::from(format!("approval-detail-{scope}-{}", id.0)),
                                a.detail.clone(),
                            )
                            .text_size(px(14.))
                            .line_height(relative(1.5)),
                        )
                    })
                    .child(div().flex().flex_col().gap(px(4.)).children(
                        a.facts.iter().enumerate().map(|(i, f)| {
                            // facts 是平台組的字（PR、sha、動作）：只有拒絕／回答時
                            // 附上的「你的理由：」後面那段才是使用者寫的原文。
                            match fact_note(f) {
                                Some((prefix, note)) => agent_text_prefixed(
                                    prefix,
                                    agent_text_muted(
                                        SharedString::from(format!(
                                            "approval-fact-{scope}-{}-{i}",
                                            id.0
                                        )),
                                        note,
                                    ),
                                )
                                .text_size(px(13.))
                                .into_any_element(),
                                None => muted(f.clone()).into_any_element(),
                            }
                        }),
                    ))
                    .when_some(super::preferences::approval_diff(a), |d, diff| {
                        d.child(super::preferences::diff_block(
                            diff,
                            cx.theme().mono_font_family.clone(),
                        ))
                    })
                    .children(self.render_question_options(a, scope, cx))
                    .when(pending, |d| {
                        d.child(self.render_actions(id, &a.actions, scope, cx))
                    }),
            )
            .into_any_element()
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
                    (_, InboxAction::Reject | InboxAction::CancelTask | InboxAction::Skip) => {
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
