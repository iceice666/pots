//! Throwaway GPUI prototype for evaluating WYSIWYG Markdown and IME behavior.
//! Edits stay in memory; nothing is written back to disk.

use gpui_kit::component::{ActiveTheme as _, button::Button, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Entity, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, ScrollHandle, SharedString, StatefulInteractiveElement as _,
    Styled, Subscription, Window, div, px, rgb,
};
use markdown_editor::{
    BlockEditor, BlockEditorEvent, BlockEditorState, BlockKind, EditorCommand, MarkKind,
};

const SAMPLE: &str = include_str!("../sample.md");

struct EditorLab {
    editor: Entity<BlockEditorState>,
    scroll: ScrollHandle,
    initial: String,
    /// The editor's canonical serialization of `initial`, used for change detection.
    loaded: SharedString,
    source: SharedString,
    revision: u64,
    _subscriptions: Vec<Subscription>,
}

impl EditorLab {
    fn new(initial: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let scroll = ScrollHandle::new();
        let editor = cx.new(|cx| {
            let mut state = BlockEditorState::new(window, cx).with_source(&initial);
            state.set_scroll_handle(scroll.clone());
            state
        });
        let subscriptions = vec![
            cx.subscribe(&editor, |this, editor, event: &BlockEditorEvent, cx| {
                if let BlockEditorEvent::Changed { revision, .. } = event {
                    this.revision = *revision;
                    this.source = editor.read(cx).source().into();
                }
                cx.notify();
            }),
            // Composition and caret changes notify the editor; mirror them in the status bar.
            cx.observe(&editor, |_, _, cx| cx.notify()),
        ];
        editor.update(cx, |e, cx| e.focus(window, cx));
        let source: SharedString = editor.read(cx).source().into();
        Self {
            editor,
            scroll,
            initial,
            loaded: source.clone(),
            source,
            revision: 0,
            _subscriptions: subscriptions,
        }
    }

    fn command(&mut self, command: EditorCommand, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |e, cx| {
            let _ = e.run(command, cx);
            e.focus(window, cx);
        });
    }

    fn command_button(
        id: &'static str,
        label: &'static str,
        command: EditorCommand,
        cx: &mut Context<Self>,
    ) -> Button {
        Button::new(id).label(label).on_click(
            cx.listener(move |this, _, window, cx| this.command(command.clone(), window, cx)),
        )
    }

    fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let initial = self.initial.clone();
        self.editor.update(cx, |e, cx| {
            let _ = e.set_source(&initial, cx);
            e.focus(window, cx);
        });
        self.source = self.editor.read(cx).source().into();
        self.loaded = self.source.clone();
        self.revision = 0;
        cx.notify();
    }
}

fn block_label(kind: Option<BlockKind>) -> String {
    match kind {
        Some(BlockKind::Heading { level }) => format!("標題 {level}"),
        Some(BlockKind::Paragraph) => "段落".into(),
        Some(other) => format!("{other:?}"),
        None => "—".into(),
    }
}

