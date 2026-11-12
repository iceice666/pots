//! project 的右欄「記憶」：對話 agent 記得什麼（Observational Memory 的觀察紀錄）。
//! 可以釘選、修正或刪除觀察；修正與刪除在下一次回覆生效，原本的內容服務會保留作稽核。

use gpui_kit::assets::IconName;
use gpui_kit::component::input::Input;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, div, px, relative,
};

use super::channel::empty_state;
use super::text::{agent_text, agent_text_prefixed};
use super::widgets::{icon, link_button, primary_button, tag};
use crate::app::{PotsApp, SettingsTab};
use crate::model::*;
use crate::theme;

fn priority_tag(p: Priority) -> gpui_kit::Div {
    match p {
        Priority::High => tag(p.label(), theme::accent_text(), theme::accent_soft()),
        Priority::Medium => tag(p.label(), theme::chip_text(), theme::chip_bg()),
        Priority::Low => tag(p.label(), theme::text_muted(), theme::subtle_bg()),
    }
}

impl PotsApp {
    pub fn render_memory_tab(&self, p: &ProjectView, cx: &mut Context<Self>) -> AnyElement {
        let memory = &p.memory;
        let mut body = div().flex().flex_col().gap(px(12.)).child(
            div()
                .text_size(px(13.))
                .text_color(theme::text_muted())
                .line_height(relative(1.5))
                .child(
                    "Observer 在背景把較早的對話整理成觀察，對話 agent 每次回覆前都會讀到。\
                         釘選的觀察在重新整理時一定保留；修正與刪除從下一次回覆開始生效。",
                ),
        );

        if let Some(error) = &memory.error {
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(14.))
                    .py(px(10.))
                    .rounded(px(8.))
                    .bg(theme::warn_soft())
                    .text_color(theme::warn_text())
                    .text_size(px(13.))
                    .child(icon(IconName::CircleAlert, 16., theme::warn_text()))
                    .child(
                        div().flex_1().min_w_0().child(agent_text_prefixed(
                            "上次整理記憶失敗：",
                            agent_text(
                                SharedString::from("memory-error"),
                                error.clone(),
                            ),
                        )),
                    )
                    .child(link_button("memory-open-settings", "記憶設定").on_click(
                        cx.listener(|this, _, _, cx| this.open_settings(SettingsTab::Memory, cx)),
                    )),
            );
        }

        if memory.observations.is_empty() && memory.current_task.is_empty() {
            return div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .child(div().px(px(16.)).pt(px(16.)).child(body))
                .child(empty_state(
                    "還沒有觀察",
                    "對話累積到一定長度後（預設約 3 萬 token），Observer 會在背景整理出第一批觀察；\
                     最近的對話一直保留原文。",
                ))
                .into_any_element();
        }

        let mut stats = format!(
            "{} 條觀察 · 約 {} token",
            memory.observations.len(),
            memory.tokens
        );
        if memory.generation > 0 {
            stats.push_str(&format!(" · 已重新整理 {} 次", memory.generation));
        }
        body = body.child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .px(px(16.))
                .py(px(12.))
                .rounded(px(10.))
                .bg(theme::subtle_bg())
                .when(!memory.current_task.is_empty(), |d| {
                    d.child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .text_size(px(14.))
                            .child(
                                div()
                                    .flex_none()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("目前在做什麼"),
                            )
                            // 「目前在做什麼」是 Observer 寫的摘要，可能帶行內 Markdown。
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(14.))
                                    .child(agent_text(
                                        SharedString::from("memory-current-task"),
                                        memory.current_task.clone(),
                                    )),
                            ),
                    )
                })
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::text_muted())
                        .child(stats),
                ),
        );

        let mut list = div()
            .flex()
            .flex_col()
            .rounded(px(10.))
            .border_1()
            .border_color(theme::border());
        for (i, o) in memory.observations.iter().enumerate() {
            list = list.child(self.render_observation(o, i > 0, cx));
        }
        body = body.child(list);

        div()
            .id("memory-tab")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(16.))
            .py(px(16.))
            .child(body)
            .into_any_element()
    }

    fn render_observation(
        &self,
        o: &ObservationView,
        divider: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = o.id;
        let pinned = o.pinned;
        let key = |name: &str| SharedString::from(format!("observation-{name}-{id}"));
        let editing = self
            .observation_editor
            .as_ref()
            .filter(|e| e.id == id)
            .map(|e| e.input.clone());

        let content =
            match &editing {
                Some(input) => div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(Input::new(input))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(4.))
                            .child(primary_button(key("save"), "儲存").on_click(
                                cx.listener(|this, _, _, cx| this.save_observation_edit(cx)),
                            ))
                            .child(link_button(key("cancel"), "取消").on_click(
                                cx.listener(|this, _, _, cx| this.cancel_observation_edit(cx)),
                            )),
                    ),
                None => div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div().line_height(relative(1.5)).child(agent_text(
                            SharedString::from(format!("observation-text-{id}")),
                            o.text.clone(),
                        )),
                    )
                    .when(o.edited, |d| {
                        d.child(
                            div()
                                .text_size(px(12.))
                                .text_color(theme::text_muted())
                                .child("你修正過"),
                        )
                    }),
            };

        let actions =
            div()
                .ml_auto()
                .flex_none()
                .flex()
                .items_center()
                .when(editing.is_none(), |d| {
                    d.child(
                        link_button(key("pin"), if pinned { "取消釘選" } else { "釘選" }).on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.pin_observation(id, !pinned, cx)
                            }),
                        ),
                    )
                    .child(link_button(key("edit"), "修正").on_click(cx.listener(
                        move |this, _, window, cx| this.edit_observation(id, window, cx),
                    )))
                    .child(
                        link_button(key("delete"), "刪除")
                            .text_color(theme::danger())
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.delete_observation(id, cx)),
                            ),
                    )
                });

        // 右欄很窄：日期、優先度與操作一行，內容在下面佔滿寬度。
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .px(px(14.))
            .py(px(10.))
            .when(divider, |d| d.border_t_1().border_color(theme::border()))
            .when(pinned, |d| d.bg(theme::accent_soft()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(12.))
                            .text_color(theme::text_muted())
                            .child(o.date.clone()),
                    )
                    .child(priority_tag(o.priority))
                    .when(pinned, |d| {
                        d.child(icon(IconName::Pin, 13., theme::accent()))
                    })
                    .child(actions),
            )
            .child(content)
    }
}
