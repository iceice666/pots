//! 中欄：project 頻道（頂端狀態、分頁、對話流、輸入框）。

use gpui_kit::assets::IconName;
use gpui_kit::component::input::Textarea;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, Div, DragMoveEvent, ExternalPaths, FileDropEvent, FontWeight, ImageSource,
    InteractiveElement as _, IntoElement, ParentElement as _, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled as _, Window, div, img, px, relative,
};

use super::composer_attach::{PendingSnapshot, UploadState};
use super::stream::Density;
use super::widgets::*;
use crate::app::{Panel, PotsApp};
use crate::model::*;
use crate::theme;

/// 輸入框容器在測試與 debug 查詢用的 id（也是拖放目標）。
const DROP_TARGET_ID: &str = "composer-drop";

pub fn empty_state(title: impl Into<SharedString>, body: impl Into<SharedString>) -> Div {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .p(px(40.))
        .text_color(theme::text_muted())
        .child(
            div()
                .text_size(px(15.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::text())
                .child(title.into()),
        )
        .child(
            div()
                .max_w(px(420.))
                .text_size(px(13.))
                .line_height(relative(1.5))
                .child(body.into()),
        )
}

/// 待送圖片列：縮圖、上傳中／失敗狀態、移除鈕。id 都帶 project 與 key 的 scope，
/// 同一份圖片在別的地方顯示時不會共用 keyed state（見 Project Notes 的 markdown 慣例）。
fn render_pending(pending: &PendingSnapshot, cx: &mut Context<PotsApp>) -> Div {
    /// 縮圖邊長（含移除鈕的內距）。
    const THUMB: f32 = 64.;
    let items = pending
        .items
        .iter()
        .map(|image| {
            let key = image.key;
            let mut thumb = div()
                .id(SharedString::from(format!("composer-attach-{key}")))
                .relative()
                .flex_none()
                .size(px(THUMB))
                .rounded(px(8.))
                .overflow_hidden()
                .border_1()
                .border_color(match &image.state {
                    UploadState::Failed(_) => theme::danger(),
                    UploadState::Uploading => theme::border_strong(),
                    UploadState::Uploaded(_) => theme::border(),
                })
                .bg(theme::subtle_bg())
                .child(img(ImageSource::Image(image.preview.clone())).size_full());
            // 上傳中／失敗：蓋一層半透明底色與圖示，滑過看得到原因。
            match &image.state {
                UploadState::Uploaded(_) => {}
                UploadState::Uploading => {
                    thumb = thumb.child(
                        div()
                            .absolute()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(gpui_kit::black().opacity(0.35))
                            .child(icon(IconName::LoaderCircle, 20., theme::white())),
                    );
                }
                UploadState::Failed(_) => {
                    thumb = thumb.child(
                        div()
                            .absolute()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(theme::danger_soft().opacity(0.85))
                            .child(icon(IconName::CircleX, 20., theme::danger())),
                    );
                }
            }
            let tooltip = image
                .failure()
                .map(str::to_string)
                .or_else(|| image.filename.clone())
                .unwrap_or_else(|| format!("{} KiB", image.bytes.div_ceil(1024)));
            div().relative().flex_none().child(thumb).child(
                div()
                    .id(SharedString::from(format!("composer-attach-remove-{key}")))
                    .absolute()
                    .top(px(-6.))
                    .right(px(-6.))
                    .size(px(18.))
                    .rounded_full()
                    .bg(theme::white())
                    .border_1()
                    .border_color(theme::border_strong())
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::track()))
                    .child(icon(IconName::X, 11., theme::text_muted()))
                    .tip(tooltip)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.pending.update(cx, |p, cx| {
                            p.remove(key);
                            cx.notify();
                        });
                    })),
            )
        })
        .collect::<Vec<_>>();

    let mut strip = div()
        .flex()
        .flex_wrap()
        .items_end()
        .gap(px(10.))
        .px(px(10.))
        .pb(px(8.))
        .children(items);
    if pending.uploading > 0 {
        strip = strip.child(
            div()
                .text_size(px(12.))
                .text_color(theme::text_muted())
                .child(format!("{} 張上傳中…", pending.uploading)),
        );
    }
    if let Some(error) = &pending.error {
        strip = strip.child(
            div()
                .text_size(px(12.))
                .text_color(theme::danger())
                .child(error.clone()),
        );
    }
    strip
}

