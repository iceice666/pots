//! 最左側的實例直列與左欄（目前實例的 project 列表／全域 Inbox 篩選）。

use gpui_kit::assets::IconName;
use gpui_kit::component::input::Input;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, Div, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, div, px,
};

use super::widgets::{Tip as _, count_badge, icon};
use crate::app::{AutomationSection, PotsApp, Screen, SettingsTab};
use crate::model::*;
use crate::theme;

fn rail_badge(n: usize) -> Div {
    div()
        .absolute()
        .right(px(-6.))
        .bottom(px(-4.))
        .min_w(px(18.))
        .h(px(18.))
        .px(px(4.))
        .rounded_full()
        .border_2()
        .border_color(theme::rail_bg())
        .bg(theme::danger())
        .text_color(theme::white())
        .text_size(px(10.))
        .font_weight(FontWeight::BOLD)
        .flex()
        .items_center()
        .justify_center()
        .child(n.to_string())
}

fn rail_item(id: SharedString, active: bool) -> Stateful<Div> {
    div()
        .id(id)
        .relative()
        .size(px(40.))
        .rounded(px(12.))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .when_else(
            active,
            |d| d.bg(theme::accent()),
            |d| {
                d.bg(theme::rail_item())
                    .hover(|s| s.bg(theme::rail_item_hover()))
            },
        )
}

/// 左側直條：白色短條標示目前選取。
fn rail_marker(active: bool) -> Div {
    div()
        .absolute()
        .left(px(-12.))
        .top(px(8.))
        .w(px(4.))
        .h(px(24.))
        .rounded_r(px(4.))
        .when(active, |d| d.bg(theme::white()))
}

fn sidebar_row(id: SharedString, active: bool) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(10.))
        .px(px(10.))
        .py(px(7.))
        .rounded(px(6.))
        .cursor_pointer()
        .text_size(px(14.))
        .when_else(
            active,
            |d| {
                d.bg(theme::sidebar_active())
                    .text_color(theme::white())
                    .font_weight(FontWeight::SEMIBOLD)
            },
            |d| {
                d.text_color(theme::text_on_dark())
                    .hover(|s| s.bg(theme::sidebar_hover()))
            },
        )
}

fn sidebar_heading(text: &'static str) -> Div {
    div()
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::text_dim_on_dark())
        .px(px(10.))
        .pt(px(18.))
        .pb(px(6.))
        .child(text)
}

fn dim(text: impl Into<SharedString>) -> Div {
    div()
        .ml_auto()
        .text_size(px(12.))
        .text_color(theme::text_dim_on_dark())
        .child(text.into())
}

fn sidebar_frame() -> Div {
    div()
        .flex_none()
        .w(px(220.))
        .h_full()
        .bg(theme::sidebar_bg())
        .text_color(theme::text_on_dark())
        .flex()
        .flex_col()
        .gap(px(2.))
        .px(px(12.))
        .py(px(16.))
}

/// 左欄最下方：設定入口，旁邊是目前實例的連線狀態等小字。
fn settings_footer(active: bool, subtitle: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id("nav-settings")
        .mt_auto()
        .flex()
        .items_center()
        .gap(px(10.))
        .px(px(10.))
        .py(px(8.))
        .rounded(px(6.))
        .cursor_pointer()
        .when(active, |d| d.bg(theme::sidebar_active()))
        .hover(|s| s.bg(theme::sidebar_hover()))
        .child(icon(IconName::Settings, 18., theme::text_on_dark()))
        .child(div().text_color(theme::white()).child("設定"))
        .child(dim(subtitle.into()))
}

impl PotsApp {
    pub fn render_rail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let global_inbox = self.screen == Screen::Inbox(None);
        let total = self.store.badge_count(None);

        let mut rail = div()
            .flex_none()
            .w(px(64.))
            .h_full()
            .bg(theme::rail_bg())
            .flex()
            .flex_col()
            .items_center()
            .gap(px(8.))
            .py(px(16.))
            .child(
                rail_item("rail-inbox".into(), global_inbox)
                    .tip("全部實例的 Inbox")
                    .child(rail_marker(global_inbox))
                    .child(icon(IconName::Inbox, 20., theme::white()))
                    .when(total > 0, |d| d.child(rail_badge(total)))
                    .on_click(cx.listener(|this, _, _, cx| this.open_inbox(None, cx))),
            )
            .child(
                div()
                    .w(px(28.))
                    .h(px(2.))
                    .rounded(px(1.))
                    .bg(theme::rail_item())
                    .my(px(2.)),
            );

