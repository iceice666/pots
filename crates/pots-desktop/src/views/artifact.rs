//! Artifacts：頻道「Artifacts」分頁的列表，與右欄的 Markdown 編輯器。
//!
//! 編輯器逐 `##` 區段各開一個 `gpui-kit-markdown-editor`：你改過的區段與 agent 正在寫入的
//! 區段各自標示出來，agent 正在寫的那段唯讀，其他區段可以照常編輯。修改按「儲存」才送出；
//! 「版本紀錄」列出每一版的作者與 diff，可以還原。

use gpui_kit::assets::IconName;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, Div, FontWeight, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, div, px,
    relative,
};
use markdown_editor::BlockEditor;

use super::channel::empty_state;
use super::widgets::*;
use crate::app::{ArtifactEditor, ArtifactHistory, Panel, PotsApp};
use crate::model::*;
use crate::theme;

pub fn kind_colors(kind: ArtifactKind) -> (Hsla, Hsla) {
    match kind {
        ArtifactKind::Spec => (theme::violet(), theme::violet_soft()),
        ArtifactKind::Brief => (theme::accent_text(), theme::accent_soft()),
        ArtifactKind::Ledger => (theme::ok(), theme::ok_soft()),
        ArtifactKind::Report => (theme::chip_text(), theme::chip_bg()),
        ArtifactKind::ProjectNotes => (theme::warn(), theme::warn_soft()),
    }
}

pub fn kind_tag(kind: ArtifactKind) -> Div {
    let (fg, bg) = kind_colors(kind);
    tag(kind.tag(), fg, bg).px(px(8.)).py(px(2.))
}

/// 區段框右上角的小字，例如「你在 10:20 改過」。
fn section_label(text: impl Into<SharedString>, color: Hsla) -> Div {
    div()
        .absolute()
        .top(px(10.))
        .right(px(12.))
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(color)
        .child(text.into())
}

fn notice(fg: Hsla, bg: Hsla) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(10.))
        .px(px(14.))
        .py(px(10.))
        .rounded(px(8.))
        .bg(bg)
        .text_color(fg)
        .text_size(px(13.))
        .line_height(relative(1.5))
}

impl PotsApp {
    /// 右欄「Artifacts」：這個 project 的所有 artifact，點了右欄換成編輯器。
    pub fn render_artifact_list(&self, p: &ProjectView, cx: &mut Context<Self>) -> AnyElement {
        if p.artifacts.is_empty() {
            return empty_state(
                "還沒有 Artifact",
                "討論功能時 agent 會把 spec 寫成 artifact；開 Goal 時會有 Brief 與 Ledger。",
            )
            .into_any_element();
        }
        let open = match self.panel {
            Some(Panel::Artifact(id)) => Some(id),
            _ => None,
        };
        div()
            .id("artifact-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(16.))
            .py(px(16.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .children(p.artifacts.iter().map(|a| {
                let id = a.id;
                let selected = open == Some(id);
                div()
                    .id(SharedString::from(format!("artifact-row-{}", id.0)))
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .rounded(px(10.))
                    .bg(theme::white())
                    .cursor_pointer()
                    // 選取時邊框加粗，內距跟著減少，避免內容位移。
                    .when_else(
                        selected,
                        |d| {
                            d.border_2()
                                .border_color(theme::accent())
                                .px(px(13.))
                                .py(px(11.))
                        },
                        |d| {
                            d.border_1()
                                .border_color(theme::border())
                                .px(px(14.))
                                .py(px(12.))
                                .hover(|s| s.border_color(theme::border_strong()))
                        },
                    )
                    .child(kind_tag(a.kind))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(a.title.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .text_color(theme::text_muted())
                                    .truncate()
                                    .child(a.list_note()),
                            ),
                    )
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.open_artifact(id, window, cx)),
                    )
            }))
            .into_any_element()
    }

