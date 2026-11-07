//! 總管：同一個實例內跨 project 的永久對話，依目前選取的實例切換。
//! 版面和 project 頻道相同：中欄是對話，右欄是專案概況與總管記得的事。

use gpui_kit::assets::IconName;
use gpui_kit::component::input::Textarea;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px, relative,
};

use super::channel::empty_state;
use super::stream::Density;
use super::widgets::*;
use crate::app::PotsApp;
use crate::theme;

impl PotsApp {
    pub fn render_overseer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let inst = self.current_instance();
        let ov = &inst.overseer;

        let header = div()
            .flex()
            .items_center()
            .gap(px(12.))
            .px(px(24.))
            .py(px(14.))
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex_none()
                    .size(px(32.))
                    .rounded(px(8.))
                    .bg(theme::violet_soft())
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(icon(IconName::Sparkles, 16., theme::violet())),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_weight(FontWeight::BOLD)
                            .child("總管"),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(theme::text_muted())
                            .child("看得到所有專案，派工只會在某個專案開 Goal"),
                    ),
            )
            .child(div().ml_auto().child(agent_state_pill(ov.state)));

        let stream: AnyElement = if ov.items.is_empty() {
            empty_state(
                format!("{}的總管", inst.name),
                "問任何專案的進度、找過去的調查，或請它在某個專案開 Goal、暫停自動化。",
            )
            .into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .pt(px(16.))
                .pb(px(8.))
                .children(
                    ov.items
                        .iter()
                        .map(|item| self.render_stream_item(item, Density::Regular, cx)),
                )
                .into_any_element()
        };

        let composer = div()
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
                    .child(Textarea::new(&self.overseer_input).appearance(false)),
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
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(13.))
                            .text_color(theme::text_muted())
                            .child("用 # 指定專案 · /pause 暫停自動化 · /summary 摘要"),
                    )
                    .child(send_button("overseer-send", 36.).on_click(
                        cx.listener(|this, _, window, cx| this.send_overseer(window, cx)),
                    )),
            );

        div()
            .flex_1()
            .min_w(px(420.))
            .h_full()
            .flex()
            .flex_col()
            .bg(theme::white())
            .child(header)
            .child(
                div()
                    .id("overseer-stream")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.overseer_scroll)
                    .child(stream),
            )
            .child(composer)
    }

    pub fn render_overseer_aside(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let inst = self.current_instance();
        let ov = &inst.overseer;

        div()
            .id("overseer-aside")
            .flex_none()
            .w(px(340.))
            .h_full()
            .overflow_y_scroll()
            .bg(theme::panel_bg())
            .border_l_1()
            .border_color(theme::border())
            .px(px(20.))
            .py(px(16.))
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(section_label("專案概況").pt(px(0.)))
            .children(ov.overview.iter().map(|o| {
                let (fg, _) = agent_state_colors(o.state);
                let name = o.project.clone();
                let project = inst
                    .projects
                    .iter()
                    .find(|p| p.name == o.project)
                    .map(|p| p.id);
                let instance = inst.id;
                div()
                    .id(gpui_kit::SharedString::from(format!("overview-{name}")))
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .p(px(12.))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(theme::border())
                    .bg(theme::white())
                    .cursor_pointer()
                    .hover(|s| s.border_color(theme::border_strong()))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(format!("# {name}")),
                            )
                            .child(
                                div()
                                    .ml_auto()
                                    .flex()
                                    .items_center()
                                    .gap(px(5.))
                                    .text_size(px(12.))
                                    .text_color(fg)
                                    .child(dot(fg, 6.))
                                    .child(o.state.label()),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(theme::text_muted())
                            .child(o.note.clone()),
                    )
                    .when_some(project, |d, pid| {
                        d.on_click(cx.listener(move |this, _, window, cx| {
                            this.open_project(instance, pid, window, cx)
                        }))
                    })
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(section_label("總管記得的事").pt(px(16.)))
                    .when(ov.memory_total > 0, |d| {
                        d.child(
                            div()
                                .ml_auto()
                                .pt(px(12.))
                                .text_size(px(12.))
                                .text_color(theme::accent())
                                .child(format!("全部 {} 條", ov.memory_total)),
                        )
                    }),
            )
            .when(ov.memory.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme::text_muted())
                        .child("還沒有觀察紀錄。"),
                )
            })
            .children(ov.memory.iter().map(|m| {
                div()
                    .flex()
                    .gap(px(8.))
                    .text_size(px(13.))
                    .line_height(relative(1.5))
                    .child(tag(m.priority.label(), theme::chip_text(), theme::chip_bg()).mt(px(2.)))
                    .child(div().flex_1().min_w_0().child(m.text.clone()))
            }))
    }
}
