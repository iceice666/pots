//! 自動化：排程與 listener。
//!
//! 左欄「排程」「Listener」進入實例層級的自動化頁，可依專案篩選；頻道的「自動化」分頁
//! 顯示同一份資料、只看這個 project（含套用到所有專案的 listener）。點一列在右欄看細節。

use gpui_kit::assets::IconName;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::popover::Popover;
use gpui_kit::component::switch::Switch;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, Div, FontWeight, Hsla, InteractiveElement as _, IntoElement, MouseButton,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, div, px,
    relative,
};

use super::channel::empty_state;
use super::panel::{panel_frame, white_box};
use super::text::{agent_text, clamp_markdown};
use super::widgets::*;
use crate::app::{AutomationSection, Panel, PotsApp};
use crate::model::*;
use crate::theme;

/// 表格欄寬；第一欄佔剩餘空間。
const COL_PROJECT: f32 = 116.;
const COL_CREATOR: f32 = 84.;
const COL_CADENCE: f32 = 88.;
const COL_NEXT: f32 = 120.;
const COL_SWITCH: f32 = 48.;
const COL_SOURCE: f32 = 140.;
const COL_COUNT: f32 = 64.;
const COL_STATE: f32 = 92.;

fn cell(w: f32) -> Div {
    div().flex_none().w(px(w)).pr(px(12.))
}

fn table_frame() -> Div {
    div()
        .flex()
        .flex_col()
        .rounded(px(10.))
        .border_1()
        .border_color(theme::border())
        .bg(theme::white())
        .overflow_hidden()
}

fn table_head() -> Div {
    div()
        .flex()
        .items_center()
        .px(px(16.))
        .py(px(10.))
        .bg(theme::subtle_bg())
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::text_muted())
}

fn section_heading(title: &'static str, note: String) -> Div {
    div()
        .flex()
        .items_baseline()
        .gap(px(10.))
        .pb(px(10.))
        .child(
            div()
                .text_size(px(16.))
                .font_weight(FontWeight::BOLD)
                .child(title),
        )
        .child(
            div()
                .text_size(px(13.))
                .text_color(theme::text_muted())
                .child(note),
        )
}

fn empty_row(text: &'static str) -> Div {
    div()
        .px(px(16.))
        .py(px(14.))
        .border_t_1()
        .border_color(theme::border())
        .text_size(px(13.))
        .text_color(theme::text_muted())
        .child(text)
}

fn trigger_color(status: TriggerStatus) -> Hsla {
    match status {
        TriggerStatus::Done => theme::ok(),
        TriggerStatus::Running => theme::accent(),
        TriggerStatus::Failed => theme::danger(),
        TriggerStatus::Notified => theme::text_muted(),
    }
}

fn kv(label: &'static str, value: impl IntoElement) -> Div {
    div()
        .flex()
        .gap(px(12.))
        .text_size(px(13.))
        .line_height(relative(1.5))
        .child(
            div()
                .flex_none()
                .w(px(68.))
                .text_color(theme::text_muted())
                .child(label),
        )
        .child(div().flex_1().min_w_0().child(value))
}

fn box_title(text: &'static str) -> Div {
    div()
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::text_muted())
        .child(text)
}

fn history(scope: &'static str, records: &[TriggerRecord], empty: &'static str) -> Div {
    let col = div().flex().flex_col().gap(px(8.));
    if records.is_empty() {
        return col.child(
            div()
                .text_size(px(13.))
                .text_color(theme::text_muted())
                .child(empty),
        );
    }
    col.children(records.iter().enumerate().map(|(i, r)| {
        white_box()
            .gap(px(4.))
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_weight(FontWeight::SEMIBOLD)
                            // 觸發紀錄的標題是事件摘要（「#118 → 支援 refresh token」），
                            // 由平台從事件內容截出，可能含行內 Markdown。
                            .child(agent_text(
                                SharedString::from(format!("trigger-{scope}-title-{i}")),
                                r.title.clone(),
                            )),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(12.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(trigger_color(r.status))
                            .child(r.status.label()),
                    ),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme::text_muted())
                    .child(r.meta.clone()),
            )
    }))
}