        for inst in &self.store.instances {
            let id = inst.id;
            let active = !global_inbox && self.instance == id;
            let pending = self.store.badge_count(Some(id));
            let glyph = match inst.kind {
                InstanceKind::Desktop => IconName::Laptop,
                InstanceKind::MultiDevice => IconName::Server,
            };
            let offline = inst.connection != ConnectionState::Connected;
            rail =
                rail.child(
                    rail_item(format!("rail-inst-{}", id.0).into(), active)
                        .tip(format!("{}（{}）", inst.name, inst.kind.label()))
                        .child(rail_marker(active))
                        .child(icon(glyph, 20., theme::white()))
                        .when(pending > 0, |d| d.child(rail_badge(pending)))
                        .when(offline, |d| {
                            d.child(div().absolute().right(px(-4.)).top(px(-4.)).child(icon(
                                IconName::CircleAlert,
                                14.,
                                theme::attention(),
                            )))
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.select_instance(id, window, cx)
                        })),
                );
        }

        rail.child(
            div()
                .id("rail-add")
                .size(px(40.))
                .rounded(px(12.))
                .border_1()
                .border_dashed()
                .border_color(gpui_kit::rgb(0x4a4d55))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .tip("加入實例")
                .child(icon(IconName::Plus, 18., theme::text_dim_on_dark()))
                .on_click(cx.listener(|this, _, _, cx| this.notify_unimplemented("加入實例", cx))),
        )
    }

    pub fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let inst = self.current_instance();
        let instance_id = inst.id;
        let pending = self.store.badge_count(Some(instance_id));
        let inbox_active = self.screen == Screen::Inbox(Some(instance_id));

        let mut nav = sidebar_frame()
            .child(
                div()
                    .id("instance-switch")
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(10.))
                    .pt(px(4.))
                    .pb(px(14.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::sidebar_hover()))
                    .tip("實例選單：裝置、移交 project、中斷連線")
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme::white())
                            .child(inst.name.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(4.))
                            .text_size(px(12.))
                            .text_color(theme::text_dim_on_dark())
                            .child(inst.kind.label())
                            .child(icon(IconName::ChevronDown, 12., theme::text_dim_on_dark())),
                    )
                    .on_click(
                        cx.listener(|this, _, _, cx| this.notify_unimplemented("實例選單", cx)),
                    ),
            )
            .child(
                sidebar_row("nav-overseer".into(), self.screen == Screen::Overseer)
                    .child(icon(IconName::Sparkles, 16., theme::text_on_dark()))
                    .child("總管")
                    .child(dim(if cfg!(target_os = "macos") {
                        "⌘⇧O"
                    } else {
                        "Ctrl+Shift+O"
                    }))
                    .on_click(cx.listener(|this, _, window, cx| this.open_overseer(window, cx))),
            )
            .child(
                sidebar_row("nav-inbox".into(), inbox_active)
                    .child(icon(IconName::Inbox, 16., theme::text_on_dark()))
                    .child("Inbox")
                    .when(pending > 0, |d| d.child(count_badge(pending)))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.open_inbox(Some(instance_id), cx)),
                    ),
            )
            .child(sidebar_heading("專案"));

        for p in &inst.projects {
            let pid = p.id;
            let active = self.screen == Screen::Project(pid);
            let pending = self.store.pending_for_project(instance_id, pid);
            let trailing: Option<AnyElement> = if self.store.project_needs_you(instance_id, pid) {
                Some(
                    div()
                        .ml_auto()
                        .text_size(px(12.))
                        .text_color(theme::attention())
                        .child("需要你")
                        .into_any_element(),
                )
            } else if pending > 0 {
                Some(count_badge(pending).into_any_element())
            } else if p.unread > 0 {
                Some(
                    div()
                        .ml_auto()
                        .child(super::widgets::dot(theme::white(), 6.))
                        .into_any_element(),
                )
            } else {
                None
            };
            nav = nav.child(
                sidebar_row(format!("nav-project-{}", pid.0).into(), active)
                    .child(icon(IconName::Hash, 16., theme::text_on_dark()))
                    .child(p.name.clone())
                    .children(trailing)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_project(instance_id, pid, window, cx)
                    })),
            );
        }

        if self.live.is_some() {
            nav = match &self.new_project {
                Some((input, _)) => nav.child(
                    div()
                        .px(px(6.))
                        .py(px(2.))
                        .rounded(px(6.))
                        .bg(theme::white())
                        .text_color(theme::text())
                        .child(Input::new(input).appearance(false)),
                ),
                None => nav.child(
                    sidebar_row("nav-new-project".into(), false)
                        .child(icon(IconName::Plus, 16., theme::text_dim_on_dark()))
                        .child(
                            div()
                                .text_color(theme::text_dim_on_dark())
                                .child("新增專案"),
                        )
                        .on_click(
                            cx.listener(|this, _, window, cx| this.begin_new_project(window, cx)),
                        ),
                ),
            };
        }

        let on_automations =
            |section| self.screen == Screen::Automations && self.automation_section == section;
        nav.child(sidebar_heading("自動化"))
            .child(
                sidebar_row(
                    "nav-schedules".into(),
                    on_automations(AutomationSection::Schedules),
                )
                .child(icon(IconName::CalendarClock, 16., theme::text_on_dark()))
                .child("排程")
                .child(dim(self.automation_summary(AutomationSection::Schedules)))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.open_automations(AutomationSection::Schedules, cx)
                })),
            )
            .child(
                sidebar_row(
                    "nav-listeners".into(),
                    on_automations(AutomationSection::Listeners),
                )
                .child(icon(IconName::Radio, 16., theme::text_on_dark()))
                .child("Listener")
                .child(dim(self.automation_summary(AutomationSection::Listeners)))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.open_automations(AutomationSection::Listeners, cx)
                })),
            )
            .child(
                div()
                    .mt_auto()
                    .pt(px(8.))
                    .border_t_1()
                    .border_color(theme::sidebar_active())
                    .child(
                        settings_footer(self.settings.is_some(), inst.host.clone()).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.open_settings(SettingsTab::Providers, cx)
                            }),
                        ),
                    ),
            )
    }

    /// 全域 Inbox 的左欄：依實例篩選。
    pub fn render_inbox_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut nav = sidebar_frame()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(10.))
                    .pt(px(4.))
                    .pb(px(14.))
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme::white())
                            .child("Inbox"),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::text_dim_on_dark())
                            .child("所有實例"),
                    ),
            )
            .child(
                sidebar_row("inbox-all".into(), true)
                    .child(icon(IconName::Inbox, 16., theme::white()))
                    .child("全部實例")
                    .child(count_badge(self.store.badge_count(None))),
            )
            .child(sidebar_heading("實例"));

        for inst in &self.store.instances {
            let id = inst.id;
            let glyph = match inst.kind {
                InstanceKind::Desktop => IconName::Laptop,
                InstanceKind::MultiDevice => IconName::Server,
            };
            let n = self.store.badge_count(Some(id));
            nav = nav.child(
                sidebar_row(format!("inbox-inst-{}", id.0).into(), false)
                    .child(icon(glyph, 16., theme::text_on_dark()))
                    .child(inst.name.clone())
                    .when(n > 0, |d| d.child(dim(n.to_string())))
                    .on_click(cx.listener(move |this, _, _, cx| this.open_inbox(Some(id), cx))),
            );
        }

        let connected = self
            .store
            .instances
            .iter()
            .filter(|i| i.connection == ConnectionState::Connected)
            .count();
        nav.child(
            div()
                .mt_auto()
                .pt(px(8.))
                .border_t_1()
                .border_color(theme::sidebar_active())
                .child(
                    settings_footer(false, format!("{connected} 個實例已連線")).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.open_settings(SettingsTab::Providers, cx)
                        }),
                    ),
                ),
        )
    }
}
