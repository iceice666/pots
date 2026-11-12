//! 設定視窗的「Worker」類別：coding worker 的後端（內建或 Claude Code）、容器執行檔與
//! image、Claude Code 的模型、同時執行的 worker 數，以及登入 Claude Code 的指令。worker
//! 一律在容器裡執行（沒有本機模式）；project 有「環境」時改用環境建出的 image（見頻道的
//! 「專案設定」）。
//!
//! 「儲存」送 `SaveWorkerSettings`；服務回 `Worker` 時收起表單，被拒絕時（`Error`）原因留在
//! 表單裡（見 `automation_forms::Forms`）。設定只影響之後開始的任務。

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::input::Input;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    ClipboardItem, Context, Div, FontWeight, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px, relative,
};

use super::automation_forms::{chip, error_note, input, value};
use super::settings::{card, detail_row, field};
use super::widgets::*;
use crate::app::{PotsApp, WorkerEditor};
use crate::model::*;
use crate::theme;

/// 建立 worker image 的指令（在 repo 根目錄執行）。
pub const IMAGE_BUILD_COMMAND: &str = "docker build -t pots-worker:latest docker/worker";

const CLAUDE_LOGIN_NOTE: &str = "Claude Code 由你自己官方登入：在終端機執行下面的指令，在 Claude Code \
     裡輸入 /login 完成登入後離開。Pots 不會讀取或轉存你的 Claude 憑證。";

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

