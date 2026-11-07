//! 依設計稿樣式手刻的小元件。按鈕回傳 `Stateful<Div>`，由呼叫端掛 `on_click`。

use gpui_kit::assets::IconName;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Div, ElementId, FontWeight, Hsla, InteractiveElement as _, ParentElement as _, SharedString,
    Stateful, StatefulInteractiveElement as _, Styled as _, Svg, div, px, svg,
};

use crate::model::AgentState;
use crate::theme;

pub fn icon(name: IconName, size: f32, color: Hsla) -> Svg {
    svg()
        .path(name.path())
        .flex_none()
        .size(px(size))
        .text_color(color)
}

pub fn pill(text: impl Into<SharedString>, fg: Hsla, bg: Hsla) -> Div {
    div()
        .flex_none()
        .px(px(10.))
        .py(px(3.))
        .rounded_full()
        .bg(bg)
        .text_color(fg)
        .text_size(px(13.))
        .font_weight(FontWeight::SEMIBOLD)
        .child(text.into())
}

/// 小方角標籤，例如 GOAL、決定、核准。
pub fn tag(text: impl Into<SharedString>, fg: Hsla, bg: Hsla) -> Div {
    div()
        .flex_none()
        .px(px(6.))
        .py(px(1.))
        .rounded(px(4.))
        .bg(bg)
        .text_color(fg)
        .text_size(px(11.))
        .font_weight(FontWeight::BOLD)
        .child(text.into())
}

pub fn dot(color: Hsla, size: f32) -> Div {
    div().flex_none().size(px(size)).rounded_full().bg(color)
}

pub fn count_badge(n: usize) -> Div {
    div()
        .ml_auto()
        .px(px(7.))
        .py(px(1.))
        .rounded_full()
        .bg(theme::accent())
        .text_color(theme::white())
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .child(n.to_string())
}

pub fn avatar(label: impl Into<SharedString>, bg: Hsla, size: f32) -> Div {
    div()
        .flex_none()
        .size(px(size))
        .rounded(px(size * 0.22))
        .bg(bg)
        .text_color(theme::white())
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(size * 0.39))
        .font_weight(FontWeight::BOLD)
        .child(label.into())
}

pub fn agent_state_colors(state: AgentState) -> (Hsla, Hsla) {
    match state {
        AgentState::Idle => (theme::text_muted(), theme::track()),
        AgentState::Thinking | AgentState::Planning => (theme::accent_text(), theme::accent_soft()),
        AgentState::WaitingCi => (theme::warn(), theme::warn_soft()),
        AgentState::WaitingApproval => (theme::warn_text(), theme::warn_soft()),
    }
}

pub fn agent_state_pill(state: AgentState) -> Div {
    let (fg, bg) = agent_state_colors(state);
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(6.))
        .px(px(10.))
        .py(px(3.))
        .rounded_full()
        .bg(bg)
        .text_color(fg)
        .text_size(px(13.))
        .font_weight(FontWeight::SEMIBOLD)
        .child(dot(fg, 7.))
        .child(state.label())
}

fn base_button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id.into())
        .flex()
        .flex_none()
        .items_center()
        .gap(px(6.))
        .cursor_pointer()
        .text_size(px(14.))
        .font_weight(FontWeight::SEMIBOLD)
        .child(label.into())
}

pub fn primary_button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    base_button(id, label)
        .px(px(18.))
        .py(px(7.))
        .rounded(px(8.))
        .bg(theme::accent())
        .text_color(theme::white())
        .hover(|s| s.opacity(0.9))
}

pub fn secondary_button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    base_button(id, label)
        .px(px(18.))
        .py(px(7.))
        .rounded(px(8.))
        .border_1()
        .border_color(theme::border_strong())
        .bg(theme::white())
        .text_color(theme::text())
        .hover(|s| s.bg(theme::subtle_bg()))
}

pub fn link_button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    base_button(id, label)
        .px(px(10.))
        .py(px(7.))
        .text_color(theme::accent())
        .hover(|s| s.opacity(0.8))
}

/// 頂端分頁列的單一分頁；底線標示目前選取。
pub fn tab(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
) -> Stateful<Div> {
    div()
        .id(id.into())
        .cursor_pointer()
        .pt(px(6.))
        .pb(px(10.))
        .border_b_2()
        .text_size(px(14.))
        .when_else(
            selected,
            |d| {
                d.border_color(theme::text())
                    .text_color(theme::text())
                    .font_weight(FontWeight::SEMIBOLD)
            },
            |d| {
                d.border_color(gpui_kit::transparent_black())
                    .text_color(theme::text_muted())
                    .hover(|s| s.text_color(theme::text()))
            },
        )
        .child(label.into())
}

pub fn section_label(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::text_muted())
        .px(px(2.))
        .pt(px(12.))
        .pb(px(4.))
        .child(text.into())
}

pub fn send_button(id: impl Into<ElementId>, size: f32) -> Stateful<Div> {
    div()
        .id(id.into())
        .flex_none()
        .size(px(size))
        .rounded(px(size * 0.22))
        .bg(theme::accent())
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .hover(|s| s.opacity(0.9))
        .child(icon(IconName::SendHorizontal, size * 0.47, theme::white()))
}

pub fn icon_button(id: impl Into<ElementId>, name: IconName) -> Stateful<Div> {
    div()
        .id(id.into())
        .flex_none()
        .size(px(32.))
        .rounded(px(6.))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .hover(|s| s.bg(theme::track()))
        .child(icon(name, 18., theme::text_muted()))
}

/// 滑過時顯示說明文字。
pub trait Tip: Sized {
    fn tip(self, text: impl Into<SharedString>) -> Self;
}

impl Tip for Stateful<Div> {
    fn tip(self, text: impl Into<SharedString>) -> Self {
        let text = text.into();
        self.tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx))
    }
}
