//! 模型 provider 設定的整合測試：API provider 用腳本化的工廠（[`common::ScriptedApi`]）。

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::{Client, ScriptedApi, TestServer};
use pots_llm::testing::ScriptedTurn;
use pots_proto::{
    ClientCommand, MsgStatus, ProjectId, ProviderDraft, ProviderHeader, ProviderId, ProviderKind,
    ProvidersView, SendMode, ServerEvent, StreamItemKind,
};
use pots_server::{Config, FakeModelAuth};

const KEY: &str = "sk-test-secret-1234";

fn openai_draft(id: Option<ProviderId>, model: &str, key: Option<&str>) -> ProviderDraft {
    ProviderDraft {
        id,
        kind: ProviderKind::OpenAiCompatible,
        name: "OpenRouter".into(),
        base_url: "https://openrouter.ai/api/v1/".into(),
        model: model.into(),
        api_key: key.map(Into::into),
        headers: Vec::new(),
    }
}

async fn launch(
    dir: &Path,
    default: Option<ProviderId>,
    chatgpt: impl IntoIterator<Item = ScriptedTurn>,
    api: impl IntoIterator<Item = ScriptedTurn>,
    auth: FakeModelAuth,
) -> TestServer {
    let mut config = Config::desktop(dir);
    config.default_provider = default;
    TestServer::launch(config, chatgpt, ScriptedApi::new(api), Arc::new(auth)).await
}

/// 收到下一個 `Providers` 事件。
async fn next_providers(c: &mut Client) -> ProvidersView {
    match c.next().await {
        ServerEvent::Providers { providers, .. } => providers,
        other => panic!("預期 Providers，收到 {other:?}"),
    }
}

async fn expect_error(c: &mut Client, cmd: ClientCommand, needle: &str) {
    c.send(&cmd).await;
    match c.next().await {
        ServerEvent::Error { message } => {
            assert!(message.contains(needle), "{message} 不含 {needle}")
        }
        other => panic!("{cmd:?} 預期 Error，收到 {other:?}"),
    }
}

/// 傳一則訊息，回傳 agent 回覆的狀態與文字。
async fn ask(c: &mut Client, project: ProjectId, text: &str) -> (MsgStatus, String) {
    c.say(project, text, SendMode::Queue).await;
    let evs = c.until_idle(project).await;
    evs.iter()
        .find_map(|e| match e {
            ServerEvent::ItemDone { item, .. } => match &item.kind {
                StreamItemKind::Agent(m) => Some((m.status, m.text.clone())),
                _ => None,
            },
            _ => None,
        })
        .expect("沒有 ItemDone")
}

fn key_path(dir: &Path, id: ProviderId) -> std::path::PathBuf {
    dir.join("auth/providers").join(format!("{id}.json"))
}

/// 資料庫檔（含 WAL）的原始內容裡找不到 `needle`。
fn assert_not_in_db(dir: &Path, needle: &str) {
    for name in ["pots.db", "pots.db-wal"] {
        if let Ok(bytes) = std::fs::read(dir.join(name)) {
            assert!(
                !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
                "{name} 含有 API key"
            );
        }
    }
}

