//! Artifacts：頻道「Artifacts」分頁的列表，與右欄的 Markdown 編輯器。
//!
//! 編輯器逐 `##` 區段各開一個 `gpui-kit-markdown-editor`：你改過的區段與 agent 正在寫入的
//! 區段各自標示出來，agent 正在寫的那段唯讀，其他區段可以照常編輯。

use gpui_kit::assets::IconName;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, Div, FontWeight, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, div, px,
    relative,
};
use markdown_editor::BlockEditor;

use super::channel::empty_state;
use super::widgets::*;
use crate::app::{ArtifactEditor, Panel, PotsApp};
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
    /// 頻道的「Artifacts」分頁：這個 project 的所有 artifact。
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
            .px(px(24.))
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
    ) -> impl IntoElement {
        let subtitle = if a.agent_writing.is_some() {
            format!("v{} · 你與 agent 共同編輯", a.rev)
        } else {
            format!("v{}", a.rev)
        };

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
                    .child(
                        secondary_button("artifact-history", "版本紀錄")
                            .text_size(px(13.))
                            .px(px(12.))
                            .py(px(5.))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.notify_unimplemented("版本紀錄", cx)
                            })),
                    )
                    .child(
                        secondary_button("artifact-diff", "比較差異")
                            .text_size(px(13.))
                            .px(px(12.))
                            .py(px(5.))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.notify_unimplemented("比較差異", cx)
                            })),
                    )
                    .child(self.close_button(cx)),
            );

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
                    .children(writing)
                    .children(proposals)
                    .children(sections),
            )
    }
}