    pub fn render_artifact_panel(
        &self,
        a: &ArtifactView,
        editor: &ArtifactEditor,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let subtitle = if editor.saving {
            format!("v{} · 儲存中…", a.rev)
        } else if editor.dirty {
            format!("v{} · 未儲存", a.rev)
        } else if a.agent_writing.is_some() {
            format!("v{} · 你與 agent 共同編輯", a.rev)
        } else {
            format!("v{}", a.rev)
        };
        let small = |b: gpui_kit::Stateful<Div>| b.text_size(px(13.)).px(px(12.)).py(px(5.));

        let header = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(24.))
            .py(px(14.))
            .border_b_1()
            .border_color(theme::border())
            .child(kind_tag(a.kind))
            .child(
                div()
                    .min_w_0()
                    .text_size(px(16.))
                    .font_weight(FontWeight::BOLD)
                    .truncate()
                    .child(a.title.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(13.))
                    .text_color(theme::text_muted())
                    .child(subtitle),
            )
            .child(
                div()
                    .ml_auto()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(8.))
                    .when(editor.dirty && editor.history.is_none(), |d| {
                        d.child(
                            small(secondary_button("artifact-discard", "放棄修改")).on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.discard_artifact_changes(window, cx)
                                }),
                            ),
                        )
                        .child(
                            small(primary_button(
                                "artifact-save",
                                if editor.saving {
                                    "儲存中…"
                                } else {
                                    "儲存"
                                },
                            ))
                            .on_click(cx.listener(|this, _, _, cx| this.save_artifact(cx))),
                        )
                    })
                    .child(
                        small(secondary_button(
                            "artifact-history",
                            if editor.history.is_some() {
                                "回到編輯"
                            } else {
                                "版本紀錄"
                            },
                        ))
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_artifact_history(cx))),
                    )
                    .child(self.close_button(cx)),
            );

        let error = editor.error.as_ref().map(|e| {
            notice(theme::warn_text(), theme::warn_soft())
                .child(icon(IconName::CircleAlert, 15., theme::warn()))
                .child(div().flex_1().child(e.clone()))
        });
        let remote = editor.remote_rev.filter(|_| editor.dirty).map(|rev| {
            notice(theme::accent_text(), theme::accent_soft())
                .child(icon(IconName::Info, 15., theme::accent()))
                .child(div().flex_1().child(format!(
                    "你編輯期間已更新到 v{rev}。儲存時會和它以區段合併；同一段兩邊都改過會被拒絕。"
                )))
        });

        if let Some(history) = &editor.history {
            return div()
                .flex_none()
                .w(px(620.))
                .h_full()
                .bg(theme::white())
                .border_l_1()
                .border_color(theme::border())
                .flex()
                .flex_col()
                .child(header)
                .children(error.map(|e| div().px(px(16.)).pt(px(12.)).child(e)))
                .child(self.render_artifact_history(a, history, cx))
                .into_any_element();
        }

        let writing = a.agent_writing.as_ref().map(|heading| {
            notice(theme::violet(), theme::violet_soft())
                .child(dot(theme::violet(), 7.))
                .child(format!(
                    "agent 正在更新「{heading}」段落。你可以繼續編輯其他段落，它寫入前會先讀你的修改。"
                ))
        });
        let proposals = (a.proposals > 0).then(|| {
            notice(theme::warn_text(), theme::warn_soft())
                .child(icon(IconName::Inbox, 15., theme::warn()))
                .child(div().flex_1().child(format!(
                    "agent 提了 {} 個修改提案。{}只接受提案，你核准後才會寫入。",
                    a.proposals,
                    a.kind.label()
                )))
                .child(
                    link_button("artifact-proposals", "看提案")
                        .text_size(px(13.))
                        .py(px(0.))
                        .on_click(
                            cx.listener(|this, _, _, cx| this.notify_unimplemented("看提案", cx)),
                        ),
                )
        });

        let sections = editor.sections.iter().map(|s| {
            let heading = s.heading.as_deref();
            let agent = heading.is_some() && heading == a.agent_writing.as_deref();
            let edited = heading.and_then(|h| a.edited_at(h));
            div()
                .relative()
                .px(px(12.))
                .py(px(4.))
                .rounded(px(8.))
                .border_1()
                .map(|d| {
                    if agent {
                        d.border_dashed()
                            .border_color(theme::violet().opacity(0.45))
                            .child(section_label("agent 更新中", theme::violet()))
                    } else if let Some(at) = edited {
                        d.border_color(gpui_kit::transparent_black())
                            .bg(theme::accent_soft())
                            .child(section_label(
                                format!("你在 {} 改過", at.hh_mm()),
                                theme::accent_text(),
                            ))
                    } else {
                        d.border_color(gpui_kit::transparent_black())
                    }
                })
                .child(BlockEditor::new(&s.editor))
        });

        div()
            .flex_none()
            .w(px(620.))
            .h_full()
            .bg(theme::white())
            .border_l_1()
            .border_color(theme::border())
            .flex()
            .flex_col()
            .child(header)
            .child(
                div()
                    .id("artifact-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&editor.scroll)
                    .px(px(16.))
                    .py(px(16.))
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .children(error)
                    .children(remote)
                    .children(writing)
                    .children(proposals)
                    .children(sections),
            )
            .into_any_element()
    }

    /// 版本紀錄：左邊是版本列表，右邊是選取版本和前一版的 diff 與「還原」。
    fn render_artifact_history(
        &self,
        a: &ArtifactView,
        history: &ArtifactHistory,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(revisions) = &history.revisions else {
            return div()
                .p(px(24.))
                .text_color(theme::text_muted())
                .child("讀取版本紀錄…")
                .into_any_element();
        };
        let list = div()
            .id("artifact-revisions")
            .flex_none()
            .w(px(180.))
            .h_full()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(theme::border())
            .py(px(8.))
            .children(revisions.iter().enumerate().map(|(ix, r)| {
                let selected = ix == history.selected;
                div()
                    .id(SharedString::from(format!("artifact-rev-{}", r.rev)))
                    .px(px(14.))
                    .py(px(8.))
                    .cursor_pointer()
                    .when(selected, |d| d.bg(theme::accent_soft()))
                    .hover(|s| s.bg(theme::subtle_bg()))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(if r.rev == a.rev {
                                format!("v{}（目前）", r.rev)
                            } else {
                                format!("v{}", r.rev)
                            }),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::text_muted())
                            .child(format!("{} · {}", r.author, r.at.hh_mm())),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.select_revision(ix, cx)))
            }));

        let detail = revisions.get(history.selected).map(|r| {
            let rev = r.rev;
            let mono = cx.theme().mono_font_family.clone();
            let diff = if r.diff.is_empty() {
                div()
                    .text_color(theme::text_muted())
                    .child("第一版，沒有可以比較的前一版。")
            } else {
                div()
                    .flex()
                    .flex_col()
                    .font_family(mono)
                    .text_size(px(12.))
                    .children(r.diff.lines().map(|line| {
                        let (fg, bg) = if line.starts_with("+++") || line.starts_with("---") {
                            (theme::text_muted(), gpui_kit::transparent_black())
                        } else if line.starts_with('+') {
                            (theme::ok(), theme::ok_soft())
                        } else if line.starts_with('-') {
                            (theme::danger(), theme::danger_soft())
                        } else if line.starts_with("@@") {
                            (theme::accent_text(), gpui_kit::transparent_black())
                        } else {
                            (theme::text(), gpui_kit::transparent_black())
                        };
                        div()
                            .px(px(6.))
                            .text_color(fg)
                            .bg(bg)
                            .child(if line.is_empty() {
                                " ".to_string()
                            } else {
                                line.to_string()
                            })
                    }))
            };
            div()
                .id("artifact-rev-detail")
                .flex_1()
                .min_w_0()
                .h_full()
                .overflow_y_scroll()
                .p(px(16.))
                .flex()
                .flex_col()
                .gap(px(12.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(format!("v{rev} · {}", r.author)),
                        )
                        .when(rev != a.rev, |d| {
                            d.child(
                                secondary_button("artifact-restore", "還原到這一版")
                                    .ml_auto()
                                    .text_size(px(13.))
                                    .px(px(12.))
                                    .py(px(5.))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.restore_revision(rev, cx)
                                    })),
                            )
                        }),
                )
                .child(diff)
        });

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .child(list)
            .children(detail)
            .into_any_element()
    }
}