#[tokio::test]
async fn api_provider_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let srv = launch(
        dir,
        None,
        [
            ScriptedTurn::reply("ChatGPT 回覆"),
            ScriptedTurn::reply("改模型"),
        ],
        [ScriptedTurn::reply("API 回覆")],
        FakeModelAuth::signed_out(),
    )
    .await;
    let mut c = srv.client().await;
    let (start_seq, inst) = c.snapshot().await;
    // 沒有預設、ChatGPT 也沒登入：還沒有使用中的 provider；內建 ChatGPT 在第一筆。
    assert_eq!(inst.providers.active, None);
    assert_eq!(inst.providers.items.len(), 1);
    let chatgpt = &inst.providers.items[0];
    assert_eq!(chatgpt.id, ProviderId::CHATGPT);
    assert_eq!(chatgpt.kind, ProviderKind::ChatGpt);
    assert_eq!(chatgpt.name, "ChatGPT");
    assert_eq!(chatgpt.base_url, "");
    assert_eq!(chatgpt.model, "");
    assert_eq!(chatgpt.key_hint, None);

    let project = c.create_project("provider").await;
    let (status, text) = ask(&mut c, project, "嗨").await;
    assert_eq!(status, MsgStatus::Failed);
    assert!(text.contains("還沒設定模型 provider"), "{text}");
    assert!(srv.provider.requests().is_empty());

    // 新增 API provider：沒有使用中的 provider，所以自動變成使用中。
    c.send(&ClientCommand::SaveProvider {
        draft: openai_draft(None, "gpt-x", Some(KEY)),
    })
    .await;
    let view = next_providers(&mut c).await;
    assert_eq!(view.items.len(), 2);
    let api = view.items[1].clone();
    assert_eq!(view.active, Some(api.id));
    assert_eq!(api.kind, ProviderKind::OpenAiCompatible);
    assert_eq!(api.name, "OpenRouter");
    assert_eq!(api.base_url, "https://openrouter.ai/api/v1");
    assert_eq!(api.model, "gpt-x");
    assert_eq!(api.key_hint.as_deref(), Some("…1234"));
    let builds = srv.api.builds();
    assert_eq!(builds.len(), 1);
    assert_eq!(builds[0].api_key, KEY);
    assert_eq!(builds[0].base_url, "https://openrouter.ai/api/v1");

    // key 檔 0600、目錄 0700；key 不在資料庫與事件裡。
    let path = key_path(dir, api.id);
    assert!(std::fs::read_to_string(&path).unwrap().contains(KEY));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(mode(&dir.join("auth")), 0o700);
        // 暫存檔已經換上，沒有留下來。
        assert!(!path.with_extension("json.new").exists());
    }
    assert_not_in_db(dir, KEY);
    let mut replay = Client::connect(&srv, start_seq).await;
    while let Some(ev) = replay.try_next(Duration::from_millis(300)).await {
        assert!(!serde_json::to_string(&ev).unwrap().contains(KEY));
    }

    // run 走 API provider，用它的模型。
    let (status, text) = ask(&mut c, project, "用 API").await;
    assert_eq!((status, text.as_str()), (MsgStatus::Done, "API 回覆"));
    let reqs = srv.api.provider.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].model, "gpt-x");
    assert!(srv.provider.requests().is_empty());

    // 修改但不帶 key：沿用已存的 key。
    c.send(&ClientCommand::SaveProvider {
        draft: openai_draft(Some(api.id), "gpt-y", None),
    })
    .await;
    let view = next_providers(&mut c).await;
    assert_eq!(view.items[1].model, "gpt-y");
    assert_eq!(view.items[1].key_hint.as_deref(), Some("…1234"));
    assert_eq!(srv.api.builds().last().unwrap().api_key, KEY);

    // 換成 ChatGPT：run 走注入的 provider 與預設模型。
    c.send(&ClientCommand::SetActiveProvider {
        id: ProviderId::CHATGPT,
    })
    .await;
    assert_eq!(
        next_providers(&mut c).await.active,
        Some(ProviderId::CHATGPT)
    );
    let (_, text) = ask(&mut c, project, "用 ChatGPT").await;
    assert_eq!(text, "ChatGPT 回覆");
    assert_eq!(srv.provider.requests()[0].model, "test-model");

    // ChatGPT 指定模型。
    c.send(&ClientCommand::SaveProvider {
        draft: ProviderDraft {
            id: Some(ProviderId::CHATGPT),
            kind: ProviderKind::ChatGpt,
            name: "忽略".into(),
            base_url: String::new(),
            model: " gpt-5 ".into(),
            api_key: None,
            headers: Vec::new(),
        },
    })
    .await;
    let view = next_providers(&mut c).await;
    assert_eq!(view.items[0].model, "gpt-5");
    assert_eq!(view.items[0].name, "ChatGPT");
    ask(&mut c, project, "指定模型").await;
    assert_eq!(srv.provider.requests()[1].model, "gpt-5");

    // 刪掉使用中的 API provider：active 變成 None，key 檔刪除，之後的訊息失敗。
    c.send(&ClientCommand::SetActiveProvider { id: api.id })
        .await;
    assert_eq!(next_providers(&mut c).await.active, Some(api.id));
    c.send(&ClientCommand::DeleteProvider { id: api.id }).await;
    let view = next_providers(&mut c).await;
    assert_eq!(view.active, None);
    assert_eq!(view.items.len(), 1);
    assert!(!path.exists());
    let (status, text) = ask(&mut c, project, "還在嗎").await;
    assert_eq!(status, MsgStatus::Failed);
    assert!(text.contains("還沒設定模型 provider"), "{text}");
    srv.stop().await;
}