/// 標題列右側的圖示按鈕（分頁切換與「成員」）：選取時是淡色底與強調色圖示，文字放在 tooltip。
fn header_icon_button(
    id: &'static str,
    icon_name: IconName,
    selected: bool,
    tip: impl Into<SharedString>,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .size(px(32.))
        .rounded(px(6.))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .when_else(
            selected,
            |d| d.bg(theme::accent_soft()),
            |d| d.hover(|s| s.bg(theme::track())),
        )
        .child(icon(
            icon_name,
            18.,
            if selected {
                theme::accent()
            } else {
                theme::text_muted()
            },
        ))
        .tip(tip)
}

/// 輸入框底列的「選擇檔案」按鈕：紙夾圖示，滑過顯示說明。
fn icon_pick_button(id: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .size(px(28.))
        .rounded(px(6.))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .hover(|s| s.bg(theme::track()))
        .child(icon(IconName::ImagePlus, 17., theme::text_muted()))
}

impl PotsApp {
    pub fn render_channel(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(p) = self.current_project() else {
            return div().flex_1().into_any_element();
        };
        let busy = p.agent_state.is_busy();

        let counted = |label: &str, n: usize| {
            if n == 0 {
                label.to_string()
            } else {
                format!("{label} {n}")
            }
        };
        let inst = self.current_instance();
        // 右欄切換：成員、Goals、Artifacts、自動化、記憶、專案設定都在右欄開，和「成員」同樣的圖示按鈕。
        let panel_buttons = div()
            .flex()
            .items_center()
            .gap(px(4.))
            .child(self.panel_button(
                "toggle-goals",
                IconName::Flag,
                counted("Goals", p.goals.len()),
                Panel::Goals,
                cx,
            ))
            .child(self.panel_button(
                "toggle-artifacts",
                IconName::FileText,
                counted("Artifacts", p.artifacts.len()),
                Panel::Artifacts,
                cx,
            ))
            .child(self.panel_button(
                "toggle-automations",
                IconName::CalendarClock,
                counted("自動化", inst.automation_count(p.id)),
                Panel::Automations,
                cx,
            ))
            .child(self.panel_button(
                "toggle-memory",
                IconName::Brain,
                counted("記憶", p.memory.observations.len()),
                Panel::Memory,
                cx,
            ))
            .child(self.panel_button(
                "toggle-members",
                IconName::Users,
                "成員列表".to_string(),
                Panel::Members,
                cx,
            ))
            .child(self.panel_button(
                "toggle-project-settings",
                IconName::Settings2,
                "專案設定".to_string(),
                Panel::ProjectSettings,
                cx,
            ));

        let header = div()
            .px(px(24.))
            .py(px(12.))
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_weight(FontWeight::BOLD)
                            .child(format!("# {}", p.name)),
                    )
                    .child(agent_state_pill(p.agent_state))
                    .child(
                        div()
                            .ml_auto()
                            .text_size(px(13.))
                            .text_color(theme::text_muted())
                            // 還沒有預算的實例（M6 之前）不顯示。
                            .when(p.budget.limit_cents > 0, |d| d.child(p.budget.label())),
                    )
                    .when(busy, |d| {
                        d.child(
                            div()
                                .id("stop-agent")
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .px(px(12.))
                                .py(px(6.))
                                .rounded(px(6.))
                                .border_1()
                                .border_color(theme::border_strong())
                                .text_size(px(13.))
                                .cursor_pointer()
                                .hover(|s| s.bg(theme::subtle_bg()))
                                .child(icon(IconName::Square, 12., theme::text()))
                                .child("停止")
                                .on_click(cx.listener(|this, _, _, cx| this.stop_agent(cx))),
                        )
                    })
                    .child(panel_buttons),
            );

        let body = div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                // onboarding 沒做完：worker 不會派工，與其等 agent 報錯，不如在這裡說清楚。
                .when(p.workdir.is_none(), |d| {
                    let project = p.id;
                    d.child(
                        div()
                            .mx(px(24.))
                            .mt(px(12.))
                            .p(px(10.))
                            .rounded(px(8.))
                            .border_1()
                            .border_color(theme::warn_border())
                            .bg(theme::warn_soft())
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .text_size(px(13.))
                            .child(icon(IconName::Info, 14., theme::warn_text()))
                            .child(
                                div()
                                    .flex_1()
                                    .text_color(theme::warn_text())
                                    .child(
                                        "這個 project 還沒有工作資料夾：worker 不會派工。\
                                         告訴 agent「我們用這個 repo」或指定本機資料夾，也可以直接設定。",
                                    ),
                            )
                            .child(
                                link_button("workdir-set", "設定").on_click(
                                    cx.listener(move |this, _, window, cx| {
                                        this.edit_workdir(project, window, cx)
                                    }),
                                ),
                            ),
                    )
                })
                .child(self.render_project_stream(p, cx))
                .child(self.render_composer(p, window, cx));

        div()
            .flex_1()
            .min_w(px(420.))
            .h_full()
            .flex()
            .flex_col()
            .bg(theme::white())
            .child(header)
            .child(body)
            .into_any_element()
    }

    /// 標題列的右欄切換按鈕：那個列表開著時是選取狀態，再按一次關掉。
    fn panel_button(
        &self,
        id: &'static str,
        icon_name: IconName,
        label: String,
        panel: Panel,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let open = self.panel == Some(panel);
        header_icon_button(id, icon_name, open, label)
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_panel(panel, cx)))
    }

    fn render_project_stream(&self, p: &ProjectView, cx: &mut Context<Self>) -> impl IntoElement {
        let items: Vec<AnyElement> = if p.items.is_empty() {
            vec![
                empty_state(
                    format!("這是 # {} 的開頭", p.name),
                    "每個 project 只有一條對話。討論收斂時 agent 會開一張 Goal 卡片，細節掛在卡片的 thread 下。",
                )
                .into_any_element(),
            ]
        } else {
            p.items
                .iter()
                .map(|item| self.render_stream_item(item, Density::Regular, cx))
                .collect()
        };
        div()
            .id("project-stream")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.stream_scroll)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .pt(px(16.))
                    .pb(px(8.))
                    .children(items),
            )
    }

    fn render_composer(
        &self,
        p: &ProjectView,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mode = self.send_mode;
        let pending = self.pending.read(cx).snapshot();
        let segment =
            |id: &'static str, label: &'static str, value: SendMode, cx: &mut Context<Self>| {
                super::widgets::segment(id, label, mode == value).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.send_mode = value;
                        cx.notify();
                    },
                ))
            };
        let hint = if p.agent_state.is_busy() {
            match mode {
                SendMode::Queue => "agent 正在忙，訊息會等本輪結束再處理",
                SendMode::Steer => "會中斷目前的執行，讓 agent 重新規劃",
            }
        } else {
            "/fix 快速修改 · /goal 開 Goal · /schedule 排程"
        };

        // 拖放目標就是輸入框本身：不需要整頁的 drop zone。
        // `on_drop` 的 payload 是平台層塞進 `active_drag` 的 `ExternalPaths`
        // （`gpui-pre` 把 OS 的 `FileDropEvent` 轉過來），所以型別必須是它。
        // 高亮用 `on_drag_move` 判斷游標是否在範圍內，`on_file_drop_exit` 收回。
        let this = cx.entity().downgrade();
        div()
            .id(DROP_TARGET_ID)
            // 測試要拖到「真的在輸入框上面」的位置才能命中 hitbox。
            .debug_selector(|| DROP_TARGET_ID.to_string())
            .mx(px(24.))
            .mb(px(20.))
            .rounded(px(10.))
            .border_1()
            .border_color(if self.drop_hot {
                theme::accent()
            } else {
                theme::input_border()
            })
            .when(self.drop_hot, |d| d.bg(theme::accent_soft()))
            .flex()
            .flex_col()
            .can_drop(|payload, _, _| payload.is::<ExternalPaths>())
            .on_drag_move(move |event: &DragMoveEvent<ExternalPaths>, _, cx| {
                let hot = event.bounds.contains(&event.event.position);
                this.update(cx, |app, cx| app.set_drop_hot(hot, cx)).ok();
            })
            .on_file_drop_exit({
                let this = cx.entity().downgrade();
                move |_: &FileDropEvent, _, cx| {
                    this.update(cx, |app, cx| app.set_drop_hot(false, cx)).ok();
                }
            })
            .on_drop({
                let this = cx.entity().downgrade();
                move |paths: &ExternalPaths, window, cx| {
                    let paths = paths.paths().to_vec();
                    this.update(cx, |app, cx| app.accept_dropped(paths, window, cx))
                        .ok();
                }
            })
            .child(
                div()
                    .px(px(4.))
                    .pt(px(6.))
                    .text_size(px(15.))
                    .child(self.render_composer_input(cx)),
            )
            .when(!pending.items.is_empty() || pending.error.is_some(), |d| {
                d.child(render_pending(&pending, cx))
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(10.))
                    .py(px(8.))
                    .child(
                        segment_group()
                            .child(segment("mode-queue", "排隊", SendMode::Queue, cx))
                            .child(segment("mode-steer", "改方向", SendMode::Steer, cx)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(13.))
                            .text_color(theme::text_muted())
                            .truncate()
                            .child(hint),
                    )
                    // 選檔走系統對話框（gpui `prompt_for_paths`），選到的路徑跟拖放
                    // 共用同一條管線。`window` 要傳進去是因為對話框是非同步回覆。
                    .child(
                        icon_pick_button("composer-pick")
                            .tip("選擇圖片檔（也可以直接拖放或貼上）")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.pick_images(window, cx)
                            })),
                    )
                    .child(send_button("send", 36.).on_click(
                        cx.listener(|this, _, window, cx| this.send_composer(window, cx)),
                    )),
            )
    }

    /// 輸入框本體。貼上由 `Textarea::on_paste` 攔截：剪貼簿裡有圖片就收進待送附件並
    /// 吃掉這次貼上（回 `true`），否則回 `false` 讓一般文字照常插入。
    fn render_composer_input(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        Textarea::new(&self.composer)
            .appearance(false)
            .on_paste(move |item, _window, cx| {
                // hook 拿到的是 `&mut App`，要轉回 view 的 context。
                this.update(cx, |app, cx| app.accept_pasted(item, cx))
                    .unwrap_or(false)
            })
    }

    /// 右欄「Goals」：這個 project 的 Goal 卡片，點了右欄換成那個 Goal。
    pub(super) fn render_goal_list(&self, p: &ProjectView, cx: &mut Context<Self>) -> AnyElement {
        if p.goals.is_empty() {
            return empty_state(
                "沒有進行中的 Goal",
                "在對話裡描述你想做的事，討論收斂時 agent 會開 Goal。",
            )
            .into_any_element();
        }
        div()
            .id("goal-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p(px(16.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .children(
                p.goals
                    .iter()
                    .map(|g| self.goal_card("list", g, false, cx).into_any_element()),
            )
            .into_any_element()
    }
}
