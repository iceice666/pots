//! 右欄「專案設定」：這個 project 的負責人、工作資料夾、監看的 repo、核准後 merge 的方式，
//! 以及 worker 的環境（`project_env.rs`）。
//!
//! 負責人與監看的 repo 在這裡內聯編輯（Enter 儲存），送 `RenameAgent`、`SetProjectRepos`，
//! 服務以 `ProjectAgentName`、`ProjectRepos` 事件回來時收起表單；工作資料夾開 onboarding 的
//! 對話框。

use gpui_kit::component::input::{Input, InputEvent};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, Div, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};

use super::automation_forms::{chip, error_note, input, list, small_button, value};
use super::panel::white_box;
use super::widgets::*;
use crate::app::{PotsApp, ProjectField, RepoEditor};
use crate::model::*;
use crate::theme;

impl PotsApp {
    /// 右欄「專案設定」：負責人與監看設定在上，環境在下。
    pub(crate) fn render_project_settings(
        &self,
        p: &ProjectView,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id("project-settings")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(16.))
            .py(px(16.))
            .flex()
            .flex_col()
            .gap(px(24.))
            .child(self.render_project_watch(p, cx))
            .child(self.render_project_env(p, cx))
            .into_any_element()
    }

    // ---- 負責人與監看的 repo ------------------------------------------------------------

    pub fn edit_repos(&mut self, project: ProjectId, window: &mut Window, cx: &mut Context<Self>) {
        let current = self
            .store
            .project(self.instance, project)
            .map(|p| {
                p.repos
                    .iter()
                    .map(|r| r.strip_prefix("github.com/").unwrap_or(r).to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let state = input(
            current,
            "owner/repo，逗號分開；Enter 儲存",
            false,
            window,
            cx,
        );
        state.update(cx, |s, cx| s.focus(window, cx));
        let sub = cx.subscribe(&state, |this, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) {
                this.save_repos(cx);
            }
        });
        self.repo_editor = Some(RepoEditor::new(project, ProjectField::Repos, state, sub));
        cx.notify();
    }

    pub fn edit_agent_name(
        &mut self,
        project: ProjectId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self
            .store
            .project(self.instance, project)
            .map(|p| p.agent_name.clone())
            .unwrap_or_default();
        let state = input(current, "負責人的名字；Enter 儲存", false, window, cx);
        state.update(cx, |s, cx| s.focus(window, cx));
        let sub = cx.subscribe(&state, |this, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) {
                this.save_repos(cx);
            }
        });
        self.repo_editor = Some(RepoEditor::new(
            project,
            ProjectField::AgentName,
            state,
            sub,
        ));
        cx.notify();
    }

    pub fn cancel_repos(&mut self, cx: &mut Context<Self>) {
        self.repo_editor = None;
        cx.notify();
    }

    pub fn save_repos(&mut self, cx: &mut Context<Self>) {
        let Some(r) = &self.repo_editor else {
            return;
        };
        let project = r.project;
        let field = r.field;
        let text = value(&r.input, cx);
        if self.live.is_none() {
            self.notify_unimplemented(
                match field {
                    ProjectField::Repos => "設定 repo",
                    ProjectField::AgentName => "幫負責人改名",
                },
                cx,
            );
            return;
        }
        let instance = self.instance;
        match field {
            ProjectField::Repos => self.store.set_project_repos(instance, project, list(&text)),
            ProjectField::AgentName => self.store.rename_agent(instance, project, text),
        }
        if let Some(r) = &mut self.repo_editor {
            r.saving = true;
            r.error = None;
        }
        cx.notify();
    }

    /// 「專案設定」最上面：負責人、工作資料夾、監看的 repo 與 merge 方式。
    pub(crate) fn render_project_watch(&self, p: &ProjectView, cx: &mut Context<Self>) -> Div {
        let project = p.id;
        let editing = self
            .repo_editor
            .as_ref()
            .filter(|r| r.project == project && r.field == ProjectField::Repos);
        let renaming = self
            .repo_editor
            .as_ref()
            .filter(|r| r.project == project && r.field == ProjectField::AgentName);
        let local = matches!(p.workdir, Some(WorkDir::Local { .. }));
        let mut b = white_box()
            .gap(px(8.))
            // 這個 project 的負責人（對話 agent）。
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .flex_wrap()
                    .child(section_label("負責人").flex_1())
                    .when(renaming.is_none(), |d| {
                        d.child(tag(p.agent_name.clone(), theme::text(), theme::subtle_bg()))
                            .child(
                                link_button("agent-rename", "改名")
                                    .text_size(px(13.))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.edit_agent_name(project, window, cx)
                                    })),
                            )
                    }),
            )
            .when_some(renaming, |b, r| {
                b.child(Input::new(&r.input))
                    .when_some(r.error.as_deref(), |d, m| d.child(error_note(m)))
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(
                                small_button(primary_button(
                                    "agent-rename-save",
                                    if r.saving { "儲存中…" } else { "儲存" },
                                ))
                                .on_click(cx.listener(|this, _, _, cx| this.save_repos(cx))),
                            )
                            .child(
                                link_button("agent-rename-cancel", "取消")
                                    .on_click(cx.listener(|this, _, _, cx| this.cancel_repos(cx))),
                            ),
                    )
            })
            .child(div().h(px(1.)).bg(theme::border()))
            // 工作資料夾是「worker 在哪裡改」，和下面的「監看的 repo」（事件來源）分開。
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .flex_wrap()
                    .child(section_label("工作資料夾").flex_1())
                    .child(match &p.workdir {
                        Some(dir) => tag(
                            dir.label(),
                            theme::text(),
                            theme::subtle_bg(),
                        )
                        .into_any_element(),
                        None => div()
                            .text_size(px(13.))
                            .text_color(theme::warn_text())
                            .child("還沒有設定：worker 不會派工")
                            .into_any_element(),
                    })
                    .child(
                        link_button("workdir-edit", "設定")
                            .text_size(px(13.))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.edit_workdir(project, window, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme::text_muted())
                    .child(if local {
                        "本機資料夾：worker 在任務分支上改，核准後平台把分支的 commit 線性接到你的分支上；沒有 PR 與 CI。"
                    } else {
                        "worker 在任務分支上改、開 PR、等 CI，核准後 merge。"
                    }),
            )
            .child(div().h(px(1.)).bg(theme::border()))
            .child(section_label("監看的 repo"));
        match editing {
            Some(r) => {
                b = b
                    .child(Input::new(&r.input))
                    .when_some(r.error.as_deref(), |d, m| d.child(error_note(m)))
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(
                                small_button(primary_button(
                                    "repos-save",
                                    if r.saving { "儲存中…" } else { "儲存" },
                                ))
                                .on_click(cx.listener(|this, _, _, cx| this.save_repos(cx))),
                            )
                            .child(
                                link_button("repos-cancel", "取消")
                                    .on_click(cx.listener(|this, _, _, cx| this.cancel_repos(cx))),
                            ),
                    );
            }
            None => {
                b = b.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .flex_wrap()
                        .children(p.repos.iter().map(|r| {
                            tag(r.clone(), theme::text(), theme::subtle_bg())
                        }))
                        .when(p.repos.is_empty(), |d| {
                            d.child(
                                div()
                                    .w_full()
                                    .text_size(px(13.))
                                    .text_color(theme::text_muted())
                                    .child("還沒有。加上 repo 後，agent 可以等它的 CI 與 PR，listener 也收得到它的事件。"),
                            )
                        })
                        .child(
                            link_button("repos-edit", "編輯")
                                .text_size(px(13.))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.edit_repos(project, window, cx)
                                })),
                        ),
                );
            }
        }
        // 你核准 merge 後平台用的方式（agent 申請時可以另外指定，核准卡片上看得到）。
        // 本機資料夾一律線性套用，沒有 merge 方式可選。
        if local {
            return b;
        }
        b.child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .flex_wrap()
                .pt(px(4.))
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme::text_muted())
                        .child("核准後 merge 的方式"),
                )
                .children(MergeMethod::ALL.map(|m| {
                    chip(
                        SharedString::from(format!("merge-method-{}", m.as_str())),
                        m.label(),
                        p.merge_method == m,
                    )
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.set_merge_method(project, m, cx)),
                    )
                })),
        )
    }

    pub fn set_merge_method(
        &mut self,
        project: ProjectId,
        method: MergeMethod,
        cx: &mut Context<Self>,
    ) {
        let instance = self.instance;
        if let Some(p) = self.store.project_mut(instance, project) {
            p.merge_method = method;
        }
        self.store
            .outbox
            .push((instance, ClientCommand::SetMergeMethod { project, method }));
        cx.notify();
    }
}
