//! project 的工作資料夾（`proposal/onboarding.md`）：onboarding 精靈送出的建立指令、設定與清除，
//! 以及對話 agent 的 `set_workdir`。

mod common;

use std::path::Path;
use std::sync::Arc;

use common::{Client, TestServer};
use pots_llm::testing::{FnProvider, ScriptedTurn};
use pots_llm::{Item, ModelRequest};
use pots_proto::{
    ArtifactKind, ClientCommand, ProjectId, SendMode, ServerEvent, StreamItemKind, WorkDir,
};
use pots_server::{Config, Deps, FakeModelAuth, WatcherConfig};

fn instructions(req: &ModelRequest) -> &str {
    req.instructions.as_deref().unwrap_or_default()
}

fn tool_results(req: &ModelRequest) -> usize {
    req.input
        .iter()
        .filter(|i| matches!(i, Item::ToolResult { .. }))
        .count()
}

/// 一個有 commit 的本機 git repo。
fn local_repo(dir: &Path) -> String {
    std::fs::create_dir_all(dir).unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    git(&["init", "--quiet", "-b", "main"]);
    std::fs::write(dir.join("README.md"), "base\n").unwrap();
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@example.com",
        "commit",
        "--quiet",
        "-m",
        "base",
    ]);
    dir.canonicalize().unwrap().display().to_string()
}

async fn project_added(c: &mut Client) -> (ProjectId, Option<WorkDir>) {
    let evs = c
        .until(|e| matches!(e, ServerEvent::ProjectAdded { .. }))
        .await;
    match evs.last() {
        Some(ServerEvent::ProjectAdded { project, .. }) => (project.id, project.workdir.clone()),
        _ => unreachable!(),
    }
}

fn server(dir: &Path) -> Config {
    let mut config = Config::desktop(dir.join("data"));
    config.watcher = WatcherConfig {
        poll_interval: std::time::Duration::from_millis(100),
        sweep_interval: std::time::Duration::from_millis(100),
        ..WatcherConfig::default()
    };
    config
}

/// 建立時就帶工作資料夾（精靈的路徑）；留白也可以。設定錯誤不會留下半個 project。
#[tokio::test]
async fn onboarding_sets_the_workdir_before_the_project_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let repo = local_repo(&work.path().join("app"));
    let srv = TestServer::start(tmp.path(), Vec::new()).await;
    let mut c = srv.client().await;
    c.snapshot().await;

    c.send(&ClientCommand::CreateProject {
        name: "app".into(),
        workdir: Some(WorkDir::Local { path: repo.clone() }),
    })
    .await;
    let (project, dir) = project_added(&mut c).await;
    assert_eq!(dir, Some(WorkDir::Local { path: repo.clone() }));

    // 路徑不存在：錯誤，而且沒有多出一個 project。
    c.send(&ClientCommand::CreateProject {
        name: "missing".into(),
        workdir: Some(WorkDir::Local {
            path: tmp.path().join("nope").display().to_string(),
        }),
    })
    .await;
    c.until(|e| matches!(e, ServerEvent::Error { message } if message.contains("找不到")))
        .await;
    c.assert_quiet(std::time::Duration::from_millis(300)).await;

    // 沒有 git 的資料夾也不行。
    let plain = tmp.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    c.send(&ClientCommand::CreateProject {
        name: "plain".into(),
        workdir: Some(WorkDir::Local {
            path: plain.display().to_string(),
        }),
    })
    .await;
    c.until(|e| matches!(e, ServerEvent::Error { message } if message.contains("不是 git repo")))
        .await;

    let mut fresh = srv.client().await;
    let (_, snapshot) = fresh.snapshot().await;
    assert_eq!(snapshot.projects.len(), 1);
    assert_eq!(snapshot.projects[0].id, project);

    // 之後換成 GitHub（正規化）再清空。
    c.send(&ClientCommand::SetProjectWorkdir {
        project,
        workdir: Some(WorkDir::Github {
            repo: "https://github.com/Me/App.git".into(),
        }),
    })
    .await;
    let evs = c
        .until(|e| matches!(e, ServerEvent::ProjectWorkdir { .. }))
        .await;
    match evs.last() {
        Some(ServerEvent::ProjectWorkdir { workdir, .. }) => assert_eq!(
            workdir,
            &Some(WorkDir::Github {
                repo: "github.com/me/app".into()
            })
        ),
        _ => unreachable!(),
    }
    c.send(&ClientCommand::SetProjectWorkdir {
        project,
        workdir: None,
    })
    .await;
    let evs = c
        .until(|e| matches!(e, ServerEvent::ProjectWorkdir { .. }))
        .await;
    assert!(matches!(
        evs.last(),
        Some(ServerEvent::ProjectWorkdir { workdir: None, .. })
    ));
    srv.stop().await;
}

