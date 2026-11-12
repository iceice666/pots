//! pots-server 的整合測試：每個測試在暫存目錄啟動一個服務（loopback 隨機 port）。

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Client, TestServer, auth_headers, http_get, is_state};
use pots_llm::testing::ScriptedTurn;
use pots_llm::{Item, LlmError};
use pots_proto::{
    AgentState, ClientCommand, InstanceKind, ModelAuthView, MsgStatus, SendMode, ServerEvent,
    StreamItemKind, StreamItemView,
};
use pots_server::{Error, FakeModelAuth};

fn agent_msg(item: &StreamItemView) -> &pots_proto::Msg {
    match &item.kind {
        StreamItemKind::Agent(m) => m,
        other => panic!("預期 agent 訊息，得到 {other:?}"),
    }
}

fn done_item(evs: &[ServerEvent]) -> &StreamItemView {
    evs.iter()
        .find_map(|e| match e {
            ServerEvent::ItemDone { item, .. } => Some(item),
            _ => None,
        })
        .expect("沒有 ItemDone")
}

fn user_texts(input: &[Item]) -> Vec<&str> {
    input
        .iter()
        .filter_map(|i| match i {
            Item::User { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn access_checks() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = TestServer::start(tmp.path(), []).await;
    let addr = srv.addr();
    let host = ("Host", format!("127.0.0.1:{}", addr.port()));

    // /healthz 不需要 token。
    let (status, body) = http_get(addr, "/healthz", std::slice::from_ref(&host)).await;
    assert_eq!(status, 200);
    assert!(body.contains(&srv.handle.instance_id.to_string()));

    // 沒有 token、token 錯誤。
    let (status, _) = http_get(addr, "/ws", std::slice::from_ref(&host)).await;
    assert_eq!(status, 401);
    let wrong = ("Authorization", "Bearer nope".to_string());
    let (status, _) = http_get(addr, "/projects/x/items", &[host.clone(), wrong]).await;
    assert_eq!(status, 401);

    // Host 不符（DNS rebinding）。
    let mut headers = auth_headers(&srv);
    headers[0].1 = format!("evil.example:{}", addr.port());
    let (status, _) = http_get(addr, "/healthz", &headers).await;
    assert_eq!(status, 403);
    headers[0].1 = "127.0.0.1:1".into();
    let (status, _) = http_get(addr, "/healthz", &headers).await;
    assert_eq!(status, 403);

    // localhost 可以；帶 Origin（瀏覽器）一律拒絕，即使 token 正確。
    let mut headers = auth_headers(&srv);
    headers[0].1 = format!("localhost:{}", addr.port());
    let (status, _) = http_get(addr, "/healthz", &headers).await;
    assert_eq!(status, 200);
    headers.push(("Origin", "http://localhost".into()));
    let (status, _) = http_get(addr, "/healthz", &headers).await;
    assert_eq!(status, 403);

    // WebSocket 升級也要 token。
    let req = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
        format!("ws://{addr}/ws"),
    )
    .unwrap();
    match tokio_tungstenite::connect_async(req).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status(), 401)
        }
        other => panic!("預期 401，得到 {:?}", other.map(|_| ())),
    }

    // 連線資訊寫在 server.json，不含在 Debug 輸出裡。
    let info = pots_server::read_connection_info(tmp.path()).unwrap();
    assert_eq!(info.addr, addr);
    assert_eq!(info.token.as_str(), srv.token());
    assert!(!format!("{:?}", srv.handle).contains(srv.token()));

    srv.stop().await;
    assert!(pots_server::read_connection_info(tmp.path()).is_none());
}

#[tokio::test]
async fn second_start_on_same_dir_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = TestServer::start(tmp.path(), []).await;
    let deps = pots_server::Deps::new(
        pots_llm::testing::ScriptedProvider::new([]),
        "m",
        Arc::new(FakeModelAuth::signed_out()),
    );
    match pots_server::start(pots_server::Config::desktop(tmp.path()), deps.clone()).await {
        Err(Error::AlreadyRunning(Some(info))) => {
            assert_eq!(info.addr, srv.addr());
            assert_eq!(info.instance_id, srv.handle.instance_id);
        }
        other => panic!("預期 AlreadyRunning，得到 {other:?}"),
    }
    srv.stop().await;
    // 關閉後鎖已釋放。
    let again = pots_server::start(pots_server::Config::desktop(tmp.path()), deps)
        .await
        .unwrap();
    again.shutdown();
    again.wait().await.unwrap();
}