impl PotsApp {
    pub fn edit_worker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let w = self.current_instance().worker.clone();
        self.worker_editor = Some(WorkerEditor {
            backend: w.backend,
            runtime: input(w.runtime, "docker", false, window, cx),
            image: input(w.image, "pots-worker:latest", false, window, cx),
            claude_model: input(
                w.claude_model,
                "留空用 Claude Code 的預設",
                false,
                window,
                cx,
            ),
            max_parallel: w.max_parallel.clamp(1, 8),
            saving: false,
            error: None,
        });
        cx.notify();
    }

    pub fn cancel_worker(&mut self, cx: &mut Context<Self>) {
        self.worker_editor = None;
        cx.notify();
    }

    pub fn set_worker_backend(&mut self, backend: WorkerBackendKind, cx: &mut Context<Self>) {
        if let Some(e) = &mut self.worker_editor {
            e.backend = backend;
            e.error = None;
        }
        cx.notify();
    }

    pub fn set_worker_parallel(&mut self, n: u8, cx: &mut Context<Self>) {
        if let Some(e) = &mut self.worker_editor {
            e.max_parallel = n;
            e.error = None;
        }
        cx.notify();
    }

    pub fn save_worker(&mut self, cx: &mut Context<Self>) {
        let Some(e) = &self.worker_editor else {
            return;
        };
        let draft = WorkerSettingsDraft {
            backend: e.backend,
            // worker 只在容器裡執行。
            exec: ExecKind::Container,
            runtime: value(&e.runtime, cx),
            image: value(&e.image, cx),
            claude_model: value(&e.claude_model, cx),
            max_parallel: e.max_parallel,
        };
        let sent = self.settings_command(
            ClientCommand::SaveWorkerSettings { draft },
            "儲存 worker 設定",
            cx,
        );
        if let Some(e) = &mut self.worker_editor {
            e.saving = sent;
            e.error = None;
        }
    }

    /// 把指令放進剪貼簿；`which` 決定哪個按鈕顯示「已複製」。
    pub fn copy_command(&mut self, which: &'static str, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.copied = Some(which);
        cx.notify();
    }

    /// 等寬字體的指令，加一個「複製」按鈕。
    fn command_box(&self, which: &'static str, text: String, cx: &mut Context<Self>) -> Div {
        let copied = self.copied == Some(which);
        let copy_text = text.clone();
        div()
            .flex()
            .items_start()
            .gap(px(10.))
            .px(px(12.))
            .py(px(10.))
            .rounded(px(8.))
            .border_1()
            .border_color(theme::border())
            .bg(theme::subtle_bg())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_size(px(12.))
                    .line_height(relative(1.5))
                    .child(text),
            )
            .child(
                secondary_button(
                    SharedString::from(format!("worker-copy-{which}")),
                    if copied { "已複製" } else { "複製" },
                )
                .flex_none()
                .text_size(px(12.))
                .px(px(10.))
                .py(px(3.))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.copy_command(which, copy_text.clone(), cx)
                })),
            )
    }

    pub(crate) fn render_worker_section(&self, cx: &mut Context<Self>) -> Div {
        let w = &self.current_instance().worker;
        let editor = self.worker_editor.as_ref();
        let mut out = div().flex().flex_col().gap(px(12.)).child(muted_text(
            "Goal 的任務交給 worker 在自己的分支上寫程式、跑測試、開 PR。改了設定只影響之後開始的任務。",
        ));

        let model = if w.claude_model.is_empty() {
            "Claude Code 的預設".to_string()
        } else {
            w.claude_model.clone()
        };
        let summary =
            card()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(card_title("目前的設定"))
                        .child(tag(w.backend.label(), theme::chip_text(), theme::chip_bg())),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(detail_row(
                            "後端",
                            match w.backend {
                                WorkerBackendKind::Builtin => {
                                    "內建 worker（用「模型 Provider」使用中的模型）"
                                }
                                WorkerBackendKind::ClaudeCode => {
                                    "Claude Code（官方 CLI，用你的 Claude 帳號）"
                                }
                            },
                        ))
                        .child(detail_row("容器", format!("{} · {}", w.runtime, w.image)))
                        .when(w.backend == WorkerBackendKind::ClaudeCode, |d| {
                            d.child(detail_row("模型", model))
                        })
                        .child(detail_row(
                            "同時執行",
                            format!("最多 {} 個 worker", w.max_parallel),
                        )),
                )
                .when(editor.is_none(), |d| {
                    d.child(div().flex().child(
                        secondary_button("worker-edit", "修改").on_click(
                            cx.listener(|this, _, window, cx| this.edit_worker(window, cx)),
                        ),
                    ))
                });
        out = out.child(summary);

        if let Some(e) = editor {
            out = out.child(self.render_worker_editor(e, cx));
        }

        // 登入 Claude Code：指令依「已儲存」的設定產生。
        let claude = w.backend == WorkerBackendKind::ClaudeCode
            || editor.is_some_and(|e| e.backend == WorkerBackendKind::ClaudeCode);
        if claude {
            out = out.child(
                card()
                    .child(card_title("登入 Claude Code"))
                    .child(muted_text(CLAUDE_LOGIN_NOTE))
                    .child(self.command_box("login", w.login_command.clone(), cx)),
            );
        }

        out.child(
            card()
                .child(card_title("Worker 的容器 image"))
                .child(muted_text(
                    "每個任務起一個容器，只掛任務的工作目錄（Claude Code 另外掛它專屬的 home）。\
                     image 用 repo 附的 docker/worker/Dockerfile 建立，在 repo 根目錄執行：",
                ))
                .child(self.command_box("image", IMAGE_BUILD_COMMAND.to_string(), cx))
                .child(muted_text(
                    "project 建立了「環境」時，worker 改用環境以這個 image 為基底建出的 image\
                     （在頻道的「專案設定」管理）。",
                )),
        )
    }

    fn render_worker_editor(&self, e: &WorkerEditor, cx: &mut Context<Self>) -> Div {
        let backend_chips = div().flex().flex_wrap().gap(px(8.)).children(
            [WorkerBackendKind::Builtin, WorkerBackendKind::ClaudeCode].map(|b| {
                chip(
                    SharedString::from(format!("worker-backend-{b:?}")),
                    b.label(),
                    e.backend == b,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.set_worker_backend(b, cx)))
            }),
        );
        let parallel_chips = div()
            .flex()
            .flex_wrap()
            .gap(px(6.))
            .children((1..=8u8).map(|n| {
                chip(
                    SharedString::from(format!("worker-parallel-{n}")),
                    &n.to_string(),
                    e.max_parallel == n,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.set_worker_parallel(n, cx)))
            }));

        card()
            .bg(theme::subtle_bg())
            .child(card_title("修改 worker 設定"))
            .child(field(
                "後端",
                Some(match e.backend {
                    WorkerBackendKind::Builtin => {
                        "Pots 自己的 worker，用「模型 Provider」裡使用中的模型"
                    }
                    WorkerBackendKind::ClaudeCode => {
                        "未修改的官方 Claude Code CLI，用你自己登入的 Claude 帳號與額度"
                    }
                }),
                backend_chips.into_any_element(),
            ))
            .child(field(
                "容器執行檔",
                Some("每個任務一個容器；docker 或 podman，也可以是完整路徑"),
                Input::new(&e.runtime).into_any_element(),
            ))
            .child(field(
                "Image",
                Some("用 repo 的 docker/worker/Dockerfile 建立：docker build -t pots-worker:latest docker/worker"),
                Input::new(&e.image).into_any_element(),
            ))
            .when(e.backend == WorkerBackendKind::ClaudeCode, |d| {
                d.child(field(
                    "Claude Code 模型",
                    Some("例如 sonnet、opus；留空用 CLI 的預設"),
                    Input::new(&e.claude_model).into_any_element(),
                ))
            })
            .child(field(
                "同時執行的 worker 數",
                Some("同一個實例最多同時跑幾個任務，其餘排隊"),
                parallel_chips.into_any_element(),
            ))
            .when_some(e.error.as_deref(), |d, m| d.child(error_note(m)))
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(
                        primary_button("worker-save", if e.saving { "儲存中…" } else { "儲存" })
                            .on_click(cx.listener(|this, _, _, cx| this.save_worker(cx))),
                    )
                    .child(
                        link_button("worker-cancel", "取消")
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_worker(cx))),
                    ),
            )
    }
}