#[tokio::test]
async fn empty_model_fails_the_run() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = launch(tmp.path(), None, [], [], FakeModelAuth::signed_out()).await;
    let mut c = srv.client().await;
    c.snapshot().await;
    c.send(&ClientCommand::SaveProvider {
        draft: openai_draft(None, "", Some(KEY)),
    })
    .await;
    next_providers(&mut c).await;
    let project = c.create_project("沒模型").await;
    let (status, text) = ask(&mut c, project, "嗨").await;
    assert_eq!(status, MsgStatus::Failed);
    assert!(text.contains("請在設定選擇模型"), "{text}");
    assert!(srv.api.provider.requests().is_empty());
    srv.stop().await;
}

#[tokio::test]
async fn provider_validation() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = launch(tmp.path(), None, [], [], FakeModelAuth::signed_out()).await;
    let mut c = srv.client().await;
    c.snapshot().await;

    let mut no_name = openai_draft(None, "m", Some(KEY));
    no_name.name = "  ".into();
    expect_error(
        &mut c,
        ClientCommand::SaveProvider { draft: no_name },
        "名稱",
    )
    .await;
    for bad in [
        "ftp://example.com",
        "example.com/v1",
        "",
        "https://user:pw@api.example.com/v1",
    ] {
        let mut d = openai_draft(None, "m", Some(KEY));
        d.base_url = bad.into();
        expect_error(&mut c, ClientCommand::SaveProvider { draft: d }, "base URL").await;
    }
    // Anthropic 相容一定要有 key。
    for key in [None, Some("  ")] {
        expect_error(
            &mut c,
            ClientCommand::SaveProvider {
                draft: ProviderDraft {
                    kind: ProviderKind::AnthropicCompatible,
                    ..openai_draft(None, "m", key)
                },
            },
            "API key",
        )
        .await;
    }
    let mut chatgpt_new = openai_draft(None, "m", None);
    chatgpt_new.kind = ProviderKind::ChatGpt;
    expect_error(
        &mut c,
        ClientCommand::SaveProvider { draft: chatgpt_new },
        "ChatGPT",
    )
    .await;
    expect_error(
        &mut c,
        ClientCommand::SaveProvider {
            draft: openai_draft(Some(ProviderId::CHATGPT), "m", Some(KEY)),
        },
        "種類",
    )
    .await;
    expect_error(
        &mut c,
        ClientCommand::SaveProvider {
            draft: openai_draft(Some(ProviderId::new()), "m", Some(KEY)),
        },
        "找不到",
    )
    .await;
    expect_error(
        &mut c,
        ClientCommand::DeleteProvider {
            id: ProviderId::CHATGPT,
        },
        "不能刪除",
    )
    .await;
    expect_error(
        &mut c,
        ClientCommand::DeleteProvider {
            id: ProviderId::new(),
        },
        "找不到",
    )
    .await;
    expect_error(
        &mut c,
        ClientCommand::SetActiveProvider {
            id: ProviderId::new(),
        },
        "找不到",
    )
    .await;

    // 已存在的 provider 不能換種類。
    c.send(&ClientCommand::SaveProvider {
        draft: openai_draft(None, "m", Some(KEY)),
    })
    .await;
    let id = next_providers(&mut c).await.items[1].id;
    let mut anthropic = openai_draft(Some(id), "m", None);
    anthropic.kind = ProviderKind::AnthropicCompatible;
    expect_error(
        &mut c,
        ClientCommand::SaveProvider { draft: anthropic },
        "種類",
    )
    .await;
    // 被拒絕的指令都沒有寫 Providers 事件。
    c.assert_quiet(Duration::from_millis(200)).await;
    let (_, inst) = srv.client().await.snapshot().await;
    assert_eq!(inst.providers.items.len(), 2);
    assert_eq!(inst.providers.items[1].kind, ProviderKind::OpenAiCompatible);
    srv.stop().await;
}

