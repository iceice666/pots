//! 頻道「自動化」分頁的「環境」區塊：project 的 worker 環境（環境 worker 分析 repo 後寫出、
//! 驗證過才套用的 [`EnvSpec`]）。
//!
//! 看得到使用中的版本與它的規格、版本歷史；「分析並建立環境」送 `SetupProjectEnv`（可以附一段
//! 說明），「使用這一版」／「不用環境」送 `UseProjectEnv`。服務以 `ProjectEnv` 事件整份替換
//! 環境；被拒絕時（`Error`，例如已經在分析、還沒有工作資料夾）原因留在表單裡。

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::input::{Input, InputEvent};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Context, Div, FontWeight, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px, relative,
};

use super::automation_forms::{error_note, input, small_button, value};
use super::panel::white_box;
use super::widgets::*;
use crate::app::{EnvEditor, GoalTab, Panel, PotsApp};
use crate::model::*;
use crate::sync::month_day_time;
use crate::theme;

/// 摘要在狀態列只顯示這麼多字。
const SUMMARY_CHARS: usize = 120;

/// 截成 `max` 個字，多的換成「…」；只取第一行。
fn one_line(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    let mut out: String = line.chars().take(max).collect();
    if line.chars().count() > max || text.trim().lines().count() > 1 {
        out.push('…');
    }
    out
}

fn muted(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(12.))
        .text_color(theme::text_muted())
        .line_height(relative(1.5))
        .child(text.into())
}

/// 不能按的按鈕（外觀和 `secondary_button` 一樣，淡一點）。
fn disabled_button(id: &'static str, label: &str) -> gpui_kit::Stateful<Div> {
    small_button(secondary_button(id, label.to_string()))
        .opacity(0.5)
        .cursor_default()
}

impl PotsApp {
    // ---- 動作 -------------------------------------------------------------------

