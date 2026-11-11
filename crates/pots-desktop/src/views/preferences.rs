//! M5 的設定與對話框：設定視窗的「通知」（桌面通知、靜音時段）與「USER.md」（跨 project 的
//! 使用者偏好）類別，以及拒絕核准時填理由、回答 agent 問題的對話框。
//!
//! 儲存送 `SaveNotifications`／`SaveUserMd`；服務回 `Notifications`／`UserMd` 時收起表單，被拒絕時
//!（`Error`）原因留在表單裡（見 `automation_forms::Forms`）。

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Div, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    px, relative,
};

use super::automation_forms::{chip, error_note, value};
use super::settings::{card, detail_row, field};
use super::widgets::*;
use crate::app::{DecideKind, NotificationEditor, PotsApp, UserMdEditor};
use crate::model::*;
use crate::theme;

fn muted_text(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(13.))
        .text_color(theme::text_muted())
        .line_height(relative(1.5))
        .child(text.into())
}

fn card_title(text: &str) -> Div {
    div()
        .text_size(px(15.))
        .font_weight(FontWeight::BOLD)
        .child(text.to_string())
}

/// 核准卡片與 Inbox 上的 diff（只顯示前面的部分）。
pub(super) fn diff_block(diff: &str, mono: SharedString) -> Div {
    const MAX_LINES: usize = 40;
    let lines: Vec<&str> = diff.lines().collect();
    let more = lines.len().saturating_sub(MAX_LINES);
    div()
        .flex()
        .flex_col()
        .px(px(10.))
        .py(px(8.))
        .rounded(px(6.))
        .bg(theme::subtle_bg())
        .border_1()
        .border_color(theme::border())
        .font_family(mono)
        .text_size(px(12.))
        .line_height(relative(1.45))
        .children(lines.into_iter().take(MAX_LINES).map(|l| {
            let color = if l.starts_with('+') && !l.starts_with("+++") {
                theme::ok()
            } else if l.starts_with('-') && !l.starts_with("---") {
                theme::danger()
            } else {
                theme::text_muted()
            };
            div().text_color(color).child(l.to_string())
        }))
        .when(more > 0, |d| {
            d.child(
                div()
                    .text_color(theme::text_muted())
                    .child(format!("…還有 {more} 行")),
            )
        })
}

/// 核准的 diff（修改 USER.md、Project Notes 的提案）。
pub(super) fn approval_diff(item: &InboxItem) -> Option<&str> {
    match &item.approval.as_ref()?.action {
        ApprovalActionView::EditUserMd { diff, .. }
        | ApprovalActionView::EditProjectNotes { diff, .. } => Some(diff),
        _ => None,
    }
}

/// agent 的問題：(問題, 選項, 可不可以自己寫答案)。
pub(super) fn question_of(item: &InboxItem) -> Option<(&str, &[String], bool)> {
    match &item.approval.as_ref()?.action {
        ApprovalActionView::AskUser {
            question,
            options,
            allow_other,
        } => Some((question, options, *allow_other)),
        _ => None,
    }
}

fn hh_mm_input(
    value: &str,
    window: &mut Window,
    cx: &mut Context<PotsApp>,
) -> gpui_kit::Entity<InputState> {
    let value = value.to_string();
    cx.new(|cx| {
        let mut state = InputState::new(window, cx).placeholder("HH:MM");
        state.set_value(value, window, cx);
        state
    })
}

impl PotsApp {
    // ---- 通知 -----------------------------------------------------------------

    pub fn edit_notifications(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.current_instance().notifications.clone();
        self.notification_editor = Some(NotificationEditor {
            enabled: n.enabled,
            quiet: n.quiet,
            start: hh_mm_input(&n.quiet_start, window, cx),
            end: hh_mm_input(&n.quiet_end, window, cx),
            saving: false,
            error: None,
        });
        cx.notify();
    }

    pub fn toggle_notification_flag(&mut self, quiet: bool, cx: &mut Context<Self>) {
        if let Some(e) = &mut self.notification_editor {
            if quiet {
                e.quiet = !e.quiet;
            } else {
                e.enabled = !e.enabled;
            }
            e.error = None;
        }
        cx.notify();
    }

