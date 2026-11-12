//! Inbox：需要你決定的事。全域 Inbox 合併所有已連線實例；左欄的 Inbox 只看目前實例。

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Context, Div, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px,
};

use super::channel::empty_state;
use super::text::{agent_text, agent_text_muted, agent_text_prefixed, fact_note};
use super::widgets::*;
use crate::app::{InboxFilter, PotsApp};
use crate::model::*;
use crate::theme;

fn kind_tag(kind: InboxKind) -> Div {
    match kind {
        InboxKind::Approval => tag(kind.label(), theme::warn_text(), theme::warn_soft()),
        InboxKind::NeedsYou => tag(kind.label(), theme::danger(), theme::danger_soft()),
        InboxKind::Mention => tag(kind.label(), theme::accent_text(), theme::accent_soft()),
        InboxKind::Question => tag(kind.label(), theme::accent_text(), theme::accent_soft()),
    }
}

fn chip(text: impl Into<gpui_kit::SharedString>) -> Div {
    div()
        .flex_none()
        .px(px(8.))
        .py(px(2.))
        .rounded(px(4.))
        .bg(theme::chip_bg())
        .text_color(theme::chip_text())
        .text_size(px(12.))
        .child(text.into())
}

impl PotsApp {
    pub fn render_inbox(
        &self,
        scope: Option<InstanceId>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let in_scope = |i: &&InboxItem| scope.is_none_or(|id| i.instance == id);
        let count = |kind: Option<InboxKind>| {
            self.store
                .inbox
                .iter()
                .filter(in_scope)
                .filter(|i| i.is_open() && kind.is_none_or(|k| i.kind == k))
                .count()
        };
        let filter = self.inbox_filter;

        let filter_tab =
            |id: &'static str, label: &str, value: InboxFilter, cx: &mut Context<Self>| {
                let n = count(match value {
                    InboxFilter::All => None,
                    InboxFilter::Kind(k) => Some(k),
                });
                tab(id, format!("{label} {n}"), filter == value).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.inbox_filter = value;
                        cx.notify();
                    },
                ))
            };

        let (title, subtitle) = match scope {
            None => (
                "Inbox".to_string(),
                "需要你決定的事，合併所有已連線的實例。一般進度不會出現在這裡。".to_string(),
            ),
            Some(id) => (
                format!(
                    "Inbox · {}",
                    self.store.instance(id).map_or("", |i| i.name.as_str())
                ),
                "只顯示這個實例的待處理事項。實例直列最上方是合併所有實例的 Inbox。".to_string(),
            ),
        };

        let header = div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .px(px(32.))
            .pt(px(20.))
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .text_size(px(20.))
                            .font_weight(FontWeight::BOLD)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_size(px(14.))
                            .text_color(theme::text_muted())
                            .child(subtitle),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap(px(20.))
                    .child(filter_tab("inbox-f-all", "全部", InboxFilter::All, cx))
                    .child(filter_tab(
                        "inbox-f-approval",
                        "核准",
                        InboxFilter::Kind(InboxKind::Approval),
                        cx,
                    ))
                    .child(filter_tab(
                        "inbox-f-question",
                        "問你",
                        InboxFilter::Kind(InboxKind::Question),
                        cx,
                    ))
                    .child(filter_tab(
                        "inbox-f-needs",
                        "需要你處理",
                        InboxFilter::Kind(InboxKind::NeedsYou),
                        cx,
                    ))
                    .child(filter_tab(
                        "inbox-f-mention",
                        "找你的訊息",
                        InboxFilter::Kind(InboxKind::Mention),
                        cx,
                    )),
            );

        // 未處理的依時間新到舊排在前面，已處理的沉到底部。
        let mut items: Vec<&InboxItem> = self
            .store
            .inbox
            .iter()
            .filter(in_scope)
            .filter(|i| match filter {
                InboxFilter::All => true,
                InboxFilter::Kind(k) => i.kind == k,
            })
            .collect();
        items.sort_by_key(|i| (!i.is_open(), std::cmp::Reverse(i.at)));

        let show_instance = scope.is_none() && self.store.instances.len() > 1;
        let list = if items.is_empty() {
            empty_state(
                "都處理完了",
                "有新的核准或需要你決定的事時，會出現在這裡並推播通知。",
            )
            .into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .children(items.into_iter().map(|i| {
                    self.render_inbox_item(i, show_instance, cx)
                        .into_any_element()
                }))
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
            .child(
                div()
                    .id("inbox-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(32.))
                    .py(px(20.))
                    .child(div().max_w(px(760.)).child(list)),
            )
    }

    /// 核准理由以外的 facts（PR、sha、動作、時間）：平台組的結構化欄位，維持 chip；
    /// 只有拒絕／回答時附上的「你的理由：」後面那段是使用者原文，拆出來走 Markdown。
    fn render_facts(&self, item: &InboxItem, scope: &str, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .children(item.facts.iter().enumerate().map(|(i, f)| {
                match fact_note(f) {
                    Some((prefix, note)) => agent_text_prefixed(
                        prefix,
                        agent_text_muted(
                            gpui_kit::SharedString::from(format!("{scope}-fact-{}-{i}", item.id.0)),
                            note,
                        ),
                    )
                    .into_any_element(),
                    None => div()
                        .flex()
                        .flex_wrap()
                        .gap(px(6.))
                        .child(chip(f.clone()))
                        .into_any_element(),
                }
            }))
            .when_some(super::preferences::approval_diff(item), |d, diff| {
                d.child(super::preferences::diff_block(
                    diff,
                    cx.theme().mono_font_family.clone(),
                ))
            })
    }

    fn render_inbox_item(
        &self,
        item: &InboxItem,
        show_instance: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let open = item.is_open();
        let instance = self.store.instance(item.instance);
        let source = match item.project {
            Some(pid) => instance
                .and_then(|i| i.projects.iter().find(|p| p.id == pid))
                .map_or("# ?".to_string(), |p| format!("# {}", p.name)),
            // 設定 provider／登入那筆屬於整個實例，不是總管發的。
            None if item.id == crate::sync::model_login_item_id(item.instance) => {
                "模型 provider".to_string()
            }
            None => "總管".to_string(),
        };

        let top = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.))
            .text_size(px(13.))
            .child(kind_tag(item.kind))
            .when(show_instance, |d| {
                d.child(chip(instance.map_or("?", |i| i.name.as_str()).to_string()))
            })
            .child(div().font_weight(FontWeight::SEMIBOLD).child(source))
            // context 是 Goal 標題或專案名（使用者／agent 取的），可能帶行內 Markdown；
            // 「·」是平台的分隔字，留在純文字。
            .when(!item.context.is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .text_color(theme::text_muted())
                        .child("·")
                        .child(
                            agent_text(
                                gpui_kit::SharedString::from(format!(
                                    "inbox-context-{}",
                                    item.id.0
                                )),
                                item.context.clone(),
                            )
                            .text_size(px(13.)),
                        ),
                )
            })
            .child(
                div()
                    .ml_auto()
                    .text_size(px(12.))
                    .text_color(theme::text_muted())
                    .child(item.at.relative_to(self.store.now)),
            );

        // 沒有 detail 的項目（例如 listener 提案）直接列 facts；有 detail 就畫理由。
        let facts_only = item.detail.is_empty() && !item.facts.is_empty();
        let body = if facts_only {
            self.render_facts(item, "inbox", cx).into_any_element()
        } else {
            // 核准理由、任務「需要你」的原因都是 agent 寫的 Markdown。
            div()
                .text_color(theme::text_muted())
                .child(
                    agent_text(
                        gpui_kit::SharedString::from(format!("inbox-detail-{}", item.id.0)),
                        item.detail.clone(),
                    )
                    .text_size(px(14.)),
                )
                .into_any_element()
        };
        // 服務的核准：理由之外也列出具體的動作（PR、sha、方式）與修改的 diff。
        let body = if item.approval.is_some() && !facts_only {
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(body)
                .child(self.render_facts(item, "inbox", cx))
                .into_any_element()
        } else {
            body
        };

        let footer = if open {
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .children(self.render_question_options(item, "inbox", cx))
                .child(self.render_actions(item.id, &item.actions, "inbox", cx))
                .into_any_element()
        } else {
            let label = match &item.decision {
                Decision::Approved => "已核准".to_string(),
                Decision::Rejected => "已拒絕".to_string(),
                Decision::Resolved(s) => s.clone(),
                Decision::Pending => unreachable!(),
            };
            div()
                .text_size(px(13.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::text_muted())
                .child(label)
                .into_any_element()
        };

        div()
            .id(gpui_kit::SharedString::from(format!(
                "inbox-item-{}",
                item.id.0
            )))
            .flex()
            .flex_col()
            .gap(px(8.))
            .p(px(16.))
            .rounded(px(10.))
            .border_1()
            .border_color(theme::border())
            .bg(theme::white())
            .when(!open, |d| d.opacity(0.6))
            .child(top)
            .child(
                div()
                    .text_size(px(16.))
                    .font_weight(FontWeight::BOLD)
                    .child(item.title.clone()),
            )
            .child(body)
            .child(div().pt(px(4.)).child(footer))
    }
}