impl Render for EditorLab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editor = self.editor.read(cx);
        let composing = editor.is_composing();
        let caret = block_label(editor.block_kind_at_caret());
        let dirty = self.source != self.loaded;
        let mono = cx.theme().mono_font_family.clone();

        let toolbar = h_flex()
            .gap_1()
            .flex_wrap()
            .child(Self::command_button(
                "bold",
                "B",
                EditorCommand::ToggleMark(MarkKind::Bold),
                cx,
            ))
            .child(Self::command_button(
                "italic",
                "I",
                EditorCommand::ToggleMark(MarkKind::Italic),
                cx,
            ))
            .child(Self::command_button(
                "strike",
                "刪除線",
                EditorCommand::ToggleMark(MarkKind::Strikethrough),
                cx,
            ))
            .child(Self::command_button(
                "code",
                "行內程式碼",
                EditorCommand::ToggleMark(MarkKind::Code),
                cx,
            ))
            .child(div().w(px(1.)).h(px(20.)).mx_1().bg(rgb(0xdde3ec)))
            .child(Self::command_button(
                "p",
                "內文",
                EditorCommand::SetBlockKind(BlockKind::Paragraph),
                cx,
            ))
            .child(Self::command_button(
                "h1",
                "H1",
                EditorCommand::SetBlockKind(BlockKind::Heading { level: 1 }),
                cx,
            ))
            .child(Self::command_button(
                "h2",
                "H2",
                EditorCommand::SetBlockKind(BlockKind::Heading { level: 2 }),
                cx,
            ))
            .child(Self::command_button(
                "ul",
                "清單",
                EditorCommand::ToggleList { ordered: false },
                cx,
            ))
            .child(Self::command_button(
                "ol",
                "編號",
                EditorCommand::ToggleList { ordered: true },
                cx,
            ))
            .child(Self::command_button(
                "task",
                "待辦",
                EditorCommand::ToggleTask,
                cx,
            ))
            .child(Self::command_button(
                "table",
                "表格",
                EditorCommand::InsertTable { rows: 3, cols: 3 },
                cx,
            ))
            .child(div().w(px(1.)).h(px(20.)).mx_1().bg(rgb(0xdde3ec)))
            .child(Button::new("undo").label("復原").on_click(cx.listener(
                |this, _, window, cx| {
                    this.editor.update(cx, |e, cx| {
                        e.undo(cx);
                        e.focus(window, cx);
                    });
                },
            )))
            .child(Button::new("redo").label("重做").on_click(cx.listener(
                |this, _, window, cx| {
                    this.editor.update(cx, |e, cx| {
                        e.redo(cx);
                        e.focus(window, cx);
                    });
                },
            )))
            .child(
                Button::new("reset")
                    .label("重新載入")
                    .on_click(cx.listener(|this, _, window, cx| this.reset(window, cx))),
            );

        let editor_pane = div()
            .id("editor-scroll")
            .flex_1()
            .h_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .bg(rgb(0xffffff))
            .child(
                div()
                    .max_w(px(820.))
                    .mx_auto()
                    .px(px(40.))
                    .py(px(28.))
                    .child(BlockEditor::new(&self.editor).min_h_full()),
            );

        let source_pane = v_flex()
            .w(px(380.))
            .h_full()
            .border_l_1()
            .border_color(rgb(0xe3e8f0))
            .bg(rgb(0xf7f9fc))
            .child(
                div()
                    .px_4()
                    .py_3()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(0x6b7689))
                    .child("Markdown 原始碼（唯讀，即時同步）"),
            )
            .child(
                div()
                    .id("source-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .px_4()
                    .pb_4()
                    .font_family(mono)
                    .text_xs()
                    .text_color(rgb(0x3b4558))
                    .whitespace_normal()
                    .child(self.source.clone()),
            );

        v_flex()
            .size_full()
            .bg(rgb(0xf4f6fa))
            .text_color(rgb(0x1f2328))
            .child(
                v_flex()
                    .gap_2()
                    .px_4()
                    .pt_3()
                    .pb_2()
                    .bg(rgb(0xffffff))
                    .border_b_1()
                    .border_color(rgb(0xe3e8f0))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(FontWeight::BOLD)
                                    .child("Pots · GPUI 編輯器測試"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0x7d889a))
                                    .child("gpui-kit-markdown-editor 0.1.3 · 修改不會儲存"),
                            ),
                    )
                    .child(toolbar),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(editor_pane)
                    .child(source_pane),
            )
            .child(
                h_flex()
                    .gap_4()
                    .px_4()
                    .py_1()
                    .text_xs()
                    .text_color(rgb(0x6b7689))
                    .bg(rgb(0xffffff))
                    .border_t_1()
                    .border_color(rgb(0xe3e8f0))
                    .child(format!("版本 {}", self.revision))
                    .child(format!("游標位置：{caret}"))
                    .child(
                        div()
                            .when(composing, |d| {
                                d.text_color(rgb(0xb4690e))
                                    .font_weight(FontWeight::SEMIBOLD)
                            })
                            .child(if composing {
                                "● 組字中"
                            } else {
                                "○ 未組字"
                            }),
                    )
                    .child(if dirty {
                        "與載入內容不同"
                    } else {
                        "與載入內容相同"
                    }),
            )
    }
}

fn main() {
    let initial = match std::env::args_os().nth(1) {
        Some(path) => std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("無法讀取 {}: {e}", std::path::Path::new(&path).display())),
        None => SAMPLE.to_string(),
    };
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx: &mut App| {
            gpui_kit::init(cx);
            markdown_editor::init(cx);
            gpui_kit::open_window(gpui_kit::WindowOptions::default(), cx, move |window, cx| {
                cx.new(|cx| EditorLab::new(initial, window, cx))
            })
            .expect("failed to open window");
            cx.activate(true);
        });
}
