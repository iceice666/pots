//! M6a 的整合測試：階層預算、熔斷通知、trace 的「為什麼」、你不在期間與總管。
//!
//! 模型換成依請求內容回應的假 provider：對話 agent 執行使用者訊息裡的 `CALL` 行；
//! Planner 第一輪呼叫 `get_status` 並回報用量，第二輪才回答；總管第一輪就派工。

mod common;

use std::path::Path;
use std::sync::Arc;

use common::{Client, ScriptedApi, TestServer};
use pots_llm::testing::{FnProvider, ScriptedTurn};
use pots_llm::{Item, ModelRequest};
use pots_proto::{
    AgentRole, ApprovalActionView, ApprovalState, Author, Card, ClientCommand, GoalState,
    ModelAuthView, ProjectId, ProviderId, SendMode, ServerEvent, StreamItemKind, StreamScope,
    Timestamp,
};
use pots_server::{Config, Deps, FakeModelAuth, ManualClock};
use serde_json::json;

const PLANNER: &str = "你是 Pots 的 Planner";
const GLOBAL: &str = "你是 Pots 的總管";
const MODEL: &str = "test-model";

fn is_planner(req: &ModelRequest) -> bool {
    req.instructions
        .as_deref()
        .is_some_and(|i| i.starts_with(PLANNER))
}

fn is_global(req: &ModelRequest) -> bool {
    req.instructions
        .as_deref()
        .is_some_and(|i| i.starts_with(GLOBAL))
}

fn tool_results(req: &ModelRequest) -> usize {
    req.input
        .iter()
        .filter(|i| matches!(i, Item::ToolResult { .. }))
        .count()
}

/// 對話 agent：最後一則使用者訊息裡的 `CALL` 行依序各呼叫一次。
fn chat_turn(req: &ModelRequest) -> ScriptedTurn {
    let Some(last_user) = req
        .input
        .iter()
        .rposition(|i| matches!(i, Item::User { .. }))
    else {
        return ScriptedTurn::reply("完成");
    };
    let Item::User { text } = &req.input[last_user] else {
        unreachable!()
    };
    let done = req.input[last_user..]
        .iter()
        .filter(|i| matches!(i, Item::ToolResult { .. }))
        .count();
    let calls: Vec<(&str, &str)> = text
        .lines()
        .filter_map(|l| l.strip_prefix("CALL "))
        .filter_map(|l| l.split_once(' '))
        .collect();
    match calls.get(done) {
        Some((name, args)) => ScriptedTurn::new().tool_call(format!("c{done}"), *name, *args),
        None => ScriptedTurn::reply("完成").usage(10, 5),
    }
}

/// 對話 agent、Planner 與總管共用一個 provider。
fn model() -> Arc<FnProvider> {
    FnProvider::new(MODEL, |req| {
        if is_global(req) {
            // 總管：第一輪把工作派到 demo；之後收尾。
            return if tool_results(req) == 0 {
                ScriptedTurn::new()
                    .tool_call(
                        "g1",
                        "create_goal",
                        json!({
                            "title": "總管派的工",
                            "brief_md": "## 目標\n\n寫一個測試。",
                            "project": "demo",
                        })
                        .to_string(),
                    )
                    .usage(100, 20)
            } else {
                ScriptedTurn::reply("已經交給 demo 了").usage(50, 10)
            };
        }
        if is_planner(req) {
            // Planner：第一輪查狀態並回報用量，第二輪回答。
            return if tool_results(req) == 0 {
                ScriptedTurn::new()
                    .tool_call("p1", "get_status", "{}")
                    .usage(15_000, 12_000)
            } else {
                ScriptedTurn::reply("規劃完成").usage(100, 20)
            };
        }
        chat_turn(req)
    })
}

fn call(name: &str, args: serde_json::Value) -> String {
    format!("CALL {name} {}", serde_json::to_string(&args).unwrap())
}

