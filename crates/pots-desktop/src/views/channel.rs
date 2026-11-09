//! 中欄：project 頻道（頂端狀態、分頁、對話流、輸入框）。

use gpui_kit::assets::IconName;
use gpui_kit::component::input::Textarea;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, Div, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px, relative,
};

use super::stream::Density;
use super::widgets::*;
use crate::app::{ChannelTab, PotsApp};
use crate::model::*;
use crate::theme;

pub fn empty_state(title: impl Into<SharedString>, body: impl Into<SharedString>) -> Div {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .p(px(40.))
        .text_color(theme::text_muted())
        .child(
            div()
                .text_size(px(15.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::text())
                .child(title.into()),
        )
        .child(
            div()
                .max_w(px(420.))
                .text_size(px(13.))
                .line_height(relative(1.5))
                .child(body.into()),
        )
}

impl PotsApp {
    pub fn render_channel(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(p) = self.current_project() else {
            return div().flex_1().into_any_element();
        };
        let busy = p.agent_state.is_busy();

        let tab_item =
            |id: &'static str, label: String, tab: ChannelTab, cx: &mut Context<Self>| {
                super::widgets::tab(id, label, self.channel_tab == tab).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.channel_tab = tab;
                        cx.notify();
                    },
                ))
            };

        let header = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .px(px(24.))
            .pt(px(12.))
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_weight(FontWeight::BOLD)
                            .child(format!("# {}", p.name)),
                    )
                    .child(agent_state_pill(p.agent_state))
                    .child(
                        div()
                            .ml_auto()
                            .text_size(px(13.))
                            .text_color(theme::text_muted())
                            // 還沒有預算的實例（M6 之前）不顯示。
                            .when(p.budget.limit_cents > 0, |d| d.child(p.budget.label())),
                    )
                    .when(busy, |d| {
                        d.child(
                            div()
                                .id("stop-agent")
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .px(px(12.))
                                .py(px(6.))
                                .rounded(px(6.))
                                .border_1()
                                .border_color(theme::border_strong())
                                .text_size(px(13.))
                                .cursor_pointer()
                                .hover(|s| s.bg(theme::subtle_bg()))
                                .child(icon(IconName::Square, 12., theme::text()))
                                .child("停止")
                                .on_click(cx.listener(|this, _, _, cx| this.stop_agent(cx))),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .gap(px(20.))
                    .child(tab_item("tab-chat", "對話".into(), ChannelTab::Chat, cx))
                    .child(tab_item(
                        "tab-goals",
                        format!("Goals {}", p.goals.len()),
                        ChannelTab::Goals,
                        cx,
                    ))
                    .child(tab_item(
                        "tab-artifacts",
                        format!("Artifacts {}", p.artifacts.len()),
                        ChannelTab::Artifacts,
                        cx,
                    ))
                    .child(tab_item(
                        "tab-automations",
                        format!("自動化 {}", self.current_instance().automation_count(p.id)),
                        ChannelTab::Automations,
                        cx,
                    ))
                    .child(tab_item(
                        "tab-memory",
                        if p.memory.observations.is_empty() {
                            "記憶".to_string()
                        } else {
                            format!("記憶 {}", p.memory.observations.len())
                        },
                        ChannelTab::Memory,
                        cx,
                    )),
            );

        let body: AnyElement = match self.channel_tab {
            ChannelTab::Chat => div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                // onboarding 沒做完：worker 不會派工，與其等 agent 報錯，不如在這裡說清楚。
                .when(p.workdir.is_none(), |d| {
                    let project = p.id;
                    d.child(
                        div()
                            .mx(px(24.))
                            .mt(px(12.))
                            .p(px(10.))
                            .rounded(px(8.))
                            .border_1()
                            .border_color(theme::warn_border())
                            .bg(theme::warn_soft())
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .text_size(px(13.))
                            .child(icon(IconName::Info, 14., theme::warn_text()))
                            .child(
                                div()
                                    .flex_1()
                                    .text_color(theme::warn_text())
                                    .child(
                                        "這個 project 還沒有工作資料夾：worker 不會派工。\
                                         告訴 agent「我們用這個 repo」或指定本機資料夾，也可以直接設定。",
                                    ),
                            )
                            .child(
                                link_button("workdir-set", "設定").on_click(
                                    cx.listener(move |this, _, window, cx| {
                                        this.edit_workdir(project, window, cx)
                                    }),
                                ),
                            ),
                    )
                })
                .child(self.render_project_stream(p, cx))
                .child(self.render_composer(p, window, cx))
                .into_any_element(),
            ChannelTab::Goals => self.render_goal_list(p, cx).into_any_element(),
            ChannelTab::Artifacts => self.render_artifact_list(p, cx),
            ChannelTab::Automations => self.render_project_automations(p, cx),
            ChannelTab::Memory => self.render_memory_tab(p, cx),
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
            .into_any_element()
    }

    fn render_project_stream(&self, p: &ProjectView, cx: &mut Context<Self>) -> impl IntoElement {
        let items: Vec<AnyElement> = if p.items.is_empty() {
            vec![
                empty_state(
                    format!("這是 # {} 的開頭", p.name),
                    "每個 project 只有一條對話。討論收斂時 agent 會開一張 Goal 卡片，細節掛在卡片的 thread 下。",
                )
                .into_any_element(),
            ]
        } else {
            p.items
                .iter()
                .map(|item| self.render_stream_item(item, Density::Regular, cx))
                .collect()
        };
        div()
            .id("project-stream")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.stream_scroll)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .pt(px(16.))
                    .pb(px(8.))
                    .children(items),
            )
    }

    fn render_composer(
        &self,
        p: &ProjectView,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mode = self.send_mode;
        let segment =
            |id: &'static str, label: &'static str, value: SendMode, cx: &mut Context<Self>| {
                let selected = mode == value;
                div()
                    .id(id)
                    .px(px(12.))
                    .py(px(5.))
                    .text_size(px(13.))
                    .cursor_pointer()
                    .when_else(
                        selected,
                        |d| d.bg(theme::text()).text_color(theme::white()),
                        |d| {
                            d.bg(theme::white())
                                .text_color(theme::text())
                                .hover(|s| s.bg(theme::subtle_bg()))
                        },
                    )
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.send_mode = value;
                        cx.notify();
                    }))
            };
        let hint = if p.agent_state.is_busy() {
            match mode {
                SendMode::Queue => "agent 正在忙，訊息會等本輪結束再處理",
                SendMode::Steer => "會中斷目前的執行，讓 agent 重新規劃",
            }
        } else {
            "/fix 快速修改 · /goal 開 Goal · /schedule 排程"
        };

        div()
            .mx(px(24.))
            .mb(px(20.))
            .rounded(px(10.))
            .border_1()
            .border_color(theme::input_border())
            .flex()
            .flex_col()
            .child(
                div()
                    .px(px(4.))
                    .pt(px(6.))
                    .text_size(px(15.))
                    .child(Textarea::new(&self.composer).appearance(false)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(10.))
                    .py(px(8.))
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .rounded(px(6.))
                            .border_1()
                            .border_color(theme::border_strong())
                            .overflow_hidden()
                            .child(segment("mode-queue", "排隊", SendMode::Queue, cx))
                            .child(segment("mode-steer", "改方向", SendMode::Steer, cx)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(13.))
                            .text_color(theme::text_muted())
                            .truncate()
                            .child(hint),
                    )
                    .child(send_button("send", 36.).on_click(
                        cx.listener(|this, _, window, cx| this.send_composer(window, cx)),
                    )),
            )
    }

    fn render_goal_list(&self, p: &ProjectView, cx: &mut Context<Self>) -> impl IntoElement {
        if p.goals.is_empty() {
            return empty_state(
                "沒有進行中的 Goal",
                "在對話裡描述你想做的事，討論收斂時 agent 會開 Goal。",
            )
            .into_any_element();
        }
        div()
            .id("goal-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p(px(24.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .children(
                p.goals
                    .iter()
                    .map(|g| self.render_goal_card(g, cx).into_any_element()),
            )
            .into_any_element()
    }
}
