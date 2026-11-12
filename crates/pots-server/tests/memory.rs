//! Observational Memory 的整合測試（`proposal/memory.md`）：經由 WebSocket 對話，模型換成
//! 依請求內容回應的假 provider（對話 agent、Observer、Reflector 共用或分開）。

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::{Client, ScriptedApi, TestServer};
use pots_llm::testing::{FnProvider, ScriptedTurn};
use pots_llm::{Item, LlmError, ModelProvider, ModelRequest};
use pots_proto::{
    ClientCommand, MemoryView, ModelOption, ProjectId, ProviderDraft, ProviderId, ProviderKind,
    SendMode, ServerEvent,
};
use pots_server::{ApiProviderFactory, Config, Deps, FakeModelAuth, OmConfig};

const OBSERVER: &str = "你是 Pots 的 Observer";
const REFLECTOR: &str = "你是 Pots 的 Reflector";
const EARLY: &str = "我們決定：資料庫用 SQLite，服務 port 用 7421，專案代號叫藍鰭。";

fn role(request: &ModelRequest) -> &'static str {
    match request.instructions.as_deref() {
        Some(i) if i.starts_with(OBSERVER) => "observer",
        Some(i) if i.starts_with(REFLECTOR) => "reflector",
        _ => "chat",
    }
}

fn first_text(request: &ModelRequest) -> String {
    match request.input.first() {
        Some(Item::User { text }) => text.clone(),
        _ => String::new(),
    }
}

/// 假的模型：
///
/// - Observer：把含「決定」的人類訊息各記成一條高優先的觀察，最後一行「目前在做什麼」。
/// - Reflector：只留含「決定」的觀察。
/// - 對話：最後一則是「查狀態」就呼叫 `get_status`；問「一開始決定」時，從記憶區塊找答案；
///   其他回「收到」。
fn fake_model(request: &ModelRequest) -> ScriptedTurn {
    match role(request) {
        "observer" => {
            let mut out = String::new();
            for line in first_text(request).lines() {
                if let Some((_, text)) = line.split_once("] 使用者：")
                    && text.contains("決定")
                {
                    out.push_str(&format!("- 2026-11-07 · 高 · {text}\n"));
                }
            }
            out.push_str("目前在做什麼：測試記憶");
            ScriptedTurn::reply(out)
        }
        "reflector" => {
            let kept: Vec<_> = first_text(request)
                .lines()
                .filter(|l| l.starts_with("- ") && l.contains("決定"))
                .map(|l| l.trim_end_matches("（釘選）").to_string())
                .collect();
            ScriptedTurn::reply(format!("{}\n目前在做什麼：反思後", kept.join("\n")))
        }
        _ => match request.input.last() {
            Some(Item::User { text }) if text.contains("查狀態") => {
                ScriptedTurn::new().tool_call("call_status", "get_status", "{}")
            }
            Some(Item::User { text }) if text.contains("一開始決定") => {
                let memory = first_text(request);
                let answer = if memory.starts_with("<memory>") && memory.contains("藍鰭") {
                    "記得：SQLite、port 7421、代號藍鰭。"
                } else {
                    "不記得了。"
                };
                ScriptedTurn::reply(answer)
            }
            _ => ScriptedTurn::reply("收到"),
        },
    }
}

/// 用 `FnProvider` 當內建 ChatGPT 方案的模型啟動服務（使用中的 provider 是 ChatGPT）。
async fn launch(dir: &Path, memory: OmConfig, model: Arc<FnProvider>) -> TestServer {
    launch_with(dir, memory, model, Arc::new(FnApi(None))).await
}