    pub fn save_notifications(&mut self, cx: &mut Context<Self>) {
        let Some(e) = &self.notification_editor else {
            return;
        };
        let settings = NotificationSettings {
            enabled: e.enabled,
            quiet: e.quiet,
            quiet_start: value(&e.start, cx),
            quiet_end: value(&e.end, cx),
        };
        if e.quiet
            && (parse_hh_mm(&settings.quiet_start).is_none()
                || parse_hh_mm(&settings.quiet_end).is_none())
        {
            if let Some(e) = &mut self.notification_editor {
                e.error = Some("靜音時段的時間要是 HH:MM（例如 22:00）".into());
            }
            cx.notify();
            return;
        }
        let sent = self.settings_command(
            ClientCommand::SaveNotifications { settings },
            "儲存通知設定",
            cx,
        );
        if let Some(e) = &mut self.notification_editor {
            e.saving = sent;
            e.error = None;
        }
    }

    pub fn cancel_notifications(&mut self, cx: &mut Context<Self>) {
        self.notification_editor = None;
        cx.notify();
    }

    pub(crate) fn render_notifications_section(&self, cx: &mut Context<Self>) -> Div {
        let n = &self.current_instance().notifications;
        let held = self.notifier.as_ref().map_or(0, |x| x.held());
        let mut out = div().flex().flex_col().gap(px(12.)).child(muted_text(
            "有新的核准、任務需要你處理、Goal 完成時用系統通知告訴你；一般進度、CI 結果與子任務完成不通知。\
             視窗在前面、而且正看著那個 project 時不另外通知。",
        ));
        let quiet = if n.quiet {
            format!(
                "{} – {}（本機時間），結束時合併成一則",
                n.quiet_start, n.quiet_end
            )
        } else {
            "沒有".to_string()
        };
        out = out.child(
            card()
                .child(card_title("目前的設定"))
                .child(detail_row("通知", if n.enabled { "開啟" } else { "關閉" }))
                .child(detail_row("靜音時段", quiet))
                .when(held > 0, |d| {
                    d.child(muted_text(format!(
                        "靜音中，已累積 {held} 件，結束時一起通知。"
                    )))
                })
                .when(self.notification_editor.is_none(), |d| {
                    d.child(div().flex().child(
                        secondary_button("notifications-edit", "修改").on_click(
                            cx.listener(|this, _, window, cx| this.edit_notifications(window, cx)),
                        ),
                    ))
                }),
        );
        if let Some(e) = &self.notification_editor {
            out = out.child(self.render_notification_editor(e, cx));
        }
        out.child(muted_text(
            "點通知（或收到通知後幾分鐘內回到視窗）會開啟那一筆。Pots 還沒打包成 app 時，macOS 會把通知顯示成「Finder」送的。",
        ))
    }