#[tokio::test]
async fn test_provider_replies_to_requester_only() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = launch(tmp.path(), None, [], [], FakeModelAuth::signed_out()).await;
    let mut c = srv.client().await;
    c.snapshot().await;
    c.send(&ClientCommand::SaveProvider {
        draft: openai_draft(None, "", Some(KEY)),
    })
    .await;
    let id = next_providers(&mut c).await.items[1].id;
    let mut other = Client::connect(&srv, c.last_seq).await;

    // 用已存的 key。
    c.send(&ClientCommand::TestProvider {
        request: 7,
        draft: openai_draft(Some(id), "", None),
    })
    .await;
    match c.next().await {
        ServerEvent::ProviderTest { request, result } => {
            assert_eq!(request, 7);
            assert_eq!(result.unwrap(), ScriptedApi::models());
        }
        other => panic!("預期 ProviderTest，收到 {other:?}"),
    }
    let lists = srv.api.lists();
    assert_eq!(lists.len(), 1);
    assert_eq!(lists[0].api_key, KEY);
    assert_eq!(lists[0].kind, ProviderKind::OpenAiCompatible);
    other.assert_quiet(Duration::from_millis(300)).await;

    // 草稿帶的 key 優先；錯誤訊息不含 key。
    let bad = "sk-bad-key-0000";
    c.send(&ClientCommand::TestProvider {
        request: 8,
        draft: openai_draft(Some(id), "", Some(bad)),
    })
    .await;
    match c.next().await {
        ServerEvent::ProviderTest {
            request: 8,
            result: Err(message),
        } => {
            assert!(message.contains("API key"), "{message}");
            assert!(!message.contains(bad));
        }
        other => panic!("預期失敗的 ProviderTest，收到 {other:?}"),
    }
    assert_eq!(srv.api.lists()[1].api_key, bad);

    // Anthropic 新草稿沒有 key、URL 不對、ChatGPT（這個組裝不能列）都回錯誤，不呼叫工廠。
    for (request, draft) in [
        (
            9,
            ProviderDraft {
                kind: ProviderKind::AnthropicCompatible,
                ..openai_draft(None, "", None)
            },
        ),
        (
            10,
            ProviderDraft {
                base_url: "nope".into(),
                ..openai_draft(None, "", Some(KEY))
            },
        ),
        (
            11,
            ProviderDraft {
                id: Some(ProviderId::CHATGPT),
                kind: ProviderKind::ChatGpt,
                ..openai_draft(None, "", None)
            },
        ),
    ] {
        c.send(&ClientCommand::TestProvider { request, draft })
            .await;
        match c.next().await {
            ServerEvent::ProviderTest { request: r, result } => {
                assert_eq!(r, request);
                assert!(result.is_err());
            }
            other => panic!("預期 ProviderTest，收到 {other:?}"),
        }
    }
    assert_eq!(srv.api.lists().len(), 2);
    // 測試不寫日誌、不廣播。
    other.assert_quiet(Duration::from_millis(300)).await;
    srv.stop().await;
}

#[tokio::test]
async fn restart_preserves_providers() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let srv = launch(dir, None, [], [], FakeModelAuth::signed_out()).await;
    let mut c = srv.client().await;
    c.snapshot().await;
    c.send(&ClientCommand::SaveProvider {
        draft: openai_draft(None, "gpt-x", Some(KEY)),
    })
    .await;
    let before = next_providers(&mut c).await;
    let id = before.items[1].id;
    drop(c);
    srv.stop().await;

    // 資料庫已經有使用中的 provider：預設值不覆蓋。
    let srv = launch(
        dir,
        Some(ProviderId::CHATGPT),
        [],
        [ScriptedTurn::reply("重啟後")],
        FakeModelAuth::signed_in(),
    )
    .await;
    let mut c = srv.client().await;
    let (_, inst) = c.snapshot().await;
    assert_eq!(inst.providers, before);
    assert_eq!(inst.providers.active, Some(id));
    // 啟動時從 key 檔讀回 key 建 provider。
    assert_eq!(srv.api.builds()[0].api_key, KEY);
    let project = c.create_project("重啟").await;
    let (_, text) = ask(&mut c, project, "嗨").await;
    assert_eq!(text, "重啟後");
    assert_eq!(srv.api.provider.requests()[0].model, "gpt-x");

    // 刻意清空的選擇也保留：重啟後不套用預設、不因為已登入選 ChatGPT。
    c.send(&ClientCommand::DeleteProvider { id }).await;
    assert_eq!(next_providers(&mut c).await.active, None);
    drop(c);
    srv.stop().await;
    let srv = launch(
        dir,
        Some(ProviderId::CHATGPT),
        [],
        [],
        FakeModelAuth::signed_in(),
    )
    .await;
    let (_, inst) = srv.client().await.snapshot().await;
    assert_eq!(inst.providers.active, None);
    assert_eq!(inst.providers.items.len(), 1);
    srv.stop().await;
}