#[tokio::test]
async fn resume_zero_gets_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = TestServer::start(tmp.path(), []).await;
    let mut c = srv.client().await;
    let (seq, inst) = c.snapshot().await;
    assert_eq!(inst.id, srv.handle.instance_id);
    assert_eq!(inst.kind, InstanceKind::Desktop);
    assert!(!inst.name.is_empty());
    assert!(inst.projects.is_empty());
    assert_eq!(inst.model_auth, ModelAuthView::SignedIn { account: None });
    // 啟動時記了一筆 ModelAuth。
    assert!(seq >= 1);

    // 第一個指令不是 Resume 時回錯誤並關閉。
    let mut req = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
        format!("ws://{}/ws", srv.addr()),
    )
    .unwrap();
    req.headers_mut().insert(
        "authorization",
        format!("Bearer {}", srv.token()).parse().unwrap(),
    );
    let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let mut raw = Client::from_ws(ws);
    raw.send_raw(r#"{"t":"CreateProject","name":"x"}"#).await;
    assert!(matches!(raw.next().await, ServerEvent::Error { .. }));
    assert!(raw.try_next(Duration::from_secs(2)).await.is_none());

    // 找不到的核准與無法解析的指令回 Error。
    c.send(&ClientCommand::Decide {
        approval: pots_proto::ApprovalId::new(),
        approve: true,
        note: None,
    })
    .await;
    assert!(
        matches!(c.next().await, ServerEvent::Error { message } if message == "找不到這筆核准")
    );
    c.send_raw("not json").await;
    assert!(matches!(c.next().await, ServerEvent::Error { .. }));
    srv.stop().await;
}

#[tokio::test]
async fn chat_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = TestServer::start(
        tmp.path(),
        [ScriptedTurn::new().text_deltas(["你好", "，我是 Pots"])],
    )
    .await;
    let mut c = srv.client().await;
    c.snapshot().await;
    let project = c.create_project("測試").await;
    let human = c.say(project, "嗨", SendMode::Queue).await;
    let evs = c.until_idle(project).await;

    // Item(human) → ItemUpdated(human，已進歷史) → Item(agent, Streaming) → Thinking
    // → Delta… → ItemDone → Idle
    let ServerEvent::Item { item, .. } = &evs[0] else {
        panic!("{:?}", evs[0])
    };
    assert_eq!(item.id, human);
    assert!(matches!(&item.kind, StreamItemKind::Human(m) if m.text == "嗨"));
    assert!(matches!(&evs[1], ServerEvent::ItemUpdated { item, .. } if item.id == human));
    let ServerEvent::Item { item: agent, .. } = &evs[2] else {
        panic!("{:?}", evs[2])
    };
    let msg = agent_msg(agent);
    assert_eq!(msg.status, MsgStatus::Streaming);
    assert_eq!(msg.text, "");
    assert!(
        matches!(&msg.author, pots_proto::Author::Agent { label, .. } if label == "測試 負責人")
    );
    assert!(is_state(&evs[3], project, AgentState::Thinking));
    let deltas: Vec<&str> = evs
        .iter()
        .filter_map(|e| match e {
            ServerEvent::Delta { item_id, text, .. } if *item_id == agent.id => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, ["你好", "，我是 Pots"]);
    let done = done_item(&evs);
    assert_eq!(done.id, agent.id);
    assert_eq!(agent_msg(done).status, MsgStatus::Done);
    assert_eq!(agent_msg(done).text, "你好，我是 Pots");

    let req = &srv.provider.requests()[0];
    assert_eq!(req.model, "test-model");
    assert!(
        req.instructions
            .as_deref()
            .unwrap()
            .contains("這個 project 叫「測試」，你是它的負責人，名字是「測試 負責人」。")
    );
    assert_eq!(req.input, [Item::User { text: "嗨".into() }]);
    assert!(req.tools.iter().any(|t| t.name == "get_status"));

    // 同一則訊息重送：不重複寫入、不再觸發 run。
    c.send(&ClientCommand::Send {
        project,
        id: human,
        text: "嗨".into(),
        mode: SendMode::Queue,
        attachments: Vec::new(),
    })
    .await;
    c.assert_quiet(Duration::from_millis(300)).await;
    assert_eq!(srv.provider.requests().len(), 1);
    srv.stop().await;
}

