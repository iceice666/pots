//! M2 任務樹的整合測試（`proposal/agent-collaboration.md`、`proposal/artifacts.md`）：經由
//! WebSocket 對話，模型換成依請求內容回應的假 provider。
//!
//! - 對話 agent：使用者訊息裡每一行 `CALL <工具> <JSON>` 依序各呼叫一次（一輪一個），
//!   做完回「完成」。
//! - Planner：每個測試自己決定（依觸發訊息與這一輪之前有幾個工具結果）。

mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::{Client, ScriptedApi, TestServer};
use pots_llm::testing::{FnProvider, ScriptedTurn};
use pots_llm::{Item, ModelRequest};
use pots_proto::{
    ArtifactKind, ArtifactView, Card, ClientCommand, GoalState, GoalView, ProjectId, ProviderId,
    SendMode, ServerEvent, StreamItemKind, TaskStateView,
};
use pots_server::{Config, Deps, FakeModelAuth};
use serde_json::{Value, json};

const PLANNER: &str = "你是 Pots 的 Planner";

fn is_planner(req: &ModelRequest) -> bool {
    req.instructions
        .as_deref()
        .is_some_and(|i| i.starts_with(PLANNER))
}

/// 觸發訊息（第一則使用者訊息）與它之後已經有幾個工具結果。
fn planner_turn(req: &ModelRequest) -> (String, usize) {
    let trigger = match req.input.first() {
        Some(Item::User { text }) => text.clone(),
        _ => String::new(),
    };
    let k = req
        .input
        .iter()
        .filter(|i| matches!(i, Item::ToolResult { .. }))
        .count();
    (trigger, k)
}

/// 對話 agent：最後一則使用者訊息裡的 `CALL` 行依序執行。
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
        Some((name, args)) => {
            ScriptedTurn::new().tool_call(format!("call_{}_{done}", req.input.len()), *name, *args)
        }
        None => ScriptedTurn::reply("完成"),
    }
}

fn call(name: &str, args: Value) -> String {
    format!("CALL {name} {}", serde_json::to_string(&args).unwrap())
}