#[tokio::test]
async fn initial_active_provider() {
    // 第一次啟動、有預設值。
    let tmp = tempfile::tempdir().unwrap();
    let srv = launch(
        tmp.path(),
        Some(ProviderId::CHATGPT),
        [],
        [],
        FakeModelAuth::signed_out(),
    )
    .await;
    let (_, inst) = srv.client().await.snapshot().await;
    assert_eq!(inst.providers.active, Some(ProviderId::CHATGPT));
    srv.stop().await;

    // 沒有預設，但 ChatGPT 已登入（升級前的使用者）：選 ChatGPT，並寫進資料庫。
    let tmp = tempfile::tempdir().unwrap();
    let srv = launch(tmp.path(), None, [], [], FakeModelAuth::signed_in()).await;
    let (_, inst) = srv.client().await.snapshot().await;
    assert_eq!(inst.providers.active, Some(ProviderId::CHATGPT));
    srv.stop().await;
    let srv = launch(tmp.path(), None, [], [], FakeModelAuth::signed_out()).await;
    let (_, inst) = srv.client().await.snapshot().await;
    assert_eq!(inst.providers.active, Some(ProviderId::CHATGPT));
    srv.stop().await;
}

/// OpenAI 相容（例如本機的 Ollama）可以不填 key：沒有 key 檔，provider 用空字串建立。
#[tokio::test]
async fn openai_compatible_without_key() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let srv = launch(
        dir,
        None,
        [],
        [ScriptedTurn::reply("本機模型")],
        FakeModelAuth::signed_out(),
    )
    .await;
    let mut c = srv.client().await;
    c.snapshot().await;

    // 測試：沒有 key 也呼叫工廠（空字串）。
    let local = |id| ProviderDraft {
        name: "Ollama".into(),
        base_url: "http://localhost:11434/v1".into(),
        ..openai_draft(id, "llama3", None)
    };
    c.send(&ClientCommand::TestProvider {
        request: 1,
        draft: local(None),
    })
    .await;
    assert!(matches!(
        c.next().await,
        ServerEvent::ProviderTest {
            request: 1,
            result: Ok(_)
        }
    ));
    assert_eq!(srv.api.lists()[0].api_key, "");

    c.send(&ClientCommand::SaveProvider { draft: local(None) })
        .await;
    let view = next_providers(&mut c).await;
    let p = view.items[1].clone();
    assert_eq!(view.active, Some(p.id));
    assert_eq!(p.key_hint, None);
    assert!(!key_path(dir, p.id).exists());
    assert_eq!(srv.api.builds()[0].api_key, "");

    let project = c.create_project("本機").await;
    let (_, text) = ask(&mut c, project, "嗨").await;
    assert_eq!(text, "本機模型");
    assert_eq!(srv.api.provider.requests()[0].model, "llama3");

    // 已存的 provider 沒有 key：測試仍用空字串。
    c.send(&ClientCommand::TestProvider {
        request: 2,
        draft: local(Some(p.id)),
    })
    .await;
    assert!(matches!(
        c.next().await,
        ServerEvent::ProviderTest {
            request: 2,
            result: Ok(_)
        }
    ));
    assert_eq!(srv.api.lists()[1].api_key, "");

    // 之後補上 key：寫 key 檔。
    c.send(&ClientCommand::SaveProvider {
        draft: ProviderDraft {
            api_key: Some(KEY.into()),
            ..local(Some(p.id))
        },
    })
    .await;
    let view = next_providers(&mut c).await;
    assert_eq!(view.items[1].key_hint.as_deref(), Some("…1234"));
    assert!(key_path(dir, p.id).exists());
    drop(c);
    srv.stop().await;

    // 重啟後從 key 檔讀回補上的 key。
    let srv = launch(dir, None, [], [], FakeModelAuth::signed_out()).await;
    assert_eq!(srv.api.builds()[0].api_key, KEY);
    srv.stop().await;
}