#[tokio::test]
async fn tool_call_round_trip_and_multi_turn_history() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = TestServer::start(
        tmp.path(),
        [
            ScriptedTurn::new()
                .text("我查一下")
                .tool_call("c1", "get_status", "{}"),
            ScriptedTurn::reply("有 2 則訊息"),
            ScriptedTurn::reply("第二次回答"),
        ],
    )
    .await;
    let mut c = srv.client().await;
    c.snapshot().await;
    let project = c.create_project("工具").await;
    c.say(project, "狀態如何？", SendMode::Queue).await;
    let evs = c.until_idle(project).await;

    let updates: Vec<_> = evs
        .iter()
        .filter_map(|e| match e {
            ServerEvent::ItemUpdated { item, .. } => match &item.kind {
                StreamItemKind::Agent(m) => m.tools.clone(),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(updates.len(), 2, "ToolStarted 與 ToolFinished 各一次");
    assert_eq!(updates[0].details, ["get_status：執行中"]);
    assert_eq!(updates[1].details, ["get_status：完成"]);
    // 「我查一下」接著工具呼叫，是旁白：結束後訊息只留最後的回答。
    let done = agent_msg(done_item(&evs));
    assert_eq!(done.text, "有 2 則訊息");
    assert!(done.narration.is_empty());
    assert_eq!(done.tools.as_ref().unwrap().text, "呼叫了 1 個工具");

    // 第二輪的輸入帶著工具結果。
    let reqs = srv.provider.requests();
    assert_eq!(reqs.len(), 2);
    let result = reqs[1]
        .input
        .iter()
        .find_map(|i| match i {
            Item::ToolResult { call_id, output } if call_id == "c1" => Some(output.clone()),
            _ => None,
        })
        .expect("第二輪沒有工具結果");
    let status: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(status["project"], "工具");
    assert_eq!(status["messages"], 2);
    assert_eq!(status["queued"], false);
    assert!(status["now"].as_str().unwrap().ends_with('Z'));

    // 第二則訊息：模型拿到完整的既有歷史。
    c.say(project, "再一次", SendMode::Queue).await;
    c.until_idle(project).await;
    let reqs = srv.provider.requests();
    let input = &reqs[2].input;
    assert_eq!(input.len(), 6, "{input:?}");
    assert_eq!(
        input[0],
        Item::User {
            text: "狀態如何？".into()
        }
    );
    assert_eq!(
        input[1],
        Item::Assistant {
            text: "我查一下".into()
        }
    );
    assert!(matches!(&input[2], Item::ToolCall(c) if c.call_id == "c1"));
    assert!(matches!(&input[3], Item::ToolResult { call_id, .. } if call_id == "c1"));
    assert_eq!(
        input[4],
        Item::Assistant {
            text: "有 2 則訊息".into()
        }
    );
    assert_eq!(
        input[5],
        Item::User {
            text: "再一次".into()
        }
    );
    srv.stop().await;
}

#[tokio::test]
async fn narration_streams_then_moves_to_why() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = TestServer::start(
        tmp.path(),
        [
            ScriptedTurn::new()
                .text("我先查一下")
                .delay(Duration::from_millis(300))
                .tool_call("c1", "get_status", "{}"),
            ScriptedTurn::new().text_deltas(["結", "果"]),
            // 第二則：最後一輪沒有文字時，最後一段旁白當成回答。
            ScriptedTurn::new()
                .text("答案在這")
                .tool_call("c2", "get_status", "{}"),
            ScriptedTurn::new(),
        ],
    )
    .await;
    let mut a = srv.client().await;
    a.snapshot().await;
    let project = a.create_project("旁白").await;
    a.say(project, "狀態如何？", SendMode::Queue).await;
    a.until(|e| matches!(e, ServerEvent::Delta { .. })).await;

    // 第一輪還在串流時重連：補送全文。之後內文搬進旁白、變短，第二輪的增量仍要送到。
    let mut b = Client::connect(&srv, a.last_seq).await;
    match b.next().await {
        ServerEvent::StreamingText { text, .. } => assert_eq!(text, "我先查一下"),
        other => panic!("預期 StreamingText，收到 {other:?}"),
    }
    let evs = b.until_idle(project).await;
    let moved = evs
        .iter()
        .find_map(|e| match e {
            ServerEvent::ItemUpdated { item, .. } => match &item.kind {
                StreamItemKind::Agent(m) if !m.narration.is_empty() => Some(m.clone()),
                _ => None,
            },
            _ => None,
        })
        .expect("沒有帶旁白的 ItemUpdated");
    assert_eq!(moved.narration, ["我先查一下"]);
    assert_eq!(moved.text, "");
    assert_eq!(moved.status, MsgStatus::Streaming);
    let deltas: Vec<&str> = evs
        .iter()
        .filter_map(|e| match e {
            ServerEvent::Delta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, ["結", "果"]);
    let done = done_item(&evs);
    assert_eq!(agent_msg(done).text, "結果");
    assert!(agent_msg(done).narration.is_empty());

    // 旁白在「為什麼」面板的 trace 裡，排在模型輪之後、工具之前。
    b.send(&ClientCommand::Why { item: done.id }).await;
    let why = b
        .until(|e| matches!(e, ServerEvent::Why { why: Some(_) }))
        .await
        .into_iter()
        .find_map(|e| match e {
            ServerEvent::Why { why: Some(why) } => Some(why),
            _ => None,
        })
        .unwrap();
    let kinds: Vec<&str> = why.steps.iter().map(|s| s.kind.as_str()).collect();
    assert_eq!(kinds, ["model", "text", "tool", "model"]);
    assert_eq!(why.steps[1].detail, "我先查一下");

    // 最後一輪沒有文字：訊息不會是空的。
    b.say(project, "再一次", SendMode::Queue).await;
    let evs = b.until_idle(project).await;
    assert_eq!(agent_msg(done_item(&evs)).text, "答案在這");

    // 使用者看得到輸出的對話 agent 會被要求先說要做什麼。
    let req = &srv.provider.requests()[0];
    assert!(
        req.instructions
            .as_deref()
            .unwrap()
            .contains("呼叫工具前，先用一句話告訴使用者")
    );
    srv.stop().await;
}

#[tokio::test]
async fn resume_replays_only_missed_events() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = TestServer::start(tmp.path(), [ScriptedTurn::reply("收到")]).await;
    let mut a = srv.client().await;
    a.snapshot().await;
    let project = a.create_project("補送").await;
    let seen = a.last_seq;
    drop(a);

    // A 不在時發生的事。
    let mut b = srv.client().await;
    b.snapshot().await;
    b.say(project, "A 不在", SendMode::Queue).await;
    let live = b.until_idle(project).await;
    let live_logged: Vec<u64> = live.iter().filter_map(|e| e.seq()).collect();

    let mut a = Client::connect(&srv, seen).await;
    let mut replayed = Vec::new();
    while let Some(ev) = a.try_next(Duration::from_millis(500)).await {
        assert!(!matches!(ev, ServerEvent::Snapshot { .. }), "不該送快照");
        assert!(!matches!(ev, ServerEvent::Delta { .. }), "Delta 不補送");
        replayed.push(ev.seq().unwrap());
    }
    assert_eq!(replayed, live_logged);
    assert!(replayed.iter().all(|s| *s > seen));

    // 目前最新的序號：沒有東西要補。
    let mut c = Client::connect(&srv, a.last_seq).await;
    c.assert_quiet(Duration::from_millis(300)).await;
    // 比目前還新（例如資料庫被換掉）：送快照。
    let mut d = Client::connect(&srv, a.last_seq + 100).await;
    let (seq, inst) = d.snapshot().await;
    assert_eq!(seq, a.last_seq);
    assert_eq!(inst.projects[0].items.len(), 2);
    srv.stop().await;
}

#[tokio::test]
async fn reconnect_during_streaming_then_stop() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = TestServer::start(
        tmp.path(),
        [ScriptedTurn::new().text_deltas(["一", "二"]).hang()],
    )
    .await;
    let mut a = srv.client().await;
    a.snapshot().await;
    let project = a.create_project("串流").await;
    a.say(project, "說話", SendMode::Queue).await;
    let mut deltas = 0;
    a.until(|e| {
        if matches!(e, ServerEvent::Delta { .. }) {
            deltas += 1;
        }
        deltas == 2
    })
    .await;
    // 讓 a 記住的序號停在串流開始之後。
    let seen = a.last_seq;

    for last_seq in [0, seen] {
        let mut b = Client::connect(&srv, last_seq).await;
        if last_seq == 0 {
            let (_, inst) = b.snapshot().await;
            let p = &inst.projects[0];
            assert_eq!(p.agent_state, AgentState::Thinking);
            assert_eq!(
                agent_msg(p.items.last().unwrap()).status,
                MsgStatus::Streaming
            );
        }
        match b.next().await {
            ServerEvent::StreamingText { text, .. } => assert_eq!(text, "一二"),
            other => panic!("預期 StreamingText，收到 {other:?}"),
        }
        b.assert_quiet(Duration::from_millis(200)).await;
    }

    a.send(&ClientCommand::Stop { project }).await;
    let evs = a.until_idle(project).await;
    let done = agent_msg(done_item(&evs));
    assert_eq!(done.status, MsgStatus::Done);
    assert_eq!(done.text, "一二\n\n（已停止）");

    // 停止後沒有串流中的訊息，重連不再補 StreamingText。
    let mut b = Client::connect(&srv, a.last_seq).await;
    b.assert_quiet(Duration::from_millis(200)).await;
    srv.stop().await;
}

