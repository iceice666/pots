//! 右欄的成員列表：頻道標題列的「成員」按鈕切換，和 Goal 面板共用右欄。
//!
//! 成員與分組由 [`crate::members::members`] 從 project、provider 與 worker 設定推導。點 worker
//! 開任務 thread、點 Planner 開那個 Goal 的面板（右欄換成那個內容）、點對話 agent 開設定的
//! 「角色」類別。

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, Div, FontWeight, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, div, px,
    relative,
};

use super::panel::panel_frame;
use super::widgets::*;
use crate::app::{GoalTab, Panel, PotsApp, SettingsTab};
use crate::members::{Member, MemberGroup, MemberKind, MemberTarget, members};
use crate::model::*;
use crate::theme;

const AVATAR: f32 = 32.;

fn avatar_color(kind: MemberKind) -> Hsla {
    match kind {
        MemberKind::Human => theme::accent(),
        MemberKind::Agent => theme::text(),
        MemberKind::Planner => theme::accent_text(),
        MemberKind::Worker => theme::violet(),
    }
}

/// 頭像右下角的狀態點：工作中藍、等待中橘、待命灰；你是綠色（在線）。
fn status_color(m: &Member) -> Hsla {
    match (m.kind, m.group) {
        (MemberKind::Human, _) => theme::ok(),
        (_, MemberGroup::Working) => theme::accent(),
        (_, MemberGroup::Waiting) => theme::warn_strong(),
        (_, MemberGroup::Standby) => theme::input_border(),
    }
}

/// 頭像加狀態點；狀態點外圈是右欄的底色，像 Discord 一樣「咬」進頭像。
fn member_avatar(m: &Member) -> Div {
    div()
        .relative()
        .flex_none()
        .size(px(AVATAR))
        .child(avatar(m.initial(), avatar_color(m.kind), AVATAR))
        .child(
            div()
                .absolute()
                .right(px(-3.))
                .bottom(px(-3.))
                .size(px(14.))
                .rounded_full()
                .bg(theme::panel_bg())
                .flex()
                .items_center()
                .justify_center()
                .child(dot(status_color(m), 8.)),
        )
}

impl PotsApp {
    pub(super) fn render_members_panel(
        &self,
        project: &ProjectView,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let inst = self.current_instance();
        let sections = members(project, &inst.providers, &inst.worker);
        let total: usize = sections.iter().map(|s| s.members.len()).sum();

        let header = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .px(px(20.))
            .pt(px(14.))
            .pb(px(14.))
            .border_b_1()
            .border_color(theme::border())
            .child(div().font_weight(FontWeight::BOLD).child("成員"))
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(theme::text_muted())
                    .child(total.to_string()),
            )
            .child(self.close_button(cx));

        let mut body = div().flex().flex_col();
        let mut index = 0;
        for (i, section) in sections.iter().enumerate() {
            body = body.child(
                section_label(section.title())
                    .when(i == 0, |d| d.pt(px(0.)))
                    .pb(px(6.)),
            );
            for m in &section.members {
                body = body.child(self.render_member(index, m, cx));
                index += 1;
            }
        }

        panel_frame().child(header).child(
            div()
                .id("members-panel-body")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .px(px(12.))
                .py(px(14.))
                .child(body),
        )
    }

    fn render_member(&self, index: usize, m: &Member, cx: &mut Context<Self>) -> AnyElement {
        let status_text = match m.group {
            MemberGroup::Waiting => theme::warn_text(),
            MemberGroup::Working => theme::text(),
            MemberGroup::Standby => theme::text_muted(),
        };
        let name = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .min_w_0()
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .line_height(px(20.))
                    .child(m.name.clone()),
            )
            .children(m.kind.tag().map(role_chip));
        let text = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(1.))
            .child(name)
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(status_text)
                    .line_height(relative(1.4))
                    .line_clamp(2)
                    .child(m.status.clone()),
            )
            .children(m.model.clone().map(|model| {
                div()
                    .text_size(px(12.))
                    .text_color(theme::text_muted())
                    .truncate()
                    .child(model)
            }));

        let row = div()
            .id(SharedString::from(format!("member-{index}")))
            .flex()
            .items_start()
            .gap(px(10.))
            .px(px(8.))
            .py(px(7.))
            .rounded(px(8.))
            .child(div().pt(px(2.)).child(member_avatar(m)))
            .child(text);
        match m.target {
            Some(target) => row
                .cursor_pointer()
                .hover(|s| s.bg(theme::track()))
                .on_click(cx.listener(move |this, _, _, cx| this.open_member(target, cx)))
                .into_any_element(),
            None => row.into_any_element(),
        }
    }

    /// 點了成員：worker 開任務 thread、Planner 開 Goal 面板（都換掉右欄的成員列表）、
    /// 對話 agent 開設定的「角色」類別。
    pub fn open_member(&mut self, target: MemberTarget, cx: &mut Context<Self>) {
        match target {
            MemberTarget::Task(task) => self.open_panel(Some(Panel::Task(task)), cx),
            MemberTarget::Goal(goal) => {
                self.open_panel(Some(Panel::Goal(goal, GoalTab::Tasks)), cx)
            }
            MemberTarget::Roles => self.open_settings(SettingsTab::Roles, cx),
        }
    }
}