#[tokio::test]
async fn keyless_provider_survives_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let srv = launch(dir, None, [], [], FakeModelAuth::signed_out()).await;
    let mut c = srv.client().await;
    c.snapshot().await;
    c.send(&ClientCommand::SaveProvider {
        draft: openai_draft(None, "llama3", None),
    })
    .await;
    let before = next_providers(&mut c).await;
    drop(c);
    srv.stop().await;

    let srv = launch(
        dir,
        None,
        [],
        [ScriptedTurn::reply("還在")],
        FakeModelAuth::signed_out(),
    )
    .await;
    let mut c = srv.client().await;
    let (_, inst) = c.snapshot().await;
    assert_eq!(inst.providers, before);
    assert_eq!(srv.api.builds()[0].api_key, "");
    let project = c.create_project("重啟").await;
    let (status, text) = ask(&mut c, project, "嗨").await;
    assert_eq!((status, text.as_str()), (MsgStatus::Done, "還在"));
    srv.stop().await;
}

/// 改了 base URL 的主機卻沒有重新輸入 key：不把已存的 key 送到新主機。
#[tokio::test]
async fn stored_key_stays_with_its_origin() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = launch(tmp.path(), None, [], [], FakeModelAuth::signed_out()).await;
    let mut c = srv.client().await;
    c.snapshot().await;
    c.send(&ClientCommand::SaveProvider {
        draft: openai_draft(None, "gpt-x", Some(KEY)),
    })
    .await;
    let id = next_providers(&mut c).await.items[1].id;
    let moved = |key: Option<&str>| ProviderDraft {
        base_url: "https://evil.example.com/v1".into(),
        ..openai_draft(Some(id), "gpt-x", key)
    };

    // TestProvider：拒絕，且沒有呼叫工廠。
    c.send(&ClientCommand::TestProvider {
        request: 1,
        draft: moved(None),
    })
    .await;
    match c.next().await {
        ServerEvent::ProviderTest {
            request: 1,
            result: Err(message),
        } => assert!(message.contains("換了主機"), "{message}"),
        other => panic!("預期失敗的 ProviderTest，收到 {other:?}"),
    }
    assert!(srv.api.lists().is_empty());

    // SaveProvider：拒絕。
    expect_error(
        &mut c,
        ClientCommand::SaveProvider { draft: moved(None) },
        "換了主機",
    )
    .await;
    assert_eq!(srv.api.builds().len(), 1);

    // 同一個 origin 換路徑：沿用 key。
    let same_host = ProviderDraft {
        base_url: "https://openrouter.ai:443/api/v2".into(),
        ..openai_draft(Some(id), "gpt-x", None)
    };
    c.send(&ClientCommand::TestProvider {
        request: 2,
        draft: same_host.clone(),
    })
    .await;
    assert!(matches!(
        c.next().await,
        ServerEvent::ProviderTest {
            request: 2,
            result: Ok(_)
        }
    ));
    assert_eq!(srv.api.lists()[0].api_key, KEY);
    c.send(&ClientCommand::SaveProvider { draft: same_host })
        .await;
    next_providers(&mut c).await;
    assert_eq!(srv.api.builds().last().unwrap().api_key, KEY);

    // 換主機並重新輸入 key：可以，用新的 key。
    let new_key = "sk-evil-host-key-5678";
    c.send(&ClientCommand::SaveProvider {
        draft: moved(Some(new_key)),
    })
    .await;
    let view = next_providers(&mut c).await;
    assert_eq!(view.items[1].base_url, "https://evil.example.com/v1");
    assert_eq!(view.items[1].key_hint.as_deref(), Some("…5678"));
    let call = srv.api.builds().last().unwrap().clone();
    assert_eq!(call.api_key, new_key);
    srv.stop().await;
}

/// 新 key 寫不進去時，原本的 key 檔與設定都不變。
#[tokio::test]
async fn failed_key_write_keeps_old_key() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let srv = launch(dir, None, [], [], FakeModelAuth::signed_out()).await;
    let mut c = srv.client().await;
    c.snapshot().await;
    c.send(&ClientCommand::SaveProvider {
        draft: openai_draft(None, "gpt-x", Some(KEY)),
    })
    .await;
    let before = next_providers(&mut c).await;
    let id = before.items[1].id;
    let path = key_path(dir, id);
    let old = std::fs::read(&path).unwrap();

    // 暫存檔的位置被目錄佔住：暫存失敗。
    let staged = path.with_extension("json.new");
    std::fs::create_dir(&staged).unwrap();
    expect_error(
        &mut c,
        ClientCommand::SaveProvider {
            draft: openai_draft(Some(id), "gpt-y", Some("sk-replacement-key-0000")),
        },
        "API key",
    )
    .await;
    assert_eq!(std::fs::read(&path).unwrap(), old);
    let (_, inst) = srv.client().await.snapshot().await;
    assert_eq!(inst.providers, before);
    srv.stop().await;
}