#[tokio::test]
async fn queued_messages_run_together_after_current() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = TestServer::start(
        tmp.path(),
        [
            ScriptedTurn::new()
                .text("第一")
                .delay(Duration::from_millis(800)),
            ScriptedTurn::reply("第二"),
        ],
    )
    .await;
    let mut c = srv.client().await;
    c.snapshot().await;
    let project = c.create_project("排隊").await;
    c.say(project, "A", SendMode::Queue).await;
    c.until(|e| matches!(e, ServerEvent::Delta { .. })).await;
    c.say(project, "B", SendMode::Queue).await;
    c.say(project, "C", SendMode::Queue).await;

    let first = c.until_idle(project).await;
    assert_eq!(agent_msg(done_item(&first)).text, "第一");
    let second = c.until_idle(project).await;
    assert_eq!(agent_msg(done_item(&second)).text, "第二");

    let reqs = srv.provider.requests();
    assert_eq!(reqs.len(), 2, "B 與 C 合併成一個 run");
    assert_eq!(user_texts(&reqs[0].input), ["A"]);
    assert_eq!(user_texts(&reqs[1].input), ["A", "B", "C"]);
    assert_eq!(
        reqs[1].input[1],
        Item::Assistant {
            text: "第一".into()
        }
    );
    srv.stop().await;
}