fn small(button: gpui_kit::Stateful<Div>) -> gpui_kit::Stateful<Div> {
    button.text_size(px(13.)).px(px(14.)).py(px(6.))
}

impl PotsApp {
    fn project_label(&self, project: Option<ProjectId>, global: &'static str) -> String {
        match project {
            Some(p) => self
                .current_instance()
                .project_name(p)
                .map_or("# ?".to_string(), |n| format!("# {n}")),
            None => global.to_string(),
        }
    }

    /// 實例層級的自動化頁（左欄「排程」「Listener」）。
    pub fn render_automations_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let inst = self.current_instance();
        let empty =
            inst.schedules.is_empty() && inst.listeners.is_empty() && inst.sources.is_empty();

        let header = div()
            .flex()
            .items_center()
            .gap(px(12.))
            .px(px(28.))
            .py(px(18.))
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .text_size(px(20.))
                            .font_weight(FontWeight::BOLD)
                            .child("自動化"),
                    )
                    .child(
                        div()
                            .text_size(px(14.))
                            .text_color(theme::text_muted())
                            .child("agent 建立的排程，和讓事件自動開工的 listener。每次觸發都計入預算。"),
                    ),
            )
            .when(!empty, |d| d.child(self.render_project_filter(cx)))
            .when(inst.automation_pause.is_none() && !empty, |d| {
                d.child(
                    secondary_button("pause-all", "全部暫停")
                        .on_click(cx.listener(|this, _, _, cx| this.pause_all_automations(cx))),
                )
            })
            .child(
                primary_button("new-listener", "新增 listener").on_click(
                    cx.listener(|this, _, window, cx| this.new_listener(window, cx)),
                ),
            );

        let body: AnyElement = if empty {
            empty_state(
                format!("{}還沒有自動化", inst.name),
                "agent 在對話裡建立排程，或提案 listener 等你核准後，會出現在這裡。",
            )
            .into_any_element()
        } else {
            div()
                .id("automations-body")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .track_scroll(&self.automations_scroll)
                .px(px(28.))
                .py(px(20.))
                .flex()
                .flex_col()
                .gap(px(24.))
                .children(self.render_automation_sections(self.automation_filter, true, cx))
                .into_any_element()
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
    }

    /// 右欄「自動化」：這個 project 的監看設定、排程與 listener。
    pub fn render_project_automations(
        &self,
        p: &ProjectView,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let watch = self.render_project_watch(p, cx);
        let body: Vec<AnyElement> = if self.current_instance().automation_count(p.id) == 0 {
            vec![
                empty_state(
                    "這個 project 沒有自動化",
                    "請 agent 定時做某件事，或讓事件自動開工，例如「issue 加上 agent label 就開 Goal」。",
                )
                .into_any_element(),
            ]
        } else {
            self.render_automation_sections(Some(p.id), false, cx)
        };
        div()
            .id("project-automations")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(16.))
            .py(px(16.))
            .flex()
            .flex_col()
            .gap(px(24.))
            .child(watch)
            .children(body)
            .into_any_element()
    }

    /// 暫停橫幅（若有）、排程、listener 三塊，自動化頁的捲動定位依賴這個順序。
    fn render_automation_sections(
        &self,
        project: Option<ProjectId>,
        show_project: bool,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut out = Vec::new();
        if let Some(pause) = &self.current_instance().automation_pause {
            out.push(
                div()
                    .flex()
                    .items_center()
                    // 右欄很窄：說明換到下一行。
                    .flex_wrap()
                    .gap(px(10.))
                    .px(px(18.))
                    .py(px(12.))
                    .rounded(px(10.))
                    .bg(theme::warn_soft())
                    .text_color(theme::warn_text())
                    .text_size(px(14.))
                    .child(icon(IconName::Pause, 15., theme::warn()))
                    .child(
                        div()
                            .flex_none()
                            .font_weight(FontWeight::BOLD)
                            .child(format!("全部暫停到{}", pause.until)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(200.))
                            .child(format!("{}，錯過的觸發不會補跑。", pause.by)),
                    )
                    .child(
                        secondary_button("resume-automations", "立即恢復")
                            .text_size(px(13.))
                            .px(px(14.))
                            .py(px(5.))
                            .border_color(theme::warn_border())
                            .text_color(theme::warn_text())
                            .on_click(cx.listener(|this, _, _, cx| this.resume_automations(cx))),
                    )
                    .into_any_element(),
            );
        }
        out.push(
            self.render_schedule_table(project, show_project, cx)
                .into_any_element(),
        );
        out.push(
            self.render_listener_table(project, show_project, cx)
                .into_any_element(),
        );
        // 實例層級的頁面才列外部來源。
        if show_project && let Some(sources) = self.render_sources(cx) {
            out.push(sources.into_any_element());
        }
        out
    }

    fn render_project_filter(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let inst = self.current_instance();
        let label = match self.automation_filter {
            Some(p) => inst.project_name(p).unwrap_or("?").to_string(),
            None => "所有專案".to_string(),
        };
        let mut options: Vec<(Option<ProjectId>, String)> = vec![(None, "所有專案".into())];
        options.extend(inst.projects.iter().map(|p| (Some(p.id), p.name.clone())));
        let selected = self.automation_filter;
        let app = cx.entity().downgrade();

        Popover::new("automation-filter")
            .anchor(gpui_kit::Anchor::TopRight)
            .trigger(
                Button::new("automation-filter-trigger")
                    .outline()
                    .label(label)
                    .dropdown_caret(true),
            )
            .content(move |_, _, cx| {
                let popover = cx.entity().downgrade();
                div()
                    .flex()
                    .flex_col()
                    .min_w(px(180.))
                    .children(options.iter().enumerate().map(|(i, (value, name))| {
                        let value = *value;
                        let app = app.clone();
                        let popover = popover.clone();
                        div()
                            .id(("automation-filter-option", i))
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .px(px(10.))
                            .py(px(6.))
                            .rounded(px(6.))
                            .text_size(px(14.))
                            .cursor_pointer()
                            .hover(|s| s.bg(theme::subtle_bg()))
                            .child(div().w(px(14.)).when(value == selected, |d| {
                                d.child(icon(IconName::Check, 14., theme::accent()))
                            }))
                            .child(name.clone())
                            .on_click(move |_, window, cx| {
                                app.update(cx, |this, cx| {
                                    this.automation_filter = value;
                                    this.panel = None;
                                    cx.notify();
                                })
                                .ok();
                                popover
                                    .update(cx, |state, cx| state.dismiss(window, cx))
                                    .ok();
                            })
                    }))
            })
    }

    fn render_schedule_table(
        &self,
        project: Option<ProjectId>,
        show_project: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let inst = self.current_instance();
        let paused = inst.automation_pause.is_some();
        let schedules: Vec<&ScheduleView> = inst.schedules_for(project).collect();
        let note = format!(
            "{} 個 · 每個專案最多 10 個，最短間隔 10 分鐘",
            schedules.len()
        );
        let app = cx.entity().downgrade();

        let head = table_head()
            .child(div().flex_1().min_w_0().child("要做什麼"))
            // 右欄（project 的自動化）太窄放不下欄位，頻率與下一次併到第一欄的第二行。
            .when(show_project, |d| {
                d.child(cell(COL_PROJECT).child("專案"))
                    .child(cell(COL_CREATOR).child("建立者"))
                    .child(cell(COL_CADENCE).child("頻率"))
                    .child(cell(COL_NEXT).child("下一次"))
            })
            .child(div().flex_none().w(px(COL_SWITCH)).child("啟用"));

        let rows = schedules.iter().map(|s| {
            let id = s.id;
            let selected = self.panel == Some(Panel::Schedule(id));
            let (next, next_color) = if !s.enabled {
                ("已停用".to_string(), theme::text_muted())
            } else if let (true, Some(after)) = (paused, &s.next_after_pause) {
                (format!("{after} 後恢復"), theme::warn())
            } else {
                (s.next.clone(), theme::text())
            };
            let app = app.clone();
            div()
                .id(SharedString::from(format!("schedule-{}", id.0)))
                .flex()
                .items_center()
                .px(px(16.))
                .py(px(12.))
                .border_t_1()
                .border_color(theme::border())
                .text_size(px(14.))
                .cursor_pointer()
                .when_else(
                    selected,
                    |d| d.bg(theme::accent_soft()),
                    |d| d.hover(|s| s.bg(theme::subtle_bg())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .pr(px(12.))
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                // 排程的 prompt 是使用者或 agent 寫的 Markdown，醒來時
                                // 原文交給 agent；表格只放得下一行，先截一段（render 後
                                // 再用 max_lines 夾住高度）。
                                .child(
                                    agent_text(
                                        SharedString::from(format!("schedule-prompt-{}", id.0)),
                                        clamp_markdown(&s.prompt, 1),
                                    )
                                    .max_lines(1),
                                ),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(theme::text_muted())
                                .child(s.note.clone()),
                        )
                        .when(!show_project, |d| {
                            d.child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(theme::text_muted())
                                    .child(format!("{} · {} · ", s.creator.label(), s.cadence))
                                    .child(
                                        div()
                                            .text_color(next_color)
                                            .when(next_color == theme::warn(), |d| {
                                                d.font_weight(FontWeight::SEMIBOLD)
                                            })
                                            .child(next.clone()),
                                    )
                                    .flex()
                                    .flex_wrap(),
                            )
                        }),
                )
                .when(show_project, |d| {
                    d.child(cell(COL_PROJECT).child(self.project_label(s.project, "總管")))
                        .child(
                            cell(COL_CREATOR)
                                .text_color(theme::text_muted())
                                .child(s.creator.label()),
                        )
                        .child(cell(COL_CADENCE).child(s.cadence.clone()))
                        .child(
                            cell(COL_NEXT)
                                .text_color(next_color)
                                .when(next_color == theme::warn(), |d| {
                                    d.font_weight(FontWeight::SEMIBOLD)
                                })
                                .child(next),
                        )
                })
                .child(
                    // 開關自己處理點擊，不要連帶選取整列。
                    div()
                        .flex_none()
                        .w(px(COL_SWITCH))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(
                            Switch::new(SharedString::from(format!("schedule-switch-{}", id.0)))
                                .checked(s.enabled)
                                .color(theme::accent())
                                .on_change(move |checked, _, cx| {
                                    app.update(cx, |this, cx| {
                                        this.set_schedule_enabled(id, *checked, cx)
                                    })
                                    .ok();
                                }),
                        ),
                )
                .on_click(
                    cx.listener(move |this, _, _, cx| {
                        this.open_panel(Some(Panel::Schedule(id)), cx)
                    }),
                )
        });

        div()
            .flex()
            .flex_col()
            .child(section_heading("排程", note))
            .child(
                table_frame()
                    .child(head)
                    .children(rows)
                    .when(schedules.is_empty(), |d| d.child(empty_row("沒有排程。"))),
            )
    }

    fn listener_state(&self, l: &ListenerView) -> AnyElement {
        let pause = self.current_instance().automation_pause.as_ref();
        let text = |t: String, color: Hsla| {
            div()
                .text_size(px(13.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(color)
                .child(t)
                .into_any_element()
        };
        match (l.state, pause) {
            (ListenerState::PendingApproval(_), _) => {
                pill("待你核准", theme::warn_text(), theme::warn_soft())
                    .text_size(px(12.))
                    .into_any_element()
            }
            (ListenerState::Disabled, _) => text("已停用".into(), theme::text_muted()),
            (ListenerState::Proposed, _) => {
                pill("待你啟用", theme::warn_text(), theme::warn_soft())
                    .text_size(px(12.))
                    .into_any_element()
            }
            (ListenerState::Throttled, _) => text("超過上限，已暫停".into(), theme::warn()),
            (ListenerState::Active, Some(p)) => {
                text(format!("暫停到{}", p.until_short), theme::warn())
            }
            (ListenerState::Active, None) => text("啟用".into(), theme::ok()),
        }
    }

    fn render_listener_table(
        &self,
        project: Option<ProjectId>,
        show_project: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let inst = self.current_instance();
        let listeners: Vec<&ListenerView> = inst.listeners_for(project).collect();
        let active = listeners
            .iter()
            .filter(|l| l.state == ListenerState::Active)
            .count();
        let pending = listeners
            .iter()
            .filter(|l| {
                matches!(
                    l.state,
                    ListenerState::PendingApproval(_) | ListenerState::Proposed
                )
            })
            .count();
        let mut note = format!("{active} 個啟用");
        if pending > 0 {
            note.push_str(&format!(" · {pending} 個待核准"));
        }

        let head = table_head()
            .child(div().flex_1().min_w_0().child("條件 → 動作"))
            // 右欄太窄：來源、觸發次數與上限併到第一欄的第二行。
            .when(show_project, |d| {
                d.child(cell(COL_PROJECT).child("專案"))
                    .child(cell(COL_SOURCE).child("來源 · 信任"))
                    .child(cell(COL_COUNT).child("本週觸發"))
                    .child(cell(COL_COUNT).child("每次上限"))
            })
            .child(div().flex_none().w(px(COL_STATE)).child("狀態"));

        let rows = listeners.iter().map(|l| {
            let id = l.id;
            let selected = self.panel == Some(Panel::Listener(id));
            div()
                .id(SharedString::from(format!("listener-{}", id.0)))
                .flex()
                .items_center()
                .px(px(16.))
                .py(px(12.))
                .border_t_1()
                .border_color(theme::border())
                .text_size(px(14.))
                .cursor_pointer()
                .when_else(
                    selected,
                    |d| d.bg(theme::accent_soft()),
                    |d| d.hover(|s| s.bg(theme::subtle_bg())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .pr(px(12.))
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(div().font_weight(FontWeight::SEMIBOLD).child(l.title()))
                        // 頻道分頁沒有專案欄，套用到所有專案的另外標示。
                        .when(!show_project && l.project.is_none(), |d| {
                            d.child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(theme::text_muted())
                                    .child("套用到所有專案"),
                            )
                        })
                        .when(!show_project, |d| {
                            let mut meta = format!("{} · {}", l.source, l.trust.label());
                            if l.triggers_this_week > 0 {
                                meta.push_str(&format!(" · 本週 {} 次", l.triggers_this_week));
                            }
                            if let Some(cents) = l.budget_per_trigger_cents {
                                meta.push_str(&format!(" · 每次上限 {}", format_dollars(cents)));
                            }
                            d.child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(theme::text_muted())
                                    .child(meta),
                            )
                        }),
                )
                .when(show_project, |d| {
                    d.child(cell(COL_PROJECT).child(self.project_label(l.project, "所有專案")))
                        .child(cell(COL_SOURCE).child(format!(
                            "{} · {}",
                            l.source,
                            l.trust.label()
                        )))
                        .child(cell(COL_COUNT).child(if l.triggers_this_week == 0 {
                            "—".to_string()
                        } else {
                            format!("{} 次", l.triggers_this_week)
                        }))
                        .child(
                            cell(COL_COUNT).child(
                                l.budget_per_trigger_cents
                                    .map_or("—".to_string(), format_dollars),
                            ),
                        )
                })
                .child(
                    div()
                        .flex_none()
                        .w(px(COL_STATE))
                        .child(self.listener_state(l)),
                )
                .on_click(
                    cx.listener(move |this, _, _, cx| {
                        this.open_panel(Some(Panel::Listener(id)), cx)
                    }),
                )
        });

        div()
            .flex()
            .flex_col()
            .child(section_heading("Listener", note))
            .child(
                table_frame()
                    .child(head)
                    .children(rows)
                    .when(listeners.is_empty(), |d| {
                        d.child(empty_row("沒有 listener。"))
                    }),
            )
    }

    fn detail_header(
        &self,
        eyebrow: String,
        title: String,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .flex()
            .items_start()
            .gap(px(8.))
            .px(px(20.))
            .pt(px(16.))
            .pb(px(12.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::text_muted())
                            .child(eyebrow),
                    )
                    .child(
                        div()
                            .text_size(px(17.))
                            .font_weight(FontWeight::BOLD)
                            .line_height(relative(1.35))
                            .child(title),
                    ),
            )
            .child(self.close_button(cx))
    }

    fn detail_footer() -> Div {
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(20.))
            .py(px(16.))
            .border_t_1()
            .border_color(theme::border())
    }

    pub fn render_listener_panel(
        &self,
        l: &ListenerView,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = l.id;
        let mono = cx.theme().mono_font_family.clone();
        let eyebrow = format!("LISTENER · {}", self.project_label(l.project, "所有專案"));

        let conditions = white_box()
            .gap(px(6.))
            .child(box_title("條件"))
            .child(kv("來源", format!("{} · {}", l.source, l.source_detail)))
            .child(kv(
                "資源",
                div()
                    .font_family(mono)
                    .text_size(px(12.))
                    .child(l.resource.clone()),
            ))
            .child(kv("事件", l.event.clone()))
            .child(kv("比對", l.matcher.clone()))
            .child(kv("信任", l.trust.label()));

        let action = white_box()
            .gap(px(6.))
            .child(box_title("動作"))
            // action_detail 是伺服器依動作種類產生的固定說明（「開一個 Goal，Brief 用下面
            // 的模板加上事件內容」……），是平台的字，維持純文字。
            .child(
                div()
                    .text_size(px(13.))
                    .line_height(relative(1.5))
                    .child(l.action_detail.clone()),
            )
            .when_some(l.template.clone(), |d, t| {
                d.child(
                    div()
                        .px(px(12.))
                        .py(px(10.))
                        .rounded(px(6.))
                        .bg(theme::subtle_bg())
                        .text_size(px(13.))
                        .line_height(relative(1.6))
                        // 刻意維持純文字：這是使用者在表單裡填的模板原文（會原樣接上事件
                        // 內容變成 Goal brief），可能含 `{{…}}`、縮排與其他非常規語法；
                        // 顯示「你填的原樣」比渲染重要，依「寧可少改不可丟字」保守處理。
                        .child(t),
                )
            })
            .child(kv(
                "每次上限",
                l.budget_per_trigger_cents
                    .map_or("不花費".to_string(), format_dollars),
            ))
            .child(kv(
                "頻率上限",
                l.max_per_hour
                    .map_or("不限".to_string(), |n| format!("每小時最多 {n} 次")),
            ));

        let pending = match l.state {
            ListenerState::PendingApproval(a) => Some(a),
            _ => None,
        };
        let status_note = match (l.state, &self.current_instance().automation_pause) {
            (ListenerState::PendingApproval(_), _) => {
                Some("agent 提案的 listener，你核准後才會開始監看。".to_string())
            }
            (ListenerState::Active, Some(p)) => {
                Some(format!("全部暫停到{}，期間的事件不會開工。", p.until))
            }
            (ListenerState::Disabled, _) => Some("已停用，事件進來不會觸發。".to_string()),
            (ListenerState::Proposed, _) => Some(
                "agent 提案的 listener，你啟用後才會開始監看（核准流程在之後的版本）。".to_string(),
            ),
            (ListenerState::Throttled, _) => {
                Some("一小時內觸發超過上限，先暫停了。確認沒問題後重新啟用。".to_string())
            }
            _ => None,
        };

        let footer =
            match pending {
                Some(approval) => {
                    Self::detail_footer()
                        .child(small(primary_button("listener-approve", "核准")).on_click(
                            cx.listener(move |this, _, window, cx| {
                                this.act(approval, InboxAction::Approve, window, cx)
                            }),
                        ))
                        .child(small(secondary_button("listener-reject", "拒絕")).on_click(
                            cx.listener(move |this, _, window, cx| {
                                this.act(approval, InboxAction::Reject, window, cx)
                            }),
                        ))
                        .child(
                            link_button("listener-edit", "修改條件")
                                .text_size(px(13.))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.notify_unimplemented("修改條件", cx)
                                })),
                        )
                }
                None => {
                    let enabled = l.state == ListenerState::Active;
                    let proposed = l.state == ListenerState::Proposed;
                    Self::detail_footer()
                        .when(!proposed, |d| {
                            d.child(
                                small(secondary_button("listener-edit", "編輯條件")).on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.notify_unimplemented("編輯條件", cx)
                                    }),
                                ),
                            )
                        })
                        .child(
                            small(if proposed {
                                primary_button("listener-toggle", "啟用")
                            } else {
                                secondary_button(
                                    "listener-toggle",
                                    if enabled { "停用" } else { "啟用" },
                                )
                            })
                            .on_click(cx.listener(
                                move |this, _, _, cx| this.set_listener_enabled(id, !enabled, cx),
                            )),
                        )
                        .child(
                            link_button("listener-delete", "刪除")
                                .ml_auto()
                                .text_size(px(13.))
                                .text_color(theme::danger())
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.delete_listener(id, cx)),
                                ),
                        )
                }
            };

        panel_frame()
            .child(self.detail_header(eyebrow, l.title(), cx))
            .child(
                div()
                    .id("listener-panel-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(20.))
                    .pb(px(16.))
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .when_some(status_note, |d, n| {
                        d.child(
                            div()
                                .px(px(12.))
                                .py(px(8.))
                                .rounded(px(8.))
                                .bg(theme::warn_soft())
                                .text_color(theme::warn_text())
                                .text_size(px(13.))
                                .child(n),
                        )
                    })
                    .child(conditions)
                    .child(action)
                    .child(section_label("觸發紀錄").pt(px(8.)).pb(px(0.)))
                    .child(history(
                        "listener",
                        &l.history,
                        if pending.is_some() {
                            "還沒有觸發紀錄。核准後才會開始監看。"
                        } else {
                            "還沒有觸發紀錄。"
                        },
                    )),
            )
            .child(footer)
    }

    pub fn render_schedule_panel(
        &self,
        s: &ScheduleView,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = s.id;
        let enabled = s.enabled;
        let pause = self.current_instance().automation_pause.as_ref();
        let next = match (pause, &s.next_after_pause) {
            (Some(_), Some(after)) => format!("{after}（暫停中，原本是{}）", s.next),
            _ => s.next.clone(),
        };

        let settings = white_box()
            .gap(px(6.))
            .child(box_title("設定"))
            .child(kv("頻率", s.cadence.clone()))
            .child(kv("下一次", if enabled { next } else { "已停用".into() }))
            .child(kv("建立者", s.creator.label()))
            // 單次排程的小字就是「單次」，不另列期限。
            .when(s.note != s.cadence, |d| d.child(kv("期限", s.note.clone())));

        let prompt =
            white_box()
                .gap(px(6.))
                .child(box_title("在哪裡醒來"))
                .child(div().text_size(px(13.)).line_height(relative(1.5)).child(
                    match s.creator {
                        ScheduleCreator::Planner => "在 Goal 下醒來，帶著 Brief 與 Ledger。",
                        ScheduleCreator::Overseer => "在總管對話裡醒來。",
                        ScheduleCreator::Chat => "在 project 對話裡醒來，帶著對話記憶。",
                    },
                ));

        panel_frame()
            .child(self.detail_header(
                format!("排程 · {}", self.project_label(s.project, "總管")),
                s.prompt.clone(),
                cx,
            ))
            .child(
                div()
                    .id("schedule-panel-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(20.))
                    .pb(px(16.))
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .child(settings)
                    .child(prompt)
                    .child(section_label("觸發紀錄").pt(px(8.)).pb(px(0.)))
                    .child(history("schedule", &s.history, "還沒有觸發過。")),
            )
            .child(
                Self::detail_footer()
                    .child(small(secondary_button("schedule-edit", "改時間")).on_click(
                        cx.listener(|this, _, _, cx| this.notify_unimplemented("改時間", cx)),
                    ))
                    .child(
                        small(secondary_button(
                            "schedule-toggle",
                            if enabled { "停用" } else { "啟用" },
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_schedule_enabled(id, !enabled, cx)
                        })),
                    )
                    .child(
                        link_button("schedule-delete", "刪除")
                            .ml_auto()
                            .text_size(px(13.))
                            .text_color(theme::danger())
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.delete_schedule(id, cx)),
                            ),
                    ),
            )
    }

    /// 左欄自動化區的狀態字：暫停時顯示「已暫停」，否則是啟用中的數量。
    pub fn automation_summary(&self, section: AutomationSection) -> String {
        let inst = self.current_instance();
        if inst.automation_pause.is_some()
            && match section {
                AutomationSection::Schedules => !inst.schedules.is_empty(),
                AutomationSection::Listeners => !inst.listeners.is_empty(),
            }
        {
            return "已暫停".into();
        }
        let n = match section {
            AutomationSection::Schedules => inst.schedules.iter().filter(|s| s.enabled).count(),
            AutomationSection::Listeners => inst
                .listeners
                .iter()
                .filter(|l| l.state == ListenerState::Active)
                .count(),
        };
        n.to_string()
    }
}
