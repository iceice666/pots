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

use super::automation_forms::chip;
use super::channel::empty_state;
use super::panel::{panel_frame, white_box};
use super::text::agent_text;
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

/// 帶右側操作元件的區塊標題列；標題與小字維持原本的基線對齊。
///
/// 右欄 project 面板很窄，小字加上操作元件放不下時讓元件換到下一行。
fn section_heading_with_action(title: &'static str, note: String, action: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(px(10.))
        .pb(px(10.))
        .child(section_heading(title, note).pb(px(0.)).flex_1().min_w_0())
        .child(div().flex_none().child(action))
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

/// 排程表列出的順序：還會觸發的在前，其次是停用的，已結束的沉到最後。
/// `sort_by_key` 是穩定排序，同組內維持 server 的 `created_at` 順序。
fn sort_schedules(schedules: &mut [&ScheduleView]) {
    schedules.sort_by_key(|s| (s.finished, !s.enabled));
}

/// 排程表表頭的小字。已結束的排程不算進「N 個」，另外標出有幾筆已結束。
fn schedules_note(schedules: &[&ScheduleView]) -> String {
    let finished = schedules.iter().filter(|s| s.finished).count();
    let note = format!(
        "{} 個 · 每個專案最多 10 個，最短間隔 10 分鐘",
        schedules.len() - finished
    );
    if finished == 0 {
        note
    } else {
        format!("{note}（另有 {finished} 個已結束）")
    }
}

/// 排程表實際要渲染的列，以及被「隱藏已結束」開關藏起來的數量。
///
/// 開關關著時原樣回傳、`hidden` 為 0（與改動前的行為一致）。
fn filter_finished(schedules: Vec<&ScheduleView>, hide: bool) -> (Vec<&ScheduleView>, usize) {
    if !hide {
        return (schedules, 0);
    }
    let hidden = schedules.iter().filter(|s| s.finished).count();
    let kept = schedules.into_iter().filter(|s| !s.finished).collect();
    (kept, hidden)
}

/// 排程表標題列的小字；開關開啟且真的藏了排程時，把 [`schedules_note`] 尾巴的
/// 「另有 N 個已結束」換成「已隱藏 N 個已結束」。
///
/// 這是畫面上唯一說明「表格少掉了東西」的地方，藏起來卻不說，使用者會以為排程消失了。
fn hide_finished_note(note: String, hidden: usize) -> String {
    if hidden == 0 {
        return note;
    }
    let suffix = format!("（另有 {hidden} 個已結束）");
    match note.strip_suffix(suffix.as_str()) {
        Some(base) => format!("{base}（已隱藏 {hidden} 個已結束）"),
        // `schedules_note` 一定會帶這個尾巴（`hidden` 就是已結束的數量），這裡只是保險。
        None => format!("{note}（已隱藏 {hidden} 個已結束）"),
    }
}

/// 排程表「下一次」欄的文字與顏色。
///
/// 已結束優先於已停用：單次排程觸發完後 server 只標 `finished`，`enabled` 仍是 true，
/// 只看 `enabled` 會把已結束的顯示成還會觸發的時間。
fn schedule_row_next(s: &ScheduleView, paused: bool) -> (String, Hsla) {
    if s.finished {
        ("已結束".to_string(), theme::text_muted())
    } else if !s.enabled {
        ("已停用".to_string(), theme::text_muted())
    } else if let (true, Some(after)) = (paused, &s.next_after_pause) {
        (format!("{after} 後恢復"), theme::warn())
    } else {
        (s.next.clone(), theme::text())
    }
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
                            // 觸發紀錄的標題是事件摘要（「#118 → 支援 refresh token」），
                            // 由平台從事件內容截出，可能含行內 Markdown（issue 標題是別人寫
                            // 的）。原本吃 14px 的 SEMIBOLD，字級與字重都明確給。
                            .child(
                                agent_text(
                                    SharedString::from(format!("trigger-{scope}-title-{i}")),
                                    r.title.clone(),
                                )
                                .text_size(px(14.))
                                .font_weight(FontWeight::SEMIBOLD),
                            ),
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
                .children(self.render_automation_sections(
                    "automations-page",
                    self.automation_filter,
                    true,
                    cx,
                ))
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

    /// 右欄「自動化」：這個 project 的監看設定、環境、排程與 listener。
    pub fn render_project_automations(
        &self,
        p: &ProjectView,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let watch = self.render_project_watch(p, cx);
        let env = self.render_project_env(p, cx);
        let inst = self.current_instance();
        // 空狀態看有沒有資料，不看計數：只剩已結束的排程時表格還是要看得到。
        let nothing = inst.schedules_for(Some(p.id)).count() == 0
            && inst.listeners_for(Some(p.id)).count() == 0;
        let body: Vec<AnyElement> = if nothing {
            vec![
                empty_state(
                    "這個 project 沒有自動化",
                    "請 agent 定時做某件事，或讓事件自動開工，例如「issue 加上 agent label 就開 Goal」。",
                )
                .into_any_element(),
            ]
        } else {
            self.render_automation_sections("project-automations-panel", Some(p.id), false, cx)
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
            .child(env)
            .children(body)
            .into_any_element()
    }

    /// 暫停橫幅（若有）、排程、listener 三塊，自動化頁的捲動定位依賴這個順序。
    /// `scope` 區分同一個畫面裡的兩個容器（實例層級的自動化頁、project 右欄的自動化
    /// 面板）：兩者會同時顯示同一批排程與 listener，元素 id 不能只靠 id 本身。
    fn render_automation_sections(
        &self,
        scope: &'static str,
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
            self.render_schedule_table(scope, project, show_project, cx)
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
        scope: &'static str,
        project: Option<ProjectId>,
        show_project: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let inst = self.current_instance();
        let paused = inst.automation_pause.is_some();
        let mut schedules: Vec<&ScheduleView> = inst.schedules_for(project).collect();
        sort_schedules(&mut schedules);
        // 小字用「全部」算（維持既有文案），再依開關決定實際要渲染的列。
        let total = schedules.len();
        let note = schedules_note(&schedules);
        let (schedules, hidden) = filter_finished(schedules, self.hide_finished_schedules);
        let note = hide_finished_note(note, hidden);
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
            let (next, next_color) = schedule_row_next(s, paused);
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
                            // 排程的 prompt 是使用者或 agent 寫的 Markdown，醒來時原文交給
                            // agent；表格列原本就會隨內容換行，這裡不截斷（截了會少字）。
                            // 原本吃這一列外層的 14px SEMIBOLD。
                            agent_text(
                                SharedString::from(format!("{scope}-prompt-{}", id.0)),
                                s.prompt.clone(),
                            )
                            .text_size(px(14.))
                            .font_weight(FontWeight::SEMIBOLD),
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

        // 全部都被藏起來時不能說「沒有排程」，那會和標題的「已隱藏 N 個」互相打架。
        let empty_text = if total == 0 {
            "沒有排程。"
        } else {
            "沒有未結束的排程（已全部隱藏）。"
        };

        div()
            .flex()
            .flex_col()
            .child(section_heading_with_action(
                "排程",
                note,
                chip(
                    // 實例頁與右欄 project 面板可能同畫面出現，id 要帶 scope。
                    SharedString::from(format!("{scope}-schedules-hide-finished")),
                    "隱藏已結束",
                    self.hide_finished_schedules,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.hide_finished_schedules = !this.hide_finished_schedules;
                    // 篩選改變時關掉右欄詳情，與 `automation_filter` 的既有慣例一致。
                    this.panel = None;
                    cx.notify();
                })),
            ))
            .child(
                table_frame()
                    .child(head)
                    .children(rows)
                    .when(schedules.is_empty(), |d| d.child(empty_row(empty_text))),
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

    // listener 表格裡沒有 agent 原文（標題與 meta 都是平台組的字），不需要 `scope`。
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

    /// 面板標題列。`title` 由呼叫端決定要純文字還是 Markdown：listener 的標題是平台
    /// 組的短字，排程的 prompt 是 agent／使用者寫的 Markdown（見 `render_schedule_panel`）。
    fn detail_header(
        &self,
        eyebrow: String,
        title: impl IntoElement,
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
                        // 模板會原文接上事件內容變成 Goal brief，本身就是 Markdown。
                        // 原本擔心 `{{…}}`、底線識別字之類的非常規語法會整段消失；實測
                        // （`markdown 1.0.0` 的 `to_mdast`，與 gpui-base 用的是同一套解析器）
                        // `{{"kind": …}}`、`legacy_login`、`a_b_c` 都解析成純文字節點，
                        // 不會丟字。殘餘風險只有行內 raw HTML（`<span>` 這類未支援的標籤
                        // 在行內層會被丟掉），使用者自己寫模板時少見，已在 PR 說明。
                        .child(
                            agent_text(
                                SharedString::from(format!("listener-template-{}", id.0)),
                                t,
                            )
                            .text_size(px(13.))
                            .line_height(relative(1.6)),
                        ),
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
            .child(kv(
                "下一次",
                if s.finished {
                    "已結束".into()
                } else if enabled {
                    next
                } else {
                    "已停用".into()
                },
            ))
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

        // 標題就是排程的 prompt：使用者或 agent 寫的 Markdown，醒來時原文交給 agent。
        // 標題列原本的 17px／BOLD／1.35 行高來自 `detail_header` 的外層 div，但
        // `TextView` 有自己的預設字級與行高會蓋掉，所以同一組值明確下在 `TextView` 上。
        let heading = agent_text(
            SharedString::from(format!("schedule-detail-prompt-{}", id.0)),
            s.prompt.clone(),
        )
        .text_size(px(17.))
        .font_weight(FontWeight::BOLD)
        .line_height(relative(1.35));

        panel_frame()
            .child(self.detail_header(
                format!("排程 · {}", self.project_label(s.project, "總管")),
                heading,
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
            // 已結束的單次排程 `enabled` 仍是 true，要另外排除，否則數字只增不減。
            AutomationSection::Schedules => inst.schedules.iter().filter(|s| s.active()).count(),
            AutomationSection::Listeners => inst
                .listeners
                .iter()
                .filter(|l| l.state == ListenerState::Active)
                .count(),
        };
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        filter_finished, hide_finished_note, schedule_row_next, schedules_note, sort_schedules,
    };
    use crate::model::*;
    use crate::theme;

    fn schedule(id: u64, enabled: bool, finished: bool) -> ScheduleView {
        ScheduleView {
            id: ScheduleId::from_u64(id),
            project: None,
            prompt: format!("排程 {id}"),
            note: String::new(),
            creator: ScheduleCreator::Planner,
            cadence: "單次".into(),
            next: "—".into(),
            next_after_pause: None,
            enabled,
            history: Vec::new(),
            next_at: None,
            expires_at: None,
            goal: None,
            finished,
        }
    }

    #[test]
    fn finished_schedules_sort_last() {
        let schedules = [
            schedule(1, false, false),
            schedule(2, true, true),
            schedule(3, true, false),
            schedule(4, true, true),
        ];
        let mut refs: Vec<&ScheduleView> = schedules.iter().collect();
        sort_schedules(&mut refs);
        // 啟用中的在前，接著停用的，已結束的沉到最後；同組內維持原本順序。
        let order: Vec<_> = refs.iter().map(|s| s.id).collect();
        assert_eq!(order, [3, 1, 2, 4].map(ScheduleId::from_u64));
    }

    #[test]
    fn header_counts_live_schedules_only() {
        let all = [schedule(1, true, false), schedule(2, true, true)];
        let refs: Vec<&ScheduleView> = all.iter().collect();
        assert_eq!(
            schedules_note(&refs),
            "1 個 · 每個專案最多 10 個，最短間隔 10 分鐘（另有 1 個已結束）"
        );
    }

    #[test]
    fn hide_finished_filters_and_reports_count() {
        let all = [
            schedule(1, true, false),
            schedule(2, true, true),
            schedule(3, true, true),
        ];
        let refs: Vec<&ScheduleView> = all.iter().collect();
        let note = schedules_note(&refs);

        let (rows, hidden) = filter_finished(refs.clone(), true);
        let ids: Vec<_> = rows.iter().map(|s| s.id).collect();
        assert_eq!(ids, [ScheduleId::from_u64(1)]);
        assert_eq!(hidden, 2);
        assert_eq!(
            hide_finished_note(note.clone(), hidden),
            "1 個 · 每個專案最多 10 個，最短間隔 10 分鐘（已隱藏 2 個已結束）"
        );

        // 開關關著：原樣回傳、不藏也不報。
        let (rows, hidden) = filter_finished(refs, false);
        assert_eq!(rows.len(), 3);
        assert_eq!(hidden, 0);
        assert_eq!(hide_finished_note(note, hidden), schedules_note(&rows));

        // 沒有已結束的排程時，開著也不會多出提示。
        let live = [schedule(1, true, false)];
        let live: Vec<&ScheduleView> = live.iter().collect();
        let note = schedules_note(&live);
        let (rows, hidden) = filter_finished(live, true);
        assert_eq!(rows.len(), 1);
        assert_eq!(hidden, 0);
        assert_eq!(hide_finished_note(note.clone(), hidden), note);
    }

    #[test]
    fn hide_finished_can_empty_the_table() {
        let all = [schedule(1, true, true), schedule(2, false, true)];
        let refs: Vec<&ScheduleView> = all.iter().collect();
        let (rows, hidden) = filter_finished(refs, true);
        assert!(rows.is_empty());
        assert_eq!(hidden, 2);
    }

    #[test]
    fn next_column_prefers_finished_over_enabled() {
        // 已結束的單次排程 `enabled` 仍是 true，不能顯示成還會觸發的時間。
        let mut s = schedule(1, true, true);
        assert_eq!(
            schedule_row_next(&s, false),
            ("已結束".to_string(), theme::text_muted())
        );
        s.finished = false;
        s.enabled = false;
        assert_eq!(
            schedule_row_next(&s, false),
            ("已停用".to_string(), theme::text_muted())
        );
        // 暫停期間還沒結束的排程照舊顯示恢復後的時間。
        s.enabled = true;
        s.next_after_pause = Some("週一 08:00".into());
        assert_eq!(
            schedule_row_next(&s, true),
            ("週一 08:00 後恢復".to_string(), theme::warn())
        );
        assert_eq!(
            schedule_row_next(&s, false),
            ("—".to_string(), theme::text())
        );
    }
}