async fn launch(dir: &Path, clock: &ManualClock) -> TestServer {
    let mut config = Config::desktop(dir).with_default_provider(ProviderId::CHATGPT);
    config.watcher.clock = clock.clock();
    let auth = Arc::new(FakeModelAuth::signed_in());
    let deps = Deps::new(model(), MODEL, auth.clone());
    let handle = pots_server::start(config, deps)
        .await
        .expect("服務啟動失敗");
    TestServer {
        handle,
        provider: pots_llm::testing::ScriptedProvider::new([]),
        api: ScriptedApi::new([]),
        auth,
    }
}

async fn say(c: &mut Client, project: ProjectId, text: &str) -> Vec<ServerEvent> {
    c.say(project, text, SendMode::Queue).await;
    c.until_idle(project).await
}

/// 建立 project 並建立一個預算 20,000 token 的 Goal；回傳這段期間收到的事件。
async fn goal_with_small_budget(c: &mut Client, project: ProjectId) -> Vec<ServerEvent> {
    say(
        c,
        project,
        &call(
            "create_goal",
            json!({
                "title": "小預算",
                "brief_md": "## 目標\n\n把測試寫完。",
                "budget_tokens": 20_000,
            }),
        ),
    )
    .await
}

#[tokio::test]
async fn budget_stop_requests_an_approval_and_continues_after_approval() {
    let tmp = tempfile::tempdir().unwrap();
    let clock = ManualClock::new(Timestamp::now());
    let srv = launch(tmp.path(), &clock).await;
    let mut c = srv.client().await;
    let _ = c.snapshot().await;
    let project = c.create_project("demo").await;

    let mut seen = goal_with_small_budget(&mut c, project).await;

    // Planner 第一輪用掉 27,000 > 20,000，第二輪開始前被預算擋下並申請追加。
    if !seen.iter().any(|e| {
        matches!(
            e,
            ServerEvent::ApprovalChanged { approval, .. }
                if matches!(approval.action, ApprovalActionView::IncreaseBudget { .. })
        )
    }) {
        seen.extend(
            c.until(|e| {
                matches!(
                    e,
                    ServerEvent::ApprovalChanged { approval, .. }
                        if matches!(approval.action, ApprovalActionView::IncreaseBudget { .. })
                )
            })
            .await,
        );
    }
    let approval = seen
        .iter()
        .rev()
        .find_map(|e| match e {
            ServerEvent::ApprovalChanged { approval, .. }
                if matches!(approval.action, ApprovalActionView::IncreaseBudget { .. }) =>
            {
                Some(approval.clone())
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(approval.state, ApprovalState::Pending);
    let ApprovalActionView::IncreaseBudget { tokens, .. } = approval.action else {
        panic!("應該是追加預算");
    };
    // 20,000 的一半，至少 100,000。
    assert_eq!(tokens, 100_000);

    // 核准後 Goal 的預算變大、Planner 繼續跑完。
    c.send(&ClientCommand::Decide {
        approval: approval.id,
        approve: true,
        note: None,
    })
    .await;
    let mut seen = c
        .until(|e| {
            matches!(
                e,
                ServerEvent::ApprovalChanged { approval, .. }
                    if approval.state == ApprovalState::Executed
            )
        })
        .await;
    assert!(!seen.is_empty());
    if !seen
        .iter()
        .any(|e| matches!(e, ServerEvent::GoalChanged { goal, .. } if goal.planner_note.contains("規劃完成")))
    {
        seen.extend(
            c.until(|e| {
                matches!(e, ServerEvent::GoalChanged { goal, .. } if goal.planner_note.contains("規劃完成"))
            })
            .await,
        );
    }
    let goal = seen
        .into_iter()
        .rev()
        .find_map(|e| match e {
            ServerEvent::GoalChanged { goal, .. } => Some(goal),
            _ => None,
        })
        .unwrap();
    assert!(goal.budget_used_pct > 0);
    assert!(matches!(goal.state, GoalState::Active));
    srv.stop().await;
}

#[tokio::test]
async fn why_panel_shows_trigger_memory_and_trace() {
    let tmp = tempfile::tempdir().unwrap();
    let clock = ManualClock::new(Timestamp::now());
    let srv = launch(tmp.path(), &clock).await;
    let mut c = srv.client().await;
    let _ = c.snapshot().await;
    let project = c.create_project("demo").await;

    let events = say(&mut c, project, "你好").await;
    let item = events
        .iter()
        .rev()
        .find_map(|e| match e {
            ServerEvent::Item {
                scope: StreamScope::Project(p),
                item,
                ..
            } if *p == project
                && matches!(item.kind, StreamItemKind::Agent(pots_proto::Msg { .. })) =>
            {
                Some(item.id)
            }
            _ => None,
        })
        .expect("應該有 agent 回覆");

    c.send(&ClientCommand::Why { item }).await;
    let events = c
        .until(|e| matches!(e, ServerEvent::Why { why: Some(_) }))
        .await;
    let why = events
        .iter()
        .rev()
        .find_map(|e| match e {
            ServerEvent::Why { why: Some(why) } => Some(why.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(why.item, item);
    assert_eq!(why.agent, "chat");
    assert_eq!(why.input_tokens, 10);
    assert_eq!(why.output_tokens, 5);
    assert!(why.trigger.contains("使用者訊息"));
    assert!(why.memory_revision.is_some());
    assert!(why.steps.iter().any(|s| s.kind == "model"));
    srv.stop().await;
}

#[tokio::test]
async fn coming_back_after_two_hours_inserts_a_catch_up_card() {
    let tmp = tempfile::tempdir().unwrap();
    let clock = ManualClock::new(Timestamp::now());
    let srv = launch(tmp.path(), &clock).await;
    let mut c = srv.client().await;
    let _ = c.snapshot().await;
    let project = c.create_project("demo").await;

    // 第一次看到：記下 last_read。
    c.send(&ClientCommand::MarkRead { project }).await;
    c.assert_quiet(std::time::Duration::from_millis(100)).await;

    goal_with_small_budget(&mut c, project).await;

    // 離開三小時再回來：期間開了 Goal，插一張摘要卡片。
    clock.advance(3 * 3600);
    c.send(&ClientCommand::MarkRead { project }).await;
    let events = c
        .until(|e| {
            matches!(
                e,
                ServerEvent::Item {
                    item,
                    ..
                } if matches!(item.kind, StreamItemKind::Card(Card::CatchUp { .. }))
            )
        })
        .await;
    let summary = events
        .iter()
        .rev()
        .find_map(|e| match e {
            ServerEvent::Item { item, .. } => match &item.kind {
                StreamItemKind::Card(Card::CatchUp { summary }) => Some(summary.clone()),
                _ => None,
            },
            _ => None,
        })
        .unwrap();
    assert!(summary.contains("Goal"), "摘要：{summary}");
    srv.stop().await;
}

#[tokio::test]
async fn overseer_creates_a_goal_in_a_project() {
    let tmp = tempfile::tempdir().unwrap();
    let clock = ManualClock::new(Timestamp::now());
    let srv = launch(tmp.path(), &clock).await;
    let mut c = srv.client().await;
    let (_, snap) = c.snapshot().await;
    assert!(matches!(snap.model_auth, ModelAuthView::SignedIn { .. }));
    let project = c.create_project("demo").await;

    c.send(&ClientCommand::SendOverseer {
        id: pots_proto::ItemId::new(),
        text: "幫 demo 開一個 Goal".into(),
    })
    .await;

    // 總管的回覆走 Overseer 這條對話流，作者是總管。
    let events = c
        .until(|e| {
            matches!(
                e,
                ServerEvent::Item {
                    scope: StreamScope::Overseer,
                    item,
                    ..
                } if matches!(
                    &item.kind,
                    StreamItemKind::Agent(pots_proto::Msg {
                        author: Author::Agent {
                            role: AgentRole::Overseer,
                            ..
                        },
                        ..
                    })
                )
            )
        })
        .await;
    assert!(!events.is_empty());

    let goal = c
        .until(|e| matches!(e, ServerEvent::GoalChanged { project: p, .. } if *p == project))
        .await
        .into_iter()
        .rev()
        .find_map(|e| match e {
            ServerEvent::GoalChanged { goal, .. } => Some(goal),
            _ => None,
        })
        .expect("總管應該在 demo 開了 Goal");
    assert_eq!(goal.title, "總管派的工");
    srv.stop().await;
}