async fn launch(dir: &Path, model: Arc<FnProvider>) -> TestServer {
    let config = Config::desktop(dir).with_default_provider(ProviderId::CHATGPT);
    let auth = Arc::new(FakeModelAuth::signed_in());
    let deps = Deps::new(model, "test-model", auth.clone());
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

/// 收事件直到某個 Goal 滿足 `pred`。
async fn until_goal(c: &mut Client, mut pred: impl FnMut(&GoalView) -> bool) -> GoalView {
    let events = c
        .until(|e| matches!(e, ServerEvent::GoalChanged { goal, .. } if pred(goal)))
        .await;
    match events.last() {
        Some(ServerEvent::GoalChanged { goal, .. }) => goal.clone(),
        _ => unreachable!(),
    }
}

/// 已經收到的事件裡最後一個符合 `pred` 的 Goal；沒有就繼續等。
async fn settle_goal(
    c: &mut Client,
    seen: &[ServerEvent],
    mut pred: impl FnMut(&GoalView) -> bool,
) -> GoalView {
    let found = seen.iter().rev().find_map(|e| match e {
        ServerEvent::GoalChanged { goal, .. } if pred(goal) => Some(goal.clone()),
        _ => None,
    });
    match found {
        Some(g) => g,
        None => until_goal(c, pred).await,
    }
}

async fn until_artifact(
    c: &mut Client,
    mut pred: impl FnMut(&ArtifactView) -> bool,
) -> ArtifactView {
    let events = c
        .until(|e| matches!(e, ServerEvent::ArtifactChanged { artifact, .. } if pred(artifact)))
        .await;
    match events.last() {
        Some(ServerEvent::ArtifactChanged { artifact, .. }) => artifact.clone(),
        _ => unreachable!(),
    }
}

/// 新連線的快照裡的第一個 project。
async fn snapshot_project(srv: &TestServer) -> pots_proto::ProjectSnapshot {
    let mut c = srv.client().await;
    let (_, snap) = c.snapshot().await;
    snap.projects.into_iter().next().expect("沒有 project")
}

fn brief_of(p: &pots_proto::ProjectSnapshot, goal: pots_proto::GoalId) -> ArtifactView {
    p.artifacts
        .iter()
        .find(|a| a.goal == Some(goal) && a.kind == ArtifactKind::Brief)
        .cloned()
        .expect("沒有 Brief")
}

/// 最後一個對話 agent 請求裡所有的工具結果，依順序。
fn chat_tool_outputs(model: &FnProvider) -> Vec<String> {
    let last = model
        .requests()
        .into_iter()
        .rev()
        .find(|r| !is_planner(r))
        .expect("沒有對話請求");
    last.input
        .iter()
        .filter_map(|i| match i {
            Item::ToolResult { output, .. } => Some(output.clone()),
            _ => None,
        })
        .collect()
}

fn create_goal_call(title: &str) -> String {
    call(
        "create_goal",
        json!({"title": title, "brief_md": "## 目標\n\n換成 OAuth\n\n## 約束\n\n- 不能停機"}),
    )
}

/// 對話 agent 開 Goal → Planner 拆任務、建 Ledger、貼里程碑；快照帶著 Goal 與 artifact。
#[tokio::test]
async fn create_goal_hands_off_to_planner() {
    let tmp = tempfile::tempdir().unwrap();
    let model = FnProvider::new("m", |req| {
        if !is_planner(req) {
            return chat_turn(req);
        }
        match planner_turn(req) {
            (_, 0) => ScriptedTurn::new()
                .tool_call(
                    "t1",
                    "create_task",
                    r#"{"title":"換掉登入頁","summary":"改用 OAuth 按鈕"}"#,
                )
                .tool_call(
                    "t2",
                    "create_task",
                    r#"{"title":"資料遷移","summary":"既有帳號綁定"}"#,
                )
                .tool_call(
                    "t3",
                    "write_artifact",
                    r##"{"kind":"Ledger","edits":[{"op":"replace_all","body_md":"# Ledger\n\n## 任務\n\n- [ ] 換掉登入頁\n- [ ] 資料遷移"}]}"##,
                )
                .tool_call("t4", "post_milestone", r#"{"text":"已拆成兩個任務"}"#)
                .usage(100, 20),
            _ => ScriptedTurn::reply("拆成兩個任務，Ledger 已建立"),
        }
    });
    let srv = launch(tmp.path(), model.clone()).await;
    let mut c = srv.client().await;
    c.snapshot().await;
    let project = c.create_project("登入").await;

    let events = say(&mut c, project, &create_goal_call("登入重構")).await;
    // 回覆下面附著 Goal 卡片。
    let cards: Vec<Card> = events
        .iter()
        .filter_map(|e| match e {
            ServerEvent::ItemDone { item, .. } => match &item.kind {
                StreamItemKind::Agent(m) => Some(m.cards.clone()),
                _ => None,
            },
            _ => None,
        })
        .flatten()
        .collect();
    let [Card::GoalCreated { goal: goal_id }] = cards.as_slice() else {
        panic!("預期一張 Goal 卡片，收到 {cards:?}");
    };
    let goal_id = *goal_id;

    let goal = settle_goal(&mut c, &events, |g| {
        g.planner_note.starts_with("拆成兩個任務")
    })
    .await;
    assert_eq!(goal.id, goal_id);
    assert_eq!(goal.title, "登入重構");
    assert_eq!(goal.state, GoalState::Active);
    let titles: Vec<_> = goal.tasks.iter().map(|t| t.title.as_str()).collect();
    assert_eq!(titles, ["換掉登入頁", "資料遷移"]);
    assert!(
        goal.tasks
            .iter()
            .all(|t| t.state == TaskStateView::Planned && t.steps[0].text == "建立（Planner）")
    );
    assert!(goal.brief.contains("換成 OAuth"));

    // 快照：Goal、Brief、Ledger 與里程碑卡片都在。
    let p = snapshot_project(&srv).await;
    assert_eq!(p.goals.len(), 1);
    assert_eq!(p.goals[0].tasks.len(), 2);
    // 每個 project 一份 Project Notes（M5），其他都掛在 Goal 上。
    let kinds: Vec<_> = p.artifacts.iter().map(|a| a.kind).collect();
    assert_eq!(
        kinds,
        [
            ArtifactKind::ProjectNotes,
            ArtifactKind::Brief,
            ArtifactKind::Ledger
        ]
    );
    assert!(
        p.artifacts
            .iter()
            .filter(|a| a.kind != ArtifactKind::ProjectNotes)
            .all(|a| a.goal == Some(goal_id))
    );
    assert!(
        brief_of(&p, goal_id)
            .body
            .starts_with("# 登入重構\n\n## 目標")
    );
    assert!(p.items.iter().any(|i| matches!(
        &i.kind,
        StreamItemKind::Card(Card::Milestone { goal, text }) if *goal == goal_id && text == "已拆成兩個任務"
    )));

    // Planner 的 context 是 Brief 與任務樹，不含對話。
    let planner_req = model
        .requests()
        .into_iter()
        .find(is_planner)
        .expect("Planner 沒有被叫醒");
    let (trigger, _) = planner_turn(&planner_req);
    assert!(trigger.contains("剛建立這個 Goal"), "{trigger}");
    assert!(trigger.contains("換成 OAuth"));
    assert!(trigger.contains("（還沒有任務）"));
    assert!(!trigger.contains("CALL create_goal"));
    assert_eq!(planner_req.input.len(), 1);

    // 下一輪對話 agent 的 instructions 帶著 Goal 概況與里程碑。
    say(&mut c, project, "現在怎樣").await;
    let chat = model
        .requests()
        .into_iter()
        .rev()
        .find(|r| !is_planner(r))
        .unwrap();
    let instructions = chat.instructions.unwrap();
    assert!(instructions.contains("「登入重構」"), "{instructions}");
    assert!(instructions.contains("已拆成兩個任務"));
    srv.stop().await;
}

/// 對話 agent 用 steer_goal 改 Brief：正在跑的 Planner 被中斷，新的 run 看到新方向。
#[tokio::test]
async fn steer_interrupts_planner_and_replans() {
    let tmp = tempfile::tempdir().unwrap();
    let model = FnProvider::new("m", |req| {
        if !is_planner(req) {
            return chat_turn(req);
        }
        let (trigger, _) = planner_turn(req);
        if trigger.contains("改方向") {
            ScriptedTurn::reply("依新方向重新規劃")
        } else {
            ScriptedTurn::new().hang()
        }
    });
    let srv = launch(tmp.path(), model.clone()).await;
    let mut c = srv.client().await;
    c.snapshot().await;
    let project = c.create_project("登入").await;
    let events = say(&mut c, project, &create_goal_call("登入重構")).await;
    let goal = settle_goal(&mut c, &events, |g| g.planner_note == "Planner 規劃中…").await;

    let steer = call(
        "steer_goal",
        json!({
            "goal_id": goal.id.to_string(),
            "reason": "使用者改用 passkey",
            "base_rev": 1,
            "edits": [{"op": "replace_section", "heading": "目標", "body_md": "改用 passkey"}],
        }),
    );
    let events = say(&mut c, project, &steer).await;
    let goal = settle_goal(&mut c, &events, |g| g.planner_note == "依新方向重新規劃").await;
    assert!(goal.brief.contains("改用 passkey"));
    assert!(!goal.brief.contains("換成 OAuth"));

    let planner: Vec<_> = model.requests().into_iter().filter(is_planner).collect();
    assert_eq!(planner.len(), 2);
    let (trigger, _) = planner_turn(&planner[1]);
    assert!(
        trigger.contains("對話 agent要求改方向：使用者改用 passkey"),
        "{trigger}"
    );
    assert!(trigger.contains("rev：2"));
    let outputs = chat_tool_outputs(&model);
    assert!(outputs.last().unwrap().contains("\"brief_rev\":2"));

    let p = snapshot_project(&srv).await;
    assert_eq!(brief_of(&p, goal.id).rev, 2);
    srv.stop().await;
}

/// 你在編輯器改 Brief：叫醒 Planner；期間 agent 改過時以區段合併；同一段衝突時拒絕；
/// 版本紀錄與還原。
#[tokio::test]
async fn human_edits_merge_steer_and_restore() {
    let tmp = tempfile::tempdir().unwrap();
    let model = FnProvider::new("m", |req| {
        if is_planner(req) {
            ScriptedTurn::reply("好")
        } else {
            chat_turn(req)
        }
    });
    let srv = launch(tmp.path(), model.clone()).await;
    let mut c = srv.client().await;
    c.snapshot().await;
    let project = c.create_project("登入").await;
    let events = say(&mut c, project, &create_goal_call("登入重構")).await;
    let goal = settle_goal(&mut c, &events, |g| g.planner_note == "好").await;
    let p = snapshot_project(&srv).await;
    let brief = brief_of(&p, goal.id);
    let v1 = brief.body.clone();

    // 你加一段：v2，標記「補充」是你改的，Planner 被叫醒。
    let mine = format!("{v1}\n## 補充\n\n人類加的\n");
    c.send(&ClientCommand::SaveArtifact {
        artifact: brief.id,
        base_rev: 1,
        body: mine.clone(),
    })
    .await;
    let a = until_artifact(&mut c, |a| a.id == brief.id && a.rev == 2).await;
    assert!(a.body.contains("人類加的"));
    let edited: Vec<_> = a.human_edits.iter().map(|(h, _)| h.as_str()).collect();
    assert_eq!(edited, ["補充"]);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let woke = model.requests().into_iter().filter(is_planner).any(|r| {
            planner_turn(&r)
                .0
                .contains("使用者直接修改了 Brief（現在是 v2）")
        });
        if woke {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "Planner 沒有被叫醒");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // agent 改「目標」→ v3。
    let steer = call(
        "steer_goal",
        json!({
            "goal_id": goal.id.to_string(),
            "reason": "改目標",
            "base_rev": 2,
            "edits": [{"op": "replace_section", "heading": "目標", "body_md": "agent 的目標"}],
        }),
    );
    let events = say(&mut c, project, &steer).await;
    assert!(events.iter().any(
        |e| matches!(e, ServerEvent::ArtifactChanged { artifact, .. } if artifact.id == brief.id && artifact.rev == 3)
    ));

    // 你從 v2 改「補充」：和 agent 改的「目標」合併成 v4。
    let mine2 = mine.replace("人類加的", "人類改過");
    c.send(&ClientCommand::SaveArtifact {
        artifact: brief.id,
        base_rev: 2,
        body: mine2.clone(),
    })
    .await;
    let a = until_artifact(&mut c, |a| a.id == brief.id && a.rev == 4).await;
    assert!(a.body.contains("人類改過"));
    assert!(a.body.contains("agent 的目標"));

    // 你從 v2 改「目標」：和 agent 衝突，拒絕。
    c.send(&ClientCommand::SaveArtifact {
        artifact: brief.id,
        base_rev: 2,
        body: mine.replace("換成 OAuth", "我的目標"),
    })
    .await;
    let events = c.until(|e| matches!(e, ServerEvent::Error { .. })).await;
    let Some(ServerEvent::Error { message }) = events.last() else {
        unreachable!()
    };
    assert!(message.contains("「目標」"), "{message}");

    // 版本紀錄：由新到舊，作者與 diff。
    c.send(&ClientCommand::ArtifactHistory { artifact: brief.id })
        .await;
    let events = c
        .until(|e| matches!(e, ServerEvent::ArtifactHistory { .. }))
        .await;
    let Some(ServerEvent::ArtifactHistory { revisions, .. }) = events.last() else {
        unreachable!()
    };
    let revs: Vec<_> = revisions
        .iter()
        .map(|r| (r.rev, r.author.as_str()))
        .collect();
    assert_eq!(
        revs,
        [(4, "你"), (3, "對話 agent"), (2, "你"), (1, "對話 agent")]
    );
    assert!(revisions[0].diff.contains("+人類改過"));
    assert!(revisions[3].diff.is_empty());

    // 還原成 v1：新增 v5，內容和 v1 一樣。
    c.send(&ClientCommand::RestoreArtifact {
        artifact: brief.id,
        rev: 1,
    })
    .await;
    let a = until_artifact(&mut c, |a| a.id == brief.id && a.rev == 5).await;
    assert_eq!(a.body, v1);
    srv.stop().await;
}

/// 你取消任務、對話 agent 取消 Goal；Planner 用 complete_goal 結束另一個 Goal（參數錯誤時可以
/// 重試），成功後這一輪就停。
#[tokio::test]
async fn cancel_and_complete_goals() {
    let tmp = tempfile::tempdir().unwrap();
    let model = FnProvider::new("m", |req| {
        if !is_planner(req) {
            return chat_turn(req);
        }
        let (trigger, k) = planner_turn(req);
        match (trigger.contains("要完成的"), k) {
            // 第一次參數錯誤：工具回錯誤，run 不結束，模型可以重試。
            (true, 0) => ScriptedTurn::new().tool_call("c0", "complete_goal", r#"{"summary":""}"#),
            (true, 1) => ScriptedTurn::new()
                .tool_call("c1", "complete_goal", r#"{"summary":"不需要做了"}"#)
                .tool_call("c2", "create_task", r#"{"title":"不該建立","summary":""}"#),
            (false, 0) => ScriptedTurn::new()
                .tool_call("t1", "create_task", r#"{"title":"甲","summary":""}"#)
                .tool_call("t2", "create_task", r#"{"title":"乙","summary":""}"#),
            _ => ScriptedTurn::reply("規劃好了"),
        }
    });
    let srv = launch(tmp.path(), model.clone()).await;
    let mut c = srv.client().await;
    c.snapshot().await;
    let project = c.create_project("登入").await;
    let events = say(&mut c, project, &create_goal_call("登入重構")).await;
    let goal = settle_goal(&mut c, &events, |g| g.planner_note == "規劃好了").await;
    let first = goal.tasks[0].id;

    // 你取消「甲」。
    c.send(&ClientCommand::Cancel { task: first }).await;
    let g = until_goal(&mut c, |g| {
        g.tasks
            .first()
            .is_some_and(|t| t.state == TaskStateView::Cancelled)
    })
    .await;
    assert_eq!(g.tasks[1].state, TaskStateView::Planned);
    assert_eq!(g.tasks[0].steps.last().unwrap().text, "取消（你）");

    // 對話 agent 取消整個 Goal：「乙」也一起取消。
    let cancel = call(
        "cancel_goal",
        json!({"goal_id": goal.id.to_string(), "reason": "不做了"}),
    );
    say(&mut c, project, &cancel).await;
    let p = snapshot_project(&srv).await;
    let g = &p.goals[0];
    assert_eq!(
        g.state,
        GoalState::Cancelled {
            reason: "不做了".into()
        }
    );
    assert!(g.tasks.iter().all(|t| t.state == TaskStateView::Cancelled));
    assert!(
        g.tasks[1]
            .steps
            .last()
            .unwrap()
            .text
            .contains("Goal 已取消")
    );

    // 已經結束的任務不能再取消。
    c.send(&ClientCommand::Cancel { task: first }).await;
    c.until(|e| matches!(e, ServerEvent::Error { message } if message.contains("已經結束")))
        .await;

    // 已取消的 Goal 不能 steer。
    let steer = call(
        "steer_goal",
        json!({"goal_id": goal.id.to_string(), "reason": "回來"}),
    );
    say(&mut c, project, &steer).await;
    assert!(
        chat_tool_outputs(&model)
            .last()
            .unwrap()
            .contains("已經已取消")
    );

    // complete_goal 成功後 Goal 已結束，同一輪後面的 create_task 失敗；下一輪不再開始。
    let events = say(&mut c, project, &create_goal_call("要完成的")).await;
    let done = settle_goal(&mut c, &events, |g| {
        matches!(g.state, GoalState::Done { .. })
    })
    .await;
    assert_eq!(
        done.state,
        GoalState::Done {
            summary: "不需要做了".into()
        }
    );
    assert!(done.tasks.is_empty());
    let p = snapshot_project(&srv).await;
    assert!(p.items.iter().any(|i| matches!(
        &i.kind,
        StreamItemKind::Card(Card::Milestone { goal, text }) if *goal == done.id && text == "Goal 完成：不需要做了"
    )));
    let turns: Vec<_> = model
        .requests()
        .into_iter()
        .filter(|r| is_planner(r) && planner_turn(r).0.contains("要完成的"))
        .collect();
    assert_eq!(turns.len(), 2, "參數錯誤後重試一次，成功後就停");
    let outputs: Vec<_> = turns[1]
        .input
        .iter()
        .filter_map(|i| match i {
            Item::ToolResult { output, .. } => Some(output.as_str()),
            _ => None,
        })
        .collect();
    assert!(outputs[0].contains("summary 不能是空的"), "{outputs:?}");
    srv.stop().await;
}

/// 對話 agent 寫 Spec：建立時回覆附 artifact 卡片；你改過之後用舊的 base_rev 寫會衝突，
/// read_artifact 附上你的修改；不能寫 profile 沒授權的種類。
#[tokio::test]
async fn write_artifact_conflicts_with_human_edits() {
    let tmp = tempfile::tempdir().unwrap();
    let model = FnProvider::new("m", chat_turn);
    let srv = launch(tmp.path(), model.clone()).await;
    let mut c = srv.client().await;
    c.snapshot().await;
    let project = c.create_project("規格").await;

    let create = call(
        "write_artifact",
        json!({"kind": "Spec", "title": "登入規格", "edits": [
            {"op": "replace_all", "body_md": "# 登入規格\n\n## 需求\n\n- A"}
        ]}),
    );
    let events = say(&mut c, project, &create).await;
    let spec = events
        .iter()
        .find_map(|e| match e {
            ServerEvent::ArtifactChanged { artifact, .. } => Some(artifact.clone()),
            _ => None,
        })
        .expect("沒有 ArtifactChanged");
    assert_eq!(spec.kind, ArtifactKind::Spec);
    assert_eq!(spec.goal, None);
    assert!(events.iter().any(|e| matches!(
        e,
        ServerEvent::ItemDone { item, .. } if matches!(
            &item.kind,
            StreamItemKind::Agent(m) if matches!(m.cards.as_slice(), [Card::Artifact { id, .. }] if *id == spec.id)
        )
    )));

    c.send(&ClientCommand::SaveArtifact {
        artifact: spec.id,
        base_rev: 1,
        body: "# 登入規格\n\n## 需求\n\n- A\n- B（人類）\n".into(),
    })
    .await;
    until_artifact(&mut c, |a| a.id == spec.id && a.rev == 2).await;

    let id = spec.id.to_string();
    let stale = call(
        "write_artifact",
        json!({"artifact_id": id, "base_rev": 1, "edits": [
            {"op": "replace_section", "heading": "需求", "body_md": "- C"}
        ]}),
    );
    let read = call("read_artifact", json!({"artifact_id": id}));
    let fresh = call(
        "write_artifact",
        json!({"artifact_id": id, "base_rev": 2, "edits": [
            {"op": "append_section", "heading": "風險", "body_md": "- 沒有"}
        ]}),
    );
    let forbidden = call(
        "write_artifact",
        json!({"kind": "Ledger", "edits": [{"op": "replace_all", "body_md": "x"}]}),
    );
    say(
        &mut c,
        project,
        &[stale, read.clone(), fresh, forbidden, read].join("\n"),
    )
    .await;
    let outputs = chat_tool_outputs(&model);
    let n = outputs.len();
    let conflict = &outputs[n - 5];
    assert!(conflict.contains("\"conflict\""), "{conflict}");
    assert!(conflict.contains("\"changed_by_human\":true"));
    assert!(conflict.contains("+- B（人類）"));
    let first_read = &outputs[n - 4];
    assert!(first_read.contains("rev 2"));
    assert!(
        first_read.contains("使用者在你上次讀取之後改了"),
        "{first_read}"
    );
    assert!(outputs[n - 3].contains("\"rev\":3"));
    assert!(outputs[n - 2].contains("你不能建立 Ledger"));
    // 讀過之後就不再附上同樣的修改。
    assert!(!outputs[n - 1].contains("使用者在你上次讀取之後改了"));

    let p = snapshot_project(&srv).await;
    let specs: Vec<_> = p
        .artifacts
        .iter()
        .filter(|a| a.kind != ArtifactKind::ProjectNotes)
        .collect();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].rev, 3);
    assert!(specs[0].body.contains("## 風險"));
    srv.stop().await;
}

/// 服務關閉時 Planner run 被中斷，下次啟動從佇列接著跑。
#[tokio::test]
async fn planner_run_resumes_after_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let hang = FnProvider::new("m", |req| {
        if is_planner(req) {
            ScriptedTurn::new().hang()
        } else {
            chat_turn(req)
        }
    });
    let srv = launch(tmp.path(), hang.clone()).await;
    let mut c = srv.client().await;
    c.snapshot().await;
    let project = c.create_project("登入").await;
    let events = say(&mut c, project, &create_goal_call("登入重構")).await;
    settle_goal(&mut c, &events, |g| g.planner_note == "Planner 規劃中…").await;
    drop(c);
    srv.stop().await;

    let calls = Arc::new(AtomicUsize::new(0));
    let resumed = {
        let calls = calls.clone();
        FnProvider::new("m", move |req| {
            if is_planner(req) {
                calls.fetch_add(1, Ordering::SeqCst);
                assert!(planner_turn(req).0.contains("剛建立這個 Goal"));
                ScriptedTurn::reply("接著規劃")
            } else {
                chat_turn(req)
            }
        })
    };
    let srv = launch(tmp.path(), resumed).await;
    let mut c = srv.client().await;
    let (_, snap) = c.snapshot().await;
    if snap.projects[0].goals[0].planner_note != "接著規劃" {
        until_goal(&mut c, |g| g.planner_note == "接著規劃").await;
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    srv.stop().await;
}
