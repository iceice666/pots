//! M6b 的整合測試：封存 Goal、成本報表與單價表。

mod common;

use std::path::Path;
use std::sync::Arc;

use common::{Client, ScriptedApi, TestServer};
use pots_llm::testing::{FnProvider, ScriptedProvider, ScriptedTurn};
use pots_llm::{Item, ModelRequest};
use pots_proto::{
    ClientCommand, GoalState, ModelPrice, ProjectId, ProviderDraft, ProviderId, ProviderKind,
    SendMode, ServerEvent, StreamItemKind,
};
use pots_server::{Config, Deps, FakeModelAuth};
use serde_json::json;

const PLANNER: &str = "你是 Pots 的 Planner";
const MODEL: &str = "test-model";

fn is_planner(req: &ModelRequest) -> bool {
    req.instructions
        .as_deref()
        .is_some_and(|i| i.starts_with(PLANNER))
}

/// 對話 agent：最後一則使用者訊息裡的 `CALL` 行依序各呼叫一次；工具結果之後回覆完成。
fn chat_turn(req: &ModelRequest) -> ScriptedTurn {
    let Some(last_user) = req
        .input
        .iter()
        .rposition(|i| matches!(i, Item::User { .. }))
    else {
        return ScriptedTurn::reply("完成").usage(10, 5);
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

fn call(name: &str, args: serde_json::Value) -> String {
    format!("CALL {name} {}", serde_json::to_string(&args).unwrap())
}

/// 用依請求內容回應的假 provider 啟動服務（`TestServer::start` 只收腳本）。
async fn launch(dir: &Path) -> TestServer {
    let provider = FnProvider::new(MODEL, |req| {
        if is_planner(req) {
            return ScriptedTurn::new()
                .tool_call(
                    "p1",
                    "complete_goal",
                    json!({"summary": "完成了"}).to_string(),
                )
                .usage(100, 20);
        }
        chat_turn(req)
    });
    let auth = Arc::new(FakeModelAuth::signed_in());
    let deps = Deps::new(provider, MODEL, auth.clone());
    let handle = pots_server::start(
        Config::desktop(dir).with_default_provider(ProviderId::CHATGPT),
        deps,
    )
    .await
    .expect("服務啟動失敗");
    TestServer {
        handle,
        provider: ScriptedProvider::new([]),
        api: ScriptedApi::new([]),
        auth,
    }
}

async fn say(c: &mut Client, project: ProjectId, text: &str) -> Vec<ServerEvent> {
    c.say(project, text, SendMode::Queue).await;
    c.until_idle(project).await
}

#[tokio::test]
async fn archive_hides_a_finished_goal_but_the_cost_report_still_sees_it() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = launch(tmp.path()).await;
    let mut c = srv.client().await;
    let _ = c.snapshot().await;
    let project = c.create_project("demo").await;

    // 開 Goal，等 Planner 完成它。
    let mut seen = say(
        &mut c,
        project,
        &call(
            "create_goal",
            json!({
                "title": "寫測試",
                "brief_md": "## 目標\n\n寫一個測試。",
            }),
        ),
    )
    .await;
    let goal = seen
        .iter()
        .rev()
        .find_map(|e| match e {
            ServerEvent::GoalChanged { goal, .. } => Some(goal.id),
            _ => None,
        })
        .expect("沒有建立 Goal");
    if !seen.iter().any(|e| {
        matches!(
            e,
            ServerEvent::GoalChanged { goal: g, .. }
                if g.id == goal && matches!(g.state, GoalState::Done { .. })
        )
    }) {
        seen.extend(
            c.until(|e| {
                matches!(
                    e,
                    ServerEvent::GoalChanged { goal: g, .. }
                        if g.id == goal && matches!(g.state, GoalState::Done { .. })
                )
            })
            .await,
        );
    }

    // 封存後從快照消失，並收到 GoalArchived。
    let archived = say(
        &mut c,
        project,
        &call("archive_goal", json!({"goal_id": goal.to_string()})),
    )
    .await;
    assert!(
        archived
            .iter()
            .any(|e| matches!(e, ServerEvent::GoalArchived { goal: g, .. } if *g == goal)),
        "{archived:?}"
    );
    let mut fresh = srv.client().await;
    let (_, snap) = fresh.snapshot().await;
    assert!(
        !snap.projects[0].goals.iter().any(|g| g.id == goal),
        "封存的 Goal 還出現在快照裡"
    );

    // 成本報表仍看得到這個 Goal 的 Planner run（封存不刪資料）。
    c.send(&ClientCommand::CostReport {
        project: Some(project),
        since: None,
        until: None,
    })
    .await;
    let report = match c.next().await {
        ServerEvent::CostReport { report } => report,
        other => panic!("預期 CostReport，收到 {other:?}"),
    };
    assert!(
        report
            .rows
            .iter()
            .any(|r| r.goal == Some(goal) && r.agent == "planner")
    );
    srv.stop().await;
}

#[tokio::test]
async fn api_cost_report_uses_the_configured_price_table() {
    let tmp = tempfile::tempdir().unwrap();
    let api = ScriptedApi::new([ScriptedTurn::reply("hi").usage(1_000, 500)]);
    let config = Config::desktop(tmp.path());
    // 未登入 ChatGPT，新增 API provider 時才會自動變成使用中。
    let auth = Arc::new(FakeModelAuth::signed_out());
    let srv = TestServer::launch(config, [], api, auth).await;
    let mut c = srv.client().await;
    let _ = c.snapshot().await;

    // 新增一個 OpenAI 相容的 API provider；沒有使用中的 provider 時會自動選它。
    c.send(&ClientCommand::SaveProvider {
        draft: ProviderDraft {
            id: None,
            kind: ProviderKind::OpenAiCompatible,
            name: "OpenAI".into(),
            base_url: "https://api.openai.com/v1".into(),
            model: MODEL.into(),
            api_key: Some("sk-test-1234567890".into()),
        },
    })
    .await;
    let providers = loop {
        match c.next().await {
            ServerEvent::Providers { providers, .. } => break providers,
            ServerEvent::Error { message } => panic!("{message}"),
            _ => {}
        }
    };
    assert!(providers.active.is_some(), "API provider 沒有變成使用中");

    let project = c.create_project("demo").await;
    let evs = say(&mut c, project, "你好").await;
    assert!(evs.iter().any(|e| matches!(
        e,
        ServerEvent::ItemDone { item, .. }
            if matches!(item.kind, StreamItemKind::Agent(_))
    )));

    c.send(&ClientCommand::SetPrice {
        price: ModelPrice {
            provider: "openai".into(),
            model: MODEL.into(),
            input_per_mtok: 2.0,
            output_per_mtok: 4.0,
            cache_read_per_mtok: Some(0.5),
        },
    })
    .await;
    assert!(matches!(c.next().await, ServerEvent::Pricing { info } if info.models == 1));

    c.send(&ClientCommand::CostReport {
        project: Some(project),
        since: None,
        until: None,
    })
    .await;
    let report = match c.next().await {
        ServerEvent::CostReport { report } => report,
        other => panic!("預期 CostReport，收到 {other:?}"),
    };
    let row = report
        .rows
        .iter()
        .find(|r| r.agent == "chat")
        .expect("沒有對話 agent 的用量");
    assert_eq!(row.input_tokens, 1_000);
    assert_eq!(row.output_tokens, 500);
    assert_eq!(row.billing, pots_proto::Billing::Api);
    let cost = row.estimated_cost_usd.expect("沒有估計金額");
    assert!((cost - 0.004).abs() < 1e-9, "{cost}");
    assert_eq!(row.priced_tokens, 1_500);
    assert_eq!(report.totals.api_tokens, 1_500);
    assert_eq!(report.totals.estimated_cost_usd, cost);
    srv.stop().await;
}

#[test]
fn parses_models_dev_prices_and_ignores_models_without_cost() {
    let json = br#"{
        "openai": {"models": {
            "gpt-x": {"cost": {"input": 2.5, "output": 15.0, "cache_read": 0.25}},
            "free": {}
        }},
        "anthropic": {"models": {
            "claude-x": {"cost": {"input": 3.0, "output": 15.0}}
        }}
    }"#;
    let mut prices = pots_server::cost::parse_models_dev(json).unwrap();
    prices.sort_by(|a, b| a.model.cmp(&b.model));
    assert_eq!(prices.len(), 2);
    assert_eq!(prices[0].model, "claude-x");
    assert_eq!(prices[0].input_per_mtok, 3.0);
    assert_eq!(prices[1].model, "gpt-x");
    assert_eq!(prices[1].cache_read_per_mtok, Some(0.25));
}