#[tokio::test]
async fn steer_cancels_and_restarts() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = TestServer::start(
        tmp.path(),
        [
            ScriptedTurn::new().text_deltas(["寫到一半"]).hang(),
            ScriptedTurn::reply("改好了"),
        ],
    )
    .await;
    let mut c = srv.client().await;
    c.snapshot().await;
    let project = c.create_project("改方向").await;
    c.say(project, "做 X", SendMode::Queue).await;
    c.until(|e| matches!(e, ServerEvent::Delta { .. })).await;
    let steer = c.say(project, "改做 Y", SendMode::Steer).await;

    let first = c.until_idle(project).await;
    let human = first
        .iter()
        .find_map(|e| match e {
            ServerEvent::Item { item, .. } if item.id == steer => Some(item),
            _ => None,
        })
        .unwrap();
    assert!(matches!(&human.kind, StreamItemKind::Human(m) if m.steer));
    let done = agent_msg(done_item(&first));
    assert_eq!(done.status, MsgStatus::Done);
    assert_eq!(done.text, "寫到一半\n\n（已依新訊息改方向）");

    let second = c.until_idle(project).await;
    assert_eq!(agent_msg(done_item(&second)).text, "改好了");
    let reqs = srv.provider.requests();
    assert_eq!(reqs.len(), 2);
    // 被取消的那輪沒有完成，不進歷史。
    assert_eq!(
        reqs[1].input,
        [
            Item::User {
                text: "做 X".into()
            },
            Item::User {
                text: "改做 Y".into()
            }
        ]
    );
    srv.stop().await;
}

#[tokio::test]
async fn stop_clears_queue_but_keeps_history() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = TestServer::start(
        tmp.path(),
        [
            ScriptedTurn::new().text("忙").hang(),
            ScriptedTurn::reply("好"),
        ],
    )
    .await;
    let mut c = srv.client().await;
    c.snapshot().await;
    let project = c.create_project("停止").await;
    c.say(project, "A", SendMode::Queue).await;
    c.until(|e| matches!(e, ServerEvent::Delta { .. })).await;
    let b = c.say(project, "B", SendMode::Queue).await;
    c.until(|e| matches!(e, ServerEvent::Item { .. })).await;
    c.send(&ClientCommand::Stop { project }).await;
    c.until_idle(project).await;
    // 被清掉的 B 進了歷史，客戶端收到 ItemUpdated 以清掉「排隊中」。
    c.until(|e| matches!(e, ServerEvent::ItemUpdated { item, .. } if item.id == b))
        .await;
    c.assert_quiet(Duration::from_millis(300)).await;
    assert_eq!(srv.provider.requests().len(), 1, "被停止的佇列不觸發 run");

    c.say(project, "C", SendMode::Queue).await;
    c.until_idle(project).await;
    let reqs = srv.provider.requests();
    assert_eq!(user_texts(&reqs[1].input), ["A", "B", "C"]);
    srv.stop().await;
}