async fn launch_with(
    dir: &Path,
    memory: OmConfig,
    model: Arc<FnProvider>,
    api: Arc<FnApi>,
) -> TestServer {
    let mut config = Config::desktop(dir).with_default_provider(ProviderId::CHATGPT);
    config.memory = memory;
    let auth = Arc::new(FakeModelAuth::signed_in());
    let deps = Deps::new(model, "chat-model", auth.clone()).with_api_providers(api);
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

/// API provider 一律接到同一個 `FnProvider`。
struct FnApi(Option<Arc<FnProvider>>);

#[async_trait::async_trait]
impl ApiProviderFactory for FnApi {
    fn build(
        &self,
        _: ProviderKind,
        _: &url::Url,
        _: &str,
        _: &[pots_proto::ProviderHeader],
    ) -> Result<Arc<dyn ModelProvider>, String> {
        match &self.0 {
            Some(p) => Ok(p.clone()),
            None => Err("沒有 API provider".into()),
        }
    }

    async fn list_models(
        &self,
        _: ProviderKind,
        _: &url::Url,
        _: &str,
        _: &[pots_proto::ProviderHeader],
    ) -> Result<Vec<ModelOption>, LlmError> {
        Ok(vec![])
    }
}

fn small() -> OmConfig {
    OmConfig {
        observe_after: 200,
        reflect_after: 100_000,
        block_after: 100_000,
        keep_recent: 2,
        max_chunk: 100_000,
        ..OmConfig::default()
    }
}

/// 傳一則訊息並等 agent 回到閒置，回傳期間的事件。
async fn say(c: &mut Client, project: ProjectId, text: &str) -> Vec<ServerEvent> {
    c.say(project, text, SendMode::Queue).await;
    c.until_idle(project).await
}

fn memory_of(events: &[ServerEvent], project: ProjectId) -> Option<MemoryView> {
    events.iter().rev().find_map(|e| match e {
        ServerEvent::Memory {
            project: p, memory, ..
        } if *p == project => Some(memory.clone()),
        _ => None,
    })
}

/// 收事件直到這個 project 的記憶滿足 `pred`。
async fn until_memory(
    c: &mut Client,
    project: ProjectId,
    mut pred: impl FnMut(&MemoryView) -> bool,
) -> MemoryView {
    let events = c
        .until(|e| matches!(e, ServerEvent::Memory { project: p, memory, .. } if *p == project && pred(memory)))
        .await;
    memory_of(&events, project).unwrap()
}

/// 收掉背景整理送來的事件，直到 300ms 內沒有新事件。
async fn drain(c: &mut Client) -> Vec<ServerEvent> {
    let mut out = Vec::new();
    while let Some(ev) = c.try_next(Duration::from_millis(300)).await {
        out.push(ev);
    }
    out
}

/// 已經收到的事件裡最後的記憶符合 `pred` 就用它，否則繼續等。
async fn settle_memory(
    c: &mut Client,
    project: ProjectId,
    seen: &[ServerEvent],
    mut pred: impl FnMut(&MemoryView) -> bool,
) -> MemoryView {
    if let Some(m) = memory_of(seen, project).filter(|m| pred(m)) {
        return m;
    }
    until_memory(c, project, pred).await
}

/// 四輪含「決定」的對話（每則約 130 token），回傳期間的事件。
async fn decisions(c: &mut Client, project: ProjectId) -> Vec<ServerEvent> {
    let mut seen = Vec::new();
    for i in 0..4 {
        let text = format!("決定第 {i} 件事：{}", "內容".repeat(60));
        seen.extend(say(c, project, &text).await);
    }
    seen
}

/// 每個工具呼叫都有結果、每個結果都有對應的呼叫。
fn assert_paired(input: &[Item]) {
    let mut open = std::collections::HashSet::new();
    for item in input {
        match item {
            Item::ToolCall(call) => {
                open.insert(call.call_id.clone());
            }
            Item::ToolResult { call_id, .. } => {
                assert!(open.remove(call_id), "工具結果 {call_id} 沒有對應的呼叫");
            }
            _ => {}
        }
    }
    assert!(open.is_empty(), "有工具呼叫沒有結果：{open:?}");
}

fn chars(input: &[Item]) -> usize {
    input
        .iter()
        .map(|i| match i {
            Item::User { text } | Item::Assistant { text } => text.chars().count(),
            Item::ToolCall(c) => c.arguments.chars().count(),
            Item::ToolResult { output, .. } => output.chars().count(),
            Item::Image { .. } => 0,
            Item::Opaque { .. } => 0,
        })
        .sum()
}

/// M1 的驗收：對話累積超過 100k tokens 後，agent 仍能回答早期做過的決定。用預設閾值。
#[tokio::test]
async fn remembers_early_decision_after_100k_tokens() {
    let tmp = tempfile::tempdir().unwrap();
    let model = FnProvider::new("fake", fake_model);
    let srv = launch(tmp.path(), OmConfig::default(), model.clone()).await;
    let mut c = srv.client().await;
    c.snapshot().await;
    let project = c.create_project("記憶驗收").await;

    let mut sent_chars = 0;
    for text in [EARLY, "幫我查狀態"] {
        say(&mut c, project, text).await;
        sent_chars += text.chars().count();
    }
    // 每則約 3000 字（CJK 一字約一個 token），40 輪約 12 萬 token。
    for i in 0..40 {
        let filler = format!(
            "第 {i} 段討論：{}",
            "這段只是背景資料，不需要記住。".repeat(200)
        );
        sent_chars += filler.chars().count();
        say(&mut c, project, &filler).await;
    }
    assert!(sent_chars > 100_000, "送出的對話只有 {sent_chars} 字");

    let events = say(
        &mut c,
        project,
        "我們一開始決定的資料庫、port 和代號是什麼？",
    )
    .await;
    let reply = events
        .iter()
        .rev()
        .find_map(|e| match e {
            ServerEvent::ItemDone { item, .. } => match &item.kind {
                pots_proto::StreamItemKind::Agent(m) => Some(m.text.clone()),
                _ => None,
            },
            _ => None,
        })
        .unwrap();
    assert_eq!(reply, "記得：SQLite、port 7421、代號藍鰭。");

    let requests = model.requests();
    let chat: Vec<_> = requests.iter().filter(|r| role(r) == "chat").collect();
    let observer = requests.iter().filter(|r| role(r) == "observer").count();
    assert!(observer >= 2, "Observer 只跑了 {observer} 次");
    for r in &chat {
        assert_paired(&r.input);
    }
    let last = chat.last().unwrap();
    let block = first_text(last);
    assert!(block.starts_with("<memory>"), "最後一次請求沒有記憶區塊");
    assert!(block.contains("資料庫用 SQLite，服務 port 用 7421，專案代號叫藍鰭"));
    // 早期的訊息已經整理進觀察，不在原始尾巴裡；送給模型的歷史有上限。
    assert!(
        !last.input[1..]
            .iter()
            .any(|i| matches!(i, Item::User { text } if text == EARLY))
    );
    assert!(
        chars(&last.input) < 65_000,
        "歷史有 {} 字",
        chars(&last.input)
    );
    // 整個對話的原始歷史遠大於最後一次請求。
    assert!(sent_chars > 2 * chars(&last.input));

    // 客戶端的快照也看得到這條觀察。
    let mut c2 = srv.client().await;
    let (_, snap) = c2.snapshot().await;
    let memory = &snap
        .projects
        .iter()
        .find(|p| p.id == project)
        .unwrap()
        .memory;
    assert!(memory.observations.iter().any(|o| o.text.contains("藍鰭")));
    assert_eq!(memory.current_task, "測試記憶");
    srv.stop().await;
}

/// 重啟後記憶還在，已經觀察過的不再送給 Observer。
#[tokio::test]
async fn memory_survives_restart_without_reobserving() {
    let tmp = tempfile::tempdir().unwrap();
    let model = FnProvider::new("fake", fake_model);
    let srv = launch(tmp.path(), small(), model.clone()).await;
    let mut c = srv.client().await;
    c.snapshot().await;
    let project = c.create_project("重啟").await;
    let seen = decisions(&mut c, project).await;
    let memory = settle_memory(&mut c, project, &seen, |m| m.observations.len() >= 2).await;
    assert!(memory.observations[0].text.starts_with("決定第 0 件事"));
    // 等背景整理結束；記憶以最後的狀態為準。
    let memory = memory_of(&drain(&mut c).await, project).unwrap_or(memory);
    let before = model
        .requests()
        .iter()
        .filter(|r| role(r) == "observer")
        .count();
    srv.stop().await;

    let model = FnProvider::new("fake", fake_model);
    let srv = launch(tmp.path(), small(), model.clone()).await;
    let mut c = srv.client().await;
    let (_, snap) = c.snapshot().await;
    let restored = &snap.projects[0].memory;
    for o in &memory.observations {
        assert!(
            restored
                .observations
                .iter()
                .any(|r| r.id == o.id && r.text == o.text)
        );
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(before >= 1);
    assert_eq!(
        model
            .requests()
            .iter()
            .filter(|r| role(r) == "observer")
            .count(),
        0,
        "重啟後不應該重新觀察"
    );

    say(&mut c, project, "繼續").await;
    let chat = model
        .requests()
        .into_iter()
        .rfind(|r| role(r) == "chat")
        .unwrap();
    assert!(first_text(&chat).contains("決定第 0 件事"));
    srv.stop().await;
}

/// 「記憶」分頁的釘選、修正、刪除經由指令進來，結果以 `Memory` 事件廣播，下一次 run 生效。
#[tokio::test]
async fn pin_edit_and_delete_from_the_client() {
    let tmp = tempfile::tempdir().unwrap();
    let model = FnProvider::new("fake", fake_model);
    let srv = launch(tmp.path(), small(), model.clone()).await;
    let mut c = srv.client().await;
    c.snapshot().await;
    let project = c.create_project("修正").await;
    let seen = decisions(&mut c, project).await;
    let memory = settle_memory(&mut c, project, &seen, |m| m.observations.len() >= 2).await;
    drain(&mut c).await;
    let (a, b) = (memory.observations[0].id, memory.observations[1].id);

    c.send(&ClientCommand::PinObservation {
        project,
        id: a,
        pinned: true,
    })
    .await;
    let m = until_memory(&mut c, project, |m| m.observations[0].pinned).await;
    assert_eq!(m.observations[0].id, a);

    c.send(&ClientCommand::EditObservation {
        project,
        id: a,
        text: "決定改用 Postgres".into(),
    })
    .await;
    let m = until_memory(&mut c, project, |m| {
        m.observations[0].text == "決定改用 Postgres"
    })
    .await;
    assert!(m.observations[0].edited && m.observations[0].pinned);
    let edited = m.observations[0].id;
    assert_ne!(edited, a);

    c.send(&ClientCommand::DeleteObservation { project, id: b })
        .await;
    until_memory(&mut c, project, |m| {
        !m.observations.iter().any(|o| o.id == b)
    })
    .await;

    // 已經被取代的那條不能再改。
    c.send(&ClientCommand::PinObservation {
        project,
        id: a,
        pinned: false,
    })
    .await;
    let evs = c.until(|e| matches!(e, ServerEvent::Error { .. })).await;
    assert!(
        matches!(evs.last(), Some(ServerEvent::Error { message }) if message.contains("找不到這條觀察"))
    );

    say(&mut c, project, "繼續").await;
    let chat = model
        .requests()
        .into_iter()
        .rfind(|r| role(r) == "chat")
        .unwrap();
    let block = first_text(&chat);
    assert!(block.contains("決定改用 Postgres"));
    assert!(!block.contains("決定第 0 件事"));
    assert!(!block.contains("決定第 1 件事"));
    srv.stop().await;
}

/// 整理記憶的模型和對話 agent 分開設定；刪掉那個 provider 時改回跟對話相同。
#[tokio::test]
async fn memory_model_is_configured_separately() {
    let tmp = tempfile::tempdir().unwrap();
    let chat = FnProvider::new("chat", fake_model);
    let cheap = FnProvider::new("cheap", fake_model);
    let srv = launch_with(
        tmp.path(),
        small(),
        chat.clone(),
        Arc::new(FnApi(Some(cheap.clone()))),
    )
    .await;
    let mut c = srv.client().await;
    let (_, snap) = c.snapshot().await;
    assert_eq!(snap.providers.memory, Default::default());

    c.send(&ClientCommand::SaveProvider {
        draft: ProviderDraft {
            id: None,
            kind: ProviderKind::OpenAiCompatible,
            name: "便宜的".into(),
            base_url: "https://api.example.com/v1".into(),
            model: "big-model".into(),
            api_key: Some("sk-test-123456789".into()),
            headers: Vec::new(),
        },
    })
    .await;
    let evs = c
        .until(|e| matches!(e, ServerEvent::Providers { .. }))
        .await;
    let Some(ServerEvent::Providers { providers, .. }) = evs.last() else {
        unreachable!()
    };
    assert_eq!(providers.active, Some(ProviderId::CHATGPT));
    let api = providers.items[1].id;

    c.send(&ClientCommand::SetMemoryModel {
        provider: Some(ProviderId::new()),
        model: String::new(),
    })
    .await;
    let evs = c.until(|e| matches!(e, ServerEvent::Error { .. })).await;
    assert!(
        matches!(evs.last(), Some(ServerEvent::Error { message }) if message.contains("找不到"))
    );

    c.send(&ClientCommand::SetMemoryModel {
        provider: Some(api),
        model: " small-model ".into(),
    })
    .await;
    let evs = c
        .until(|e| matches!(e, ServerEvent::Providers { .. }))
        .await;
    let Some(ServerEvent::Providers { providers, .. }) = evs.last() else {
        unreachable!()
    };
    assert_eq!(providers.memory.provider, Some(api));
    assert_eq!(providers.memory.model, "small-model");

    let project = c.create_project("分開").await;
    let seen = decisions(&mut c, project).await;
    settle_memory(&mut c, project, &seen, |m| !m.observations.is_empty()).await;
    let observed = cheap.requests();
    assert!(!observed.is_empty());
    assert!(
        observed
            .iter()
            .all(|r| role(r) == "observer" && r.model == "small-model")
    );
    assert!(
        chat.requests()
            .iter()
            .all(|r| role(r) == "chat" && r.model == "chat-model")
    );

    c.send(&ClientCommand::DeleteProvider { id: api }).await;
    let evs = c
        .until(|e| matches!(e, ServerEvent::Providers { .. }))
        .await;
    let Some(ServerEvent::Providers { providers, .. }) = evs.last() else {
        unreachable!()
    };
    assert_eq!(providers.memory, Default::default());
    srv.stop().await;

    // 設定存在資料庫：重啟後還是跟對話相同。
    let srv = launch(tmp.path(), small(), FnProvider::new("chat", fake_model)).await;
    let mut c = srv.client().await;
    let (_, snap) = c.snapshot().await;
    assert_eq!(snap.providers.memory, Default::default());
    srv.stop().await;
}