/// 太短的 key 只顯示「有 key」，不露出末四碼。
#[tokio::test]
async fn short_key_hint() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = launch(tmp.path(), None, [], [], FakeModelAuth::signed_out()).await;
    let mut c = srv.client().await;
    c.snapshot().await;
    c.send(&ClientCommand::SaveProvider {
        draft: openai_draft(None, "m", Some("sk-short-12")),
    })
    .await;
    let view = next_providers(&mut c).await;
    assert_eq!(view.items[1].key_hint.as_deref(), Some("…"));
    srv.stop().await;
}

/// run 開始時決定 provider；中途切換不影響這個 run 的後續輪次。
#[tokio::test]
async fn run_pins_its_provider() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = launch(
        tmp.path(),
        Some(ProviderId::CHATGPT),
        [
            ScriptedTurn::new()
                .text("查一下")
                .delay(Duration::from_millis(500))
                .tool_call("c1", "get_status", "{}"),
            ScriptedTurn::reply("ChatGPT 第二輪"),
        ],
        [ScriptedTurn::reply("下一個 run 用 API")],
        FakeModelAuth::signed_in(),
    )
    .await;
    let mut c = srv.client().await;
    c.snapshot().await;
    c.send(&ClientCommand::SaveProvider {
        draft: openai_draft(None, "gpt-x", Some(KEY)),
    })
    .await;
    let view = next_providers(&mut c).await;
    // 已經有使用中的 ChatGPT，新增的不會自動變成使用中。
    assert_eq!(view.active, Some(ProviderId::CHATGPT));
    let api = view.items[1].id;

    let project = c.create_project("固定").await;
    c.say(project, "開始", SendMode::Queue).await;
    c.until(|e| matches!(e, ServerEvent::Delta { .. })).await;
    c.send(&ClientCommand::SetActiveProvider { id: api }).await;
    let evs = c.until_idle(project).await;
    assert!(evs.iter().any(
        |e| matches!(e, ServerEvent::Providers { providers, .. } if providers.active == Some(api))
    ));
    let reqs = srv.provider.requests();
    assert_eq!(reqs.len(), 2, "第二輪仍走 ChatGPT");
    assert!(reqs.iter().all(|r| r.model == "test-model"));
    assert!(srv.api.provider.requests().is_empty());

    let (_, text) = ask(&mut c, project, "再一次").await;
    assert_eq!(text, "下一個 run 用 API");
    assert_eq!(srv.api.provider.requests()[0].model, "gpt-x");
    srv.stop().await;
}

/// API provider 回 NeedsLogin（理論上不會）不會把 ChatGPT 的授權狀態改成錯誤。
#[tokio::test]
async fn api_failure_does_not_touch_chatgpt_auth() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = launch(
        tmp.path(),
        None,
        [],
        [pots_llm::testing::ScriptedTurn::reject(
            pots_llm::LlmError::NeedsLogin,
        )],
        FakeModelAuth::signed_in(),
    )
    .await;
    let mut c = srv.client().await;
    c.snapshot().await;
    c.send(&ClientCommand::SaveProvider {
        draft: openai_draft(None, "gpt-x", Some(KEY)),
    })
    .await;
    let api = next_providers(&mut c).await.items[1].id;
    c.send(&ClientCommand::SetActiveProvider { id: api }).await;
    next_providers(&mut c).await;
    let project = c.create_project("API").await;
    c.say(project, "嗨", SendMode::Queue).await;
    let evs = c.until_idle(project).await;
    assert!(
        !evs.iter()
            .any(|e| matches!(e, ServerEvent::ModelAuth { .. })),
        "{evs:?}"
    );
    srv.stop().await;
}