    /// 打開「分析並建立環境」的表單。
    pub fn edit_project_env(
        &mut self,
        project: ProjectId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = input(
            String::new(),
            "給環境 worker 的說明（可留白）",
            false,
            window,
            cx,
        );
        state.update(cx, |s, cx| s.focus(window, cx));
        let sub = cx.subscribe(&state, |this, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) {
                this.submit_project_env(cx);
            }
        });
        self.env_editor = Some(EnvEditor::new(project, state, sub));
        cx.notify();
    }

    pub fn cancel_project_env(&mut self, cx: &mut Context<Self>) {
        self.env_editor = None;
        cx.notify();
    }

    /// 送 `SetupProjectEnv`；服務回 `ProjectEnv` 時收起表單（見 `Forms::applied`）。
    pub fn submit_project_env(&mut self, cx: &mut Context<Self>) {
        let Some(e) = &self.env_editor else {
            return;
        };
        if e.saving {
            return;
        }
        let project = e.project;
        let note = value(&e.note, cx);
        let sent = self.settings_command(
            ClientCommand::SetupProjectEnv { project, note },
            "分析並建立環境",
            cx,
        );
        if let Some(e) = &mut self.env_editor {
            e.saving = sent;
            e.error = None;
        }
        cx.notify();
    }

    /// 改用某一版環境（`None` 是不用環境）。先在畫面上換，服務回 `ProjectEnv` 時以它為準。
    pub fn use_project_env(
        &mut self,
        project: ProjectId,
        rev: Option<u32>,
        cx: &mut Context<Self>,
    ) {
        let instance = self.instance;
        if let Some(p) = self.store.project_mut(instance, project) {
            p.env.current = rev;
        }
        self.store
            .outbox
            .push((instance, ClientCommand::UseProjectEnv { project, rev }));
        cx.notify();
    }

    pub fn toggle_env_detail(&mut self, project: ProjectId, cx: &mut Context<Self>) {
        self.env_detail = if self.env_detail == Some(project) {
            None
        } else {
            Some(project)
        };
        cx.notify();
    }

    // ---- 畫面 -------------------------------------------------------------------

    /// 「環境」區塊：狀態、分析按鈕（或表單）、目前規格與版本歷史。
    pub(crate) fn render_project_env(&self, p: &ProjectView, cx: &mut Context<Self>) -> Div {
        let project = p.id;
        let env = &p.env;
        let current = env.current_rev();
        let running = env.setup_goal;
        let editor = self.env_editor.as_ref().filter(|e| e.project == project);

        let mut b = white_box().gap(px(8.)).child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .flex_wrap()
                .child(section_label("環境").flex_1())
                .child(match current {
                    Some(r) => tag(
                        format!("使用中：第 {} 版", r.rev),
                        theme::ok(),
                        theme::ok_soft(),
                    ),
                    None => tag("沒有環境", theme::text_muted(), theme::track()),
                }),
        );

        // 狀態。
        b = match current {
            Some(r) => b.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(px(13.))
                            .line_height(relative(1.5))
                            .child(one_line(&r.summary, SUMMARY_CHARS)),
                    )
                    .child(muted(format!("建立於 {}", month_day_time(r.created_at)))),
            ),
            None => b.child(
                div()
                    .text_size(px(13.))
                    .text_color(theme::text_muted())
                    .child("還沒有環境：worker 用「Worker」設定的 image"),
            ),
        };

        // 分析中、表單或按鈕。
        if let Some(goal) = running {
            let exists = self.store.goal(self.instance, goal).is_some();
            b = b.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .flex_wrap()
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::warn_text())
                            .child("環境分析進行中…"),
                    )
                    .when(exists, |d| {
                        d.child(
                            link_button("env-open-goal", "查看進度")
                                .text_size(px(13.))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_panel(Some(Panel::Goal(goal, GoalTab::Tasks)), cx)
                                })),
                        )
                    }),
            );
        } else if let Some(e) = editor {
            b = b
                .child(Input::new(&e.note))
                .when_some(e.error.as_deref(), |d, m| d.child(error_note(m)))
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(
                            small_button(primary_button(
                                "env-submit",
                                if e.saving { "送出中…" } else { "送出" },
                            ))
                            .on_click(cx.listener(|this, _, _, cx| this.submit_project_env(cx))),
                        )
                        .child(
                            link_button("env-cancel", "取消").on_click(
                                cx.listener(|this, _, _, cx| this.cancel_project_env(cx)),
                            ),
                        ),
                );
        } else {
            let label = if env.revs.is_empty() {
                "分析並建立環境"
            } else {
                "重新分析"
            };
            let row = div().flex().items_center().gap(px(8.)).flex_wrap();
            b = b.child(if p.workdir.is_none() {
                row.child(disabled_button("env-setup", label)).child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme::warn_text())
                        .child("先設定工作資料夾"),
                )
            } else {
                row.child(
                    small_button(secondary_button("env-setup", label)).on_click(cx.listener(
                        move |this, _, window, cx| this.edit_project_env(project, window, cx),
                    )),
                )
            });
        }
        b = b.child(muted(
            "環境 worker 讀 repo、寫出安裝與準備腳本並實際驗證，通過就直接套用（舊版本留著，可以換回去）。\
             之後的 worker 用它建出的 image，每次開始先跑準備腳本，60 分鐘的計時從準備完才開始。",
        ));

        // 目前的規格（唯讀，可收合）。
        if let Some(r) = current {
            let open = self.env_detail == Some(project);
            b = b.child(
                div().flex().child(
                    link_button(
                        "env-detail-toggle",
                        if open { "收起規格" } else { "顯示規格" },
                    )
                    .text_size(px(13.))
                    .px(px(0.))
                    .py(px(2.))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.toggle_env_detail(project, cx)),
                    ),
                ),
            );
            if open {
                b = b.child(self.env_spec_detail(&r.spec, cx));
            }
        }

        // 版本歷史（新的在前）。
        if !env.revs.is_empty() {
            b = b
                .child(div().h(px(1.)).bg(theme::border()))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(section_label("版本").pt(px(0.)).flex_1())
                        .when(env.current.is_some(), |d| {
                            d.child(
                                link_button("env-use-none", "不用環境")
                                    .text_size(px(13.))
                                    .px(px(0.))
                                    .py(px(2.))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.use_project_env(project, None, cx)
                                    })),
                            )
                        }),
                )
                .children(env.revs.iter().map(|r| {
                    let rev = r.rev;
                    let in_use = env.current == Some(rev);
                    // 第一行：版本、時間與動作；第二行：一行摘要。
                    let head = div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .flex_none()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(format!("第 {rev} 版")),
                        )
                        .child(
                            div()
                                .flex_1()
                                .text_color(theme::text_muted())
                                .child(month_day_time(r.created_at)),
                        )
                        .child(if in_use {
                            tag("使用中", theme::ok(), theme::ok_soft())
                                .flex_none()
                                .into_any_element()
                        } else {
                            link_button(SharedString::from(format!("env-use-{rev}")), "使用這一版")
                                .text_size(px(13.))
                                .px(px(0.))
                                .py(px(2.))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.use_project_env(project, Some(rev), cx)
                                }))
                                .into_any_element()
                        });
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .text_size(px(13.))
                        .child(head)
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_color(theme::text_muted())
                                .child(one_line(&r.summary, SUMMARY_CHARS)),
                        )
                }));
        }
        b
    }

    /// 一份規格的內容：腳本用等寬字，環境變數 `K=V` 一行一個。
    fn env_spec_detail(&self, spec: &EnvSpec, cx: &mut Context<Self>) -> Div {
        let mono = cx.theme().mono_font_family.clone();
        let block = |label: &str, text: String| {
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(muted(label.to_string()))
                .child(
                    div()
                        .px(px(10.))
                        .py(px(8.))
                        .rounded(px(6.))
                        .border_1()
                        .border_color(theme::border())
                        .bg(theme::subtle_bg())
                        .font_family(mono.clone())
                        .text_size(px(12.))
                        .line_height(relative(1.5))
                        .when_else(
                            text.trim().is_empty(),
                            |d| d.text_color(theme::text_muted()).child("（沒有）"),
                            |d| d.children(text.lines().map(|l| div().child(l.to_string()))),
                        ),
                )
        };
        let base = if spec.base_image.is_empty() {
            "「Worker」設定的 image".to_string()
        } else {
            spec.base_image.clone()
        };
        let env_lines = spec
            .env
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("\n");
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(muted(format!("基底 image：{base}")))
            .child(block(
                "安裝（建 image 時以 root 執行）",
                spec.install.clone(),
            ))
            .child(block("準備（每次 run 開始時執行）", spec.prepare.clone()))
            .child(block("驗證", spec.verify.clone()))
            .child(block("環境變數", env_lines))
            .child(block("快取（跨 run 保留）", spec.caches.join("\n")))
    }
}