#[tokio::test]
async fn model_errors_and_login() {
    let tmp = tempfile::tempdir().unwrap();
    let auth = Arc::new(FakeModelAuth::signed_out());
    let srv = TestServer::start_with(
        tmp.path(),
        [
            ScriptedTurn::reject(LlmError::NeedsLogin),
            ScriptedTurn::reject(LlmError::NeedsLogin),
            ScriptedTurn::reply("好了"),
        ],
        auth.clone(),
    )
    .await;
    let mut c = srv.client().await;
    let (_, inst) = c.snapshot().await;
    assert_eq!(inst.model_auth, ModelAuthView::SignedOut);
    let project = c.create_project("登入").await;
    c.say(project, "嗨", SendMode::Queue).await;
    let evs = c.until_idle(project).await;
    let done = agent_msg(done_item(&evs));
    assert_eq!(done.status, MsgStatus::Failed);
    assert!(done.text.contains("登入"), "{}", done.text);

    let before_login = c.last_seq;
    c.send(&ClientCommand::StartModelLogin).await;
    let (mut pending, mut login_url) = (false, false);
    while !(pending && login_url) {
        match c.next().await {
            ServerEvent::ModelAuth {
                state: ModelAuthView::Pending { url },
                ..
            } => pending = url.starts_with("https://"),
            ServerEvent::LoginUrl { url } => login_url = url.starts_with("https://"),
            other => panic!("預期 ModelAuth Pending 與 LoginUrl，收到 {other:?}"),
        }
    }
    // 登入進行中再按一次：狀態沒變，網址仍回給這條連線（只給它）。
    let mut other = Client::connect(&srv, c.last_seq).await;
    c.send(&ClientCommand::StartModelLogin).await;
    assert!(matches!(c.next().await, ServerEvent::LoginUrl { .. }));
    other.assert_quiet(Duration::from_millis(200)).await;
    // 快照（記憶體）有完整網址；事件日誌與補送不留網址（可能帶 id_token_hint）。
    let (_, inst) = Client::connect(&srv, 0).await.snapshot().await;
    assert!(matches!(inst.model_auth, ModelAuthView::Pending { url } if !url.is_empty()));
    let mut r = Client::connect(&srv, before_login).await;
    assert!(matches!(
        r.next().await,
        ServerEvent::ModelAuth { state: ModelAuthView::Pending { url }, .. } if url.is_empty()
    ));
    auth.set(ModelAuthView::SignedIn {
        account: Some("me@example.com".into()),
    });
    assert!(matches!(
        c.next().await,
        ServerEvent::ModelAuth {
            state: ModelAuthView::SignedIn { .. },
            ..
        }
    ));

    // 授權顯示已登入，但 provider 說要重新登入：狀態改成錯誤，讓客戶端提示。
    c.say(project, "再試", SendMode::Queue).await;
    let evs = c.until_idle(project).await;
    assert!(evs.iter().any(|e| matches!(
        e,
        ServerEvent::ModelAuth {
            state: ModelAuthView::Error { .. },
            ..
        }
    )));

    // 之後推論成功：授權狀態改回授權控制的實際狀態。
    c.say(project, "第三次", SendMode::Queue).await;
    let evs = c.until_idle(project).await;
    assert_eq!(agent_msg(done_item(&evs)).text, "好了");
    assert!(evs.iter().any(|e| matches!(
        e,
        ServerEvent::ModelAuth {
            state: ModelAuthView::SignedIn { .. },
            ..
        }
    )));

    c.send(&ClientCommand::ModelLogout).await;
    assert!(matches!(
        c.next().await,
        ServerEvent::ModelAuth {
            state: ModelAuthView::SignedOut,
            ..
        }
    ));
    srv.stop().await;
}