    fn render_notification_editor(&self, e: &NotificationEditor, cx: &mut Context<Self>) -> Div {
        card()
            .bg(theme::subtle_bg())
            .child(card_title("修改通知設定"))
            .child(field(
                "桌面通知",
                None,
                div()
                    .flex()
                    .gap(px(8.))
                    .child(chip("notifications-on".into(), "開啟", e.enabled).on_click(
                        cx.listener(|this, _, _, cx| {
                            if !this.notification_editor.as_ref().is_some_and(|e| e.enabled) {
                                this.toggle_notification_flag(false, cx)
                            }
                        }),
                    ))
                    .child(
                        chip("notifications-off".into(), "關閉", !e.enabled).on_click(cx.listener(
                            |this, _, _, cx| {
                                if this.notification_editor.as_ref().is_some_and(|e| e.enabled) {
                                    this.toggle_notification_flag(false, cx)
                                }
                            },
                        )),
                    )
                    .into_any_element(),
            ))
            .child(field(
                "靜音時段",
                Some("期間的通知先累積在 Inbox，結束時合併成一則。開始比結束晚表示跨午夜。"),
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        chip("quiet-toggle".into(), "啟用靜音時段", e.quiet).on_click(
                            cx.listener(|this, _, _, cx| this.toggle_notification_flag(true, cx)),
                        ),
                    )
                    .when(e.quiet, |d| {
                        d.child(div().w(px(90.)).child(Input::new(&e.start)))
                            .child("到")
                            .child(div().w(px(90.)).child(Input::new(&e.end)))
                    })
                    .into_any_element(),
            ))
            .when_some(e.error.as_deref(), |d, m| d.child(error_note(m)))
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(
                        primary_button(
                            "notifications-save",
                            if e.saving { "儲存中…" } else { "儲存" },
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.save_notifications(cx))),
                    )
                    .child(
                        link_button("notifications-cancel", "取消")
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_notifications(cx))),
                    ),
            )
    }

    // ---- USER.md ------------------------------------------------------------------

    pub fn edit_user_md(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let u = self.current_instance().user_md.clone();
        let input = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx)
                .auto_grow(10, 24)
                .placeholder(
                    "例如：\n## 語言\n\n用繁體中文回答。\n\n## commit\n\n用 Conventional Commits。",
                );
            state.set_value(u.body.clone(), window, cx);
            state
        });
        self.user_md_editor = Some(UserMdEditor {
            input,
            base_rev: u.rev,
            saving: false,
            error: None,
        });
        cx.notify();
    }

    pub fn save_user_md(&mut self, cx: &mut Context<Self>) {
        let Some(e) = &self.user_md_editor else {
            return;
        };
        let body = e.input.read(cx).value().to_string();
        if body.chars().count() > MAX_USER_MD_CHARS {
            if let Some(e) = &mut self.user_md_editor {
                e.error = Some(format!("USER.md 最多 {MAX_USER_MD_CHARS} 個字"));
            }
            cx.notify();
            return;
        }
        let base_rev = e.base_rev;
        let sent = self.settings_command(
            ClientCommand::SaveUserMd { base_rev, body },
            "儲存 USER.md",
            cx,
        );
        if let Some(e) = &mut self.user_md_editor {
            e.saving = sent;
            e.error = None;
        }
    }

    pub fn cancel_user_md(&mut self, cx: &mut Context<Self>) {
        self.user_md_editor = None;
        cx.notify();
    }

    pub(crate) fn render_user_md_section(&self, cx: &mut Context<Self>) -> Div {
        let u = &self.current_instance().user_md;
        let mono = cx.theme().mono_font_family.clone();
        let mut out = div().flex().flex_col().gap(px(12.)).child(muted_text(
            "你是什麼樣的人：commit 風格、語言、程式碼習慣、溝通方式。對話 agent、Planner 與 worker 都把它放在指示的最前面，\
             所有 project 共用。agent 只能提案修改，你核准後才寫入。",
        ));
        match &self.user_md_editor {
            Some(e) => {
                let len = e.input.read(cx).value().chars().count();
                out = out.child(
                    card()
                        .bg(theme::subtle_bg())
                        .child(card_title(&format!("編輯 USER.md（目前 v{}）", e.base_rev)))
                        .child(Textarea::new(&e.input))
                        .when(len > USER_MD_HINT_CHARS, |d| {
                            d.child(muted_text(format!(
                                "已經 {len} 個字：太長會佔掉每次對話的空間，考慮整理成重點。"
                            )))
                        })
                        .when_some(e.error.as_deref(), |d, m| d.child(error_note(m)))
                        .child(
                            div()
                                .flex()
                                .gap(px(8.))
                                .child(
                                    primary_button(
                                        "user-md-save",
                                        if e.saving { "儲存中…" } else { "儲存" },
                                    )
                                    .on_click(cx.listener(|this, _, _, cx| this.save_user_md(cx))),
                                )
                                .child(link_button("user-md-cancel", "取消").on_click(
                                    cx.listener(|this, _, _, cx| this.cancel_user_md(cx)),
                                )),
                        ),
                );
            }
            None => {
                let body: AnyElement = if u.body.trim().is_empty() {
                    muted_text("還沒有內容。").into_any_element()
                } else {
                    // 刻意維持純文字：這裡是 USER.md 檔案的唯讀預覽（等寬字、逐行照原樣
                    // 顯示，和上面的編輯器一致），不是「該渲染卻輸出原樣」的地方。
                    // `TextView` 的內文排版走 `window.text_style()`，外層的
                    // `.font_family(...)` 對它不保證生效，改成渲染反而會失去等寬字，
                    // 使用者就沒辦法照原樣檢查自己的檔案。
                    div()
                        .px(px(12.))
                        .py(px(10.))
                        .rounded(px(8.))
                        .bg(theme::subtle_bg())
                        .font_family(mono)
                        .text_size(px(13.))
                        .line_height(relative(1.5))
                        .children(u.body.lines().map(|l| div().child(l.to_string())))
                        .into_any_element()
                };
                out = out.child(
                    card()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .child(card_title("USER.md"))
                                .when(u.rev > 0, |d| {
                                    d.child(tag(
                                        format!("v{}", u.rev),
                                        theme::chip_text(),
                                        theme::chip_bg(),
                                    ))
                                }),
                        )
                        .child(body)
                        .child(div().flex().child(
                            secondary_button("user-md-edit", "編輯").on_click(
                                cx.listener(|this, _, window, cx| this.edit_user_md(window, cx)),
                            ),
                        )),
                );
            }
        }
        out
    }

    // ---- 拒絕核准 ------------------------------------------------------------------

    pub(crate) fn render_reject_dialog(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let d = self.reject_dialog.as_ref()?;
        let item = self.store.approval(d.id);
        let title = item.map(|i| i.title.clone()).unwrap_or_default();
        let question = item.and_then(question_of);
        let (heading, hint, confirm) = match d.kind {
            DecideKind::Reject => (
                format!("拒絕：{title}"),
                "可以附一句理由，agent 會照著改（merge 被拒絕時 worker 會被叫醒）。Enter 送出，Esc 取消。",
                "拒絕",
            ),
            DecideKind::Answer => (
                question
                    .map(|(q, _, _)| q.to_string())
                    .unwrap_or(title.clone()),
                "選一個選項，或寫下你的答案（會原文轉告 agent）。Enter 送出，Esc 取消。",
                "送出答案",
            ),
            DecideKind::Skip => (
                format!("略過：{title}"),
                "可以附一句理由，agent 會依現有資訊自己決定。Enter 送出，Esc 取消。",
                "略過",
            ),
        };
        let id = d.id;
        let (options, free) = match (d.kind, question) {
            (DecideKind::Answer, Some((_, options, allow_other))) => {
                (options.to_vec(), allow_other)
            }
            _ => (Vec::new(), true),
        };
        let dialog = div()
            .id("reject-dialog")
            .w(px(520.))
            .max_w(relative(0.9))
            .flex()
            .flex_col()
            .gap(px(12.))
            .p(px(20.))
            .rounded(px(12.))
            .bg(theme::white())
            .shadow_lg()
            .child(
                div()
                    .text_size(px(16.))
                    .font_weight(FontWeight::BOLD)
                    .line_height(relative(1.4))
                    .child(heading),
            )
            .child(muted_text(if free {
                hint
            } else {
                "選一個選項（會原文轉告 agent）。Esc 取消。"
            }))
            .when(!options.is_empty(), |el| {
                el.child(div().flex().flex_col().items_start().gap(px(8.)).children(
                    options.into_iter().enumerate().map(|(i, o)| {
                        let label = o.clone();
                        option_button(SharedString::from(format!("answer-opt-{i}")), label)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.answer_option(id, o.clone(), cx)
                            }))
                    }),
                ))
            })
            .when(free, |el| el.child(Input::new(&d.input)))
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .when(free, |el| {
                        el.child(
                            primary_button("reject-confirm", confirm)
                                .on_click(cx.listener(|this, _, _, cx| this.confirm_reject(cx))),
                        )
                    })
                    .child(
                        link_button("reject-cancel", "取消")
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_reject(cx))),
                    ),
            )
            .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
                cx.stop_propagation()
            })
            .on_click(|_, _, cx| cx.stop_propagation());
        Some(
            div()
                .id("reject-backdrop")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui_kit::hsla(0., 0., 0., 0.45))
                .occlude()
                .child(dialog)
                .on_click(cx.listener(|this, _, _, cx| this.cancel_reject(cx)))
                .into_any_element(),
        )
    }
}