/// 使用者說「我們用這個資料夾」：對話 agent 呼叫 `set_workdir`，之後的 instructions 帶著它。
#[tokio::test]
async fn the_chat_agent_can_point_the_project_at_a_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let repo = local_repo(&work.path().join("app"));
    let expected = repo.clone();
    let model = FnProvider::new("m", move |req| {
        let instr = instructions(req);
        match tool_results(req) {
            0 => {
                assert!(instr.contains("# 工作資料夾"), "{instr}");
                assert!(instr.contains("還沒設定"), "{instr}");
                ScriptedTurn::new().tool_call(
                    "t0",
                    "set_workdir",
                    serde_json::json!({"kind": "local", "path": expected}).to_string(),
                )
            }
            // 同一輪的 instructions 是 run 開始時的快照；下一個 run 才會帶新的資料夾。
            _ if !instr.contains("本機資料夾 /") => ScriptedTurn::reply("設好了"),
            _ => {
                assert!(!instr.contains("local:"), "instructions 不該有任務用的 key");
                ScriptedTurn::reply("這是新的工作資料夾")
            }
        }
    });
    let auth = Arc::new(FakeModelAuth::signed_in());
    let deps = Deps::new(model, "test-model", auth);
    let handle = pots_server::start(server(tmp.path()), deps)
        .await
        .expect("服務啟動失敗");
    let srv = TestServer {
        handle,
        provider: pots_llm::testing::ScriptedProvider::new([]),
        api: common::ScriptedApi::new([]),
        auth: Arc::new(FakeModelAuth::signed_in()),
    };
    let mut c = srv.client().await;
    c.snapshot().await;
    let project = c.create_project("app").await;

    c.say(project, "我們用這個資料夾", SendMode::Queue).await;
    let evs = c
        .until(|e| matches!(e, ServerEvent::ProjectWorkdir { project: p, .. } if *p == project))
        .await;
    match evs.last() {
        Some(ServerEvent::ProjectWorkdir { workdir, .. }) => {
            assert_eq!(workdir, &Some(WorkDir::Local { path: repo.clone() }))
        }
        _ => unreachable!(),
    }
    c.until_idle(project).await;

    // 下一個 run 的 instructions 已經是新資料夾（上面的假模型會檢查）。
    c.say(project, "再確認一次", SendMode::Queue).await;
    let evs = c
        .until(|e| {
            matches!(e, ServerEvent::ItemDone { item, .. }
            if matches!(&item.kind, StreamItemKind::Agent(m) if m.text.contains("新的工作資料夾")))
        })
        .await;
    assert!(
        !evs.iter()
            .any(|e| matches!(e, ServerEvent::ProjectWorkdir { .. }))
    );
    srv.stop().await;
}

/// 監看的 repo 和工作的資料夾是兩件事：設定工作資料夾不會改到自動化分頁的清單。
#[tokio::test]
async fn watched_repos_stay_separate_from_the_workdir() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let repo = local_repo(&work.path().join("app"));
    let srv = TestServer::start(tmp.path(), Vec::new()).await;
    let mut c = srv.client().await;
    c.snapshot().await;
    let project = c.create_project("app").await;

    c.send(&ClientCommand::SetProjectRepos {
        project,
        repos: vec!["me/app".into()],
    })
    .await;
    c.until(|e| matches!(e, ServerEvent::ProjectRepos { .. }))
        .await;
    c.send(&ClientCommand::SetProjectWorkdir {
        project,
        workdir: Some(WorkDir::Local { path: repo }),
    })
    .await;
    c.until(|e| matches!(e, ServerEvent::ProjectWorkdir { .. }))
        .await;

    let mut fresh = srv.client().await;
    let (_, snap) = fresh.snapshot().await;
    let p = &snap.projects[0];
    assert_eq!(p.repos, vec!["github.com/me/app".to_string()]);
    assert!(matches!(p.workdir, Some(WorkDir::Local { .. })));
    // Project Notes 也還在。
    assert!(
        p.artifacts
            .iter()
            .any(|a| a.kind == ArtifactKind::ProjectNotes)
    );
    srv.stop().await;
}