#[tokio::test]
async fn restart_preserves_state() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = TestServer::start(tmp.path(), [ScriptedTurn::reply("記住了：藍色")]).await;
    let instance = srv.handle.instance_id;
    let mut c = srv.client().await;
    c.snapshot().await;
    let project = c.create_project("重啟").await;
    c.say(project, "我喜歡藍色", SendMode::Queue).await;
    c.until_idle(project).await;
    let seen = c.last_seq;
    drop(c);
    srv.stop().await;

    let srv = TestServer::start(tmp.path(), [ScriptedTurn::reply("你喜歡藍色")]).await;
    assert_eq!(srv.handle.instance_id, instance);
    let mut c = srv.client().await;
    let (_, inst) = c.snapshot().await;
    assert_eq!(inst.projects.len(), 1);
    let p = &inst.projects[0];
    assert_eq!(p.id, project);
    assert_eq!(p.items.len(), 2);
    assert_eq!(agent_msg(&p.items[1]).text, "記住了：藍色");

    // 舊的序號仍然可以補送（事件日誌跨重啟保留）。
    let mut r = Client::connect(&srv, seen).await;
    assert!(matches!(r.next().await, ServerEvent::ModelAuth { .. }));

    c.say(project, "我喜歡什麼？", SendMode::Queue).await;
    c.until_idle(project).await;
    let input = &srv.provider.requests()[0].input;
    assert_eq!(
        input,
        &[
            Item::User {
                text: "我喜歡藍色".into()
            },
            Item::Assistant {
                text: "記住了：藍色".into()
            },
            Item::User {
                text: "我喜歡什麼？".into()
            },
        ]
    );
    srv.stop().await;
}

#[tokio::test]
async fn shutdown_during_run_and_queue() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = TestServer::start(tmp.path(), [ScriptedTurn::new().text("忙").hang()]).await;
    let mut c = srv.client().await;
    c.snapshot().await;
    let project = c.create_project("關閉").await;
    c.say(project, "A", SendMode::Queue).await;
    c.until(|e| matches!(e, ServerEvent::Delta { .. })).await;
    c.say(project, "B", SendMode::Queue).await;
    c.until(|e| matches!(e, ServerEvent::Item { .. })).await;
    srv.stop().await;
    // 伺服器關閉時連線也關閉。
    while c.try_next(Duration::from_secs(2)).await.is_some() {}

    let srv = TestServer::start(tmp.path(), [ScriptedTurn::reply("好")]).await;
    let mut c = srv.client().await;
    let (_, inst) = c.snapshot().await;
    let p = &inst.projects[0];
    assert_eq!(p.agent_state, AgentState::Idle);
    let reply = agent_msg(&p.items[1]);
    assert_eq!(reply.status, MsgStatus::Done);
    assert_eq!(reply.text, "忙\n\n（服務已關閉，回覆中斷）");
    // 排隊中的 B 沒有觸發 run，但已寫進歷史。
    c.say(project, "C", SendMode::Queue).await;
    c.until_idle(project).await;
    assert_eq!(
        user_texts(&srv.provider.requests()[0].input),
        ["A", "B", "C"]
    );
    srv.stop().await;
}

#[tokio::test]
async fn items_pagination() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = TestServer::start(
        tmp.path(),
        (1..=3).map(|i| ScriptedTurn::reply(format!("回覆 {i}"))),
    )
    .await;
    let mut c = srv.client().await;
    c.snapshot().await;
    let project = c.create_project("分頁").await;
    for i in 1..=3 {
        c.say(project, &format!("訊息 {i}"), SendMode::Queue).await;
        c.until_idle(project).await;
    }

    let get = |path: String| {
        let headers = auth_headers(&srv);
        let addr = srv.addr();
        async move {
            let (status, body) = http_get(addr, &path, &headers).await;
            (status, body)
        }
    };
    let texts = |body: &str| -> Vec<String> {
        let items: Vec<StreamItemView> = serde_json::from_str(body).unwrap();
        items
            .iter()
            .map(|i| match &i.kind {
                StreamItemKind::Human(m) | StreamItemKind::Agent(m) => m.text.clone(),
                _ => String::new(),
            })
            .collect()
    };

    let (status, body) = get(format!("/projects/{project}/items?limit=2")).await;
    assert_eq!(status, 200);
    assert_eq!(texts(&body), ["訊息 3", "回覆 3"]);
    let first: Vec<StreamItemView> = serde_json::from_str(&body).unwrap();

    let (status, body) = get(format!(
        "/projects/{project}/items?before={}&limit=3",
        first[0].id
    ))
    .await;
    assert_eq!(status, 200);
    assert_eq!(texts(&body), ["回覆 1", "訊息 2", "回覆 2"]);
    let page: Vec<StreamItemView> = serde_json::from_str(&body).unwrap();

    let (_, body) = get(format!("/projects/{project}/items?before={}", page[0].id)).await;
    assert_eq!(texts(&body), ["訊息 1"]);

    let (status, _) = get(format!("/projects/{}/items", pots_proto::ProjectId::new())).await;
    assert_eq!(status, 404);
    let (status, _) = get(format!(
        "/projects/{project}/items?before={}",
        pots_proto::ItemId::new()
    ))
    .await;
    assert_eq!(status, 400);
    srv.stop().await;
}