/// 資料庫裡有不認得種類的 provider（例如較新版本寫的）：略過，服務照常啟動。
#[tokio::test]
async fn unknown_provider_kind_is_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let srv = launch(dir, None, [], [], FakeModelAuth::signed_out()).await;
    let mut c = srv.client().await;
    c.snapshot().await;
    c.send(&ClientCommand::SaveProvider {
        draft: openai_draft(None, "gpt-x", Some(KEY)),
    })
    .await;
    let before = next_providers(&mut c).await;
    drop(c);
    srv.stop().await;

    {
        use sqlx::Connection;
        let url = format!("sqlite://{}", dir.join("pots.db").display());
        let mut db = sqlx::SqliteConnection::connect(&url).await.unwrap();
        sqlx::query(
            "INSERT INTO model_connections \
             (id, user_id, kind, name, base_url, model, secret_ref, created_at, updated_at) \
             VALUES (?, ?, 'FutureKind', '未來', 'https://x.example', '', NULL, 0, 0)",
        )
        .bind(ProviderId::new().to_string())
        .bind(uuid::Uuid::from_u64_pair(0, 1).to_string())
        .execute(&mut db)
        .await
        .unwrap();
        db.close().await.unwrap();
    }

    let srv = launch(dir, None, [], [], FakeModelAuth::signed_out()).await;
    let (_, inst) = srv.client().await.snapshot().await;
    assert_eq!(inst.providers, before);
    srv.stop().await;
}

#[tokio::test]
async fn provider_headers_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let srv = launch(
        dir,
        None,
        [],
        [ScriptedTurn::reply("好")],
        FakeModelAuth::signed_out(),
    )
    .await;
    let mut c = srv.client().await;
    c.snapshot().await;

    let headers = vec![
        ProviderHeader {
            name: "x-opencode-client".into(),
            value: "pots".into(),
        },
        ProviderHeader {
            name: "X-Test".into(),
            value: "yes".into(),
        },
    ];
    let mut draft = openai_draft(None, "gpt-x", Some(KEY));
    draft.headers = headers.clone();
    c.send(&ClientCommand::SaveProvider { draft }).await;
    let view = next_providers(&mut c).await;
    let api = &view.items[1];
    assert_eq!(api.headers, headers);
    assert_eq!(srv.api.builds().last().unwrap().headers, headers);

    drop(c);
    srv.stop().await;

    // 重啟後 headers 從資料庫讀回，建立 provider 時仍然帶著。
    let srv = launch(dir, None, [], [], FakeModelAuth::signed_out()).await;
    let mut c = srv.client().await;
    let (_, snap) = c.snapshot().await;
    assert_eq!(snap.providers.items[1].headers, headers);
    assert_eq!(srv.api.builds().last().unwrap().headers, headers);
}

#[tokio::test]
async fn opencode_preset_uses_catalog_and_headers() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = launch(
        tmp.path(),
        None,
        [],
        [ScriptedTurn::reply("好")],
        FakeModelAuth::signed_out(),
    )
    .await;
    let mut c = srv.client().await;
    c.snapshot().await;

    // 測試連線：preset 直接回內建目錄，不呼叫工廠、不連網。
    let preset = ProviderKind::OpenCodeGo;
    let draft = ProviderDraft {
        id: None,
        kind: preset,
        name: "OpenCode Go".into(),
        base_url: preset.default_base_url().into(),
        model: String::new(),
        api_key: Some(KEY.into()),
        headers: vec![ProviderHeader {
            name: "x-opencode-client".into(),
            value: "pots".into(),
        }],
    };
    c.send(&ClientCommand::TestProvider {
        request: 1,
        draft: draft.clone(),
    })
    .await;
    let models = match c.next().await {
        ServerEvent::ProviderTest { result, .. } => result.unwrap(),
        other => panic!("預期 ProviderTest，收到 {other:?}"),
    };
    assert!(models.iter().any(|m| m.id == "claude-haiku-5-5"));
    assert!(srv.api.lists().is_empty());

    // 不在目錄裡的模型不能存。
    let mut unknown = draft.clone();
    unknown.model = "not-in-the-catalog".into();
    expect_error(
        &mut c,
        ClientCommand::SaveProvider { draft: unknown },
        "內建模型清單",
    )
    .await;

    // 目錄裡的模型可以存；建立 provider 時拿到 preset kind 與 header。
    let mut valid = draft;
    valid.model = "claude-haiku-5-5".into();
    c.send(&ClientCommand::SaveProvider { draft: valid }).await;
    let view = next_providers(&mut c).await;
    let api = &view.items[1];
    assert_eq!(api.kind, ProviderKind::OpenCodeGo);
    assert_eq!(api.model, "claude-haiku-5-5");
    assert_eq!(api.headers[0].name, "x-opencode-client");
    let builds = srv.api.builds();
    let build = builds.last().unwrap();
    assert_eq!(build.kind, ProviderKind::OpenCodeGo);
    assert_eq!(build.headers[0].value, "pots");
}