/// run 寫 `om_raw` 的同時有人送訊息：寫入 transaction 不能因為 SQLITE_BUSY 失敗。
#[tokio::test]
async fn sends_while_run_writes_history() {
    let tmp = tempfile::tempdir().unwrap();
    let mut turns: Vec<ScriptedTurn> = (0..7)
        .map(|i| ScriptedTurn::new().tool_call(format!("c{i}"), "get_status", "{}"))
        .collect();
    turns.push(ScriptedTurn::reply("第一輪完成"));
    turns.push(ScriptedTurn::reply("排隊的都看到了"));
    let srv = TestServer::start(tmp.path(), turns).await;
    let mut c = srv.client().await;
    c.snapshot().await;
    let project = c.create_project("並行").await;
    c.say(project, "開始", SendMode::Queue).await;
    c.until(|e| is_state(e, project, AgentState::Thinking))
        .await;

    // 多條連線同時送，和 run 的 om_raw 寫入交錯。
    let mut senders = Vec::new();
    for k in 0..4 {
        let mut s = Client::connect(&srv, 0).await;
        s.snapshot().await;
        senders.push(tokio::spawn(async move {
            for i in 0..5 {
                s.say(project, &format!("{k}-{i}"), SendMode::Queue).await;
            }
            // 有 Error 就是寫入失敗。
            while let Some(ev) = s.try_next(Duration::from_millis(300)).await {
                assert!(!matches!(ev, ServerEvent::Error { .. }), "{ev:?}");
            }
        }));
    }
    for s in senders {
        s.await.unwrap();
    }
    c.until_idle(project).await;
    c.until_idle(project).await;
    let reqs = srv.provider.requests();
    let last = reqs.last().unwrap();
    assert_eq!(
        user_texts(&last.input).len(),
        21,
        "{:?}",
        user_texts(&last.input)
    );
    srv.stop().await;
}

/// 每個 project 有自己的負責人：預設從 project 名稱推導，可以改名，之後的回覆與
/// instructions 都用新名字。
#[tokio::test]
async fn each_project_has_its_own_lead() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = TestServer::start(tmp.path(), [ScriptedTurn::reply("收到")]).await;
    let mut c = srv.client().await;
    c.snapshot().await;
    let auth = c.create_project("auth-service").await;
    let web = c.create_project("web").await;

    let (_, snap) = Client::connect(&srv, 0).await.snapshot().await;
    let name = |id| {
        snap.projects
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.agent_name.clone())
            .unwrap()
    };
    assert_eq!(name(auth), "auth-service 負責人");
    assert_eq!(name(web), "web 負責人");

    // 空白或太長的名字被拒絕，不寫入。
    for bad in ["  ".to_string(), "長".repeat(41)] {
        c.send(&ClientCommand::RenameAgent {
            project: auth,
            name: bad,
        })
        .await;
        let evs = c.until(|e| matches!(e, ServerEvent::Error { .. })).await;
        assert!(
            !evs.iter()
                .any(|e| matches!(e, ServerEvent::ProjectAgentName { .. }))
        );
    }

    c.send(&ClientCommand::RenameAgent {
        project: auth,
        name: "  阿認  ".into(),
    })
    .await;
    let evs = c
        .until(|e| matches!(e, ServerEvent::ProjectAgentName { .. }))
        .await;
    assert!(matches!(
        evs.last(),
        Some(ServerEvent::ProjectAgentName { project, agent_name, .. })
            if *project == auth && agent_name == "阿認"
    ));

    c.say(auth, "你是誰", SendMode::Queue).await;
    let evs = c.until_idle(auth).await;
    let reply = done_item(&evs);
    assert!(matches!(
        &agent_msg(reply).author,
        pots_proto::Author::Agent { label, .. } if label == "阿認"
    ));
    let instructions = srv.provider.requests()[0].instructions.clone().unwrap();
    assert!(instructions.contains("名字是「阿認」"), "{instructions}");

    // 另一個 project 不受影響；新連線的快照也是新名字。
    let (_, snap) = Client::connect(&srv, 0).await.snapshot().await;
    let names: Vec<_> = snap
        .projects
        .iter()
        .map(|p| (p.id, p.agent_name.as_str()))
        .collect();
    assert!(names.contains(&(auth, "阿認")));
    assert!(names.contains(&(web, "web 負責人")));
    srv.stop().await;
}
