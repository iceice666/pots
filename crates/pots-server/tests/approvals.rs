//! M5 核准與通知的整合測試（`proposal/approvals-and-notifications.md`）。
//!
//! - GitHub API 換成假的伺服器：PR 的 head 從本機的 bare remote 讀；可以 merge（帶 sha 檢查）與刪分支；
//!   CI 可以設成一律失敗。git remote 換成本機的 bare repo，worker 在本機執行（內建 worker）。
//! - 模型換成依請求內容回應的假 provider：對話 agent 執行使用者訊息裡的 `CALL` 行；Planner 與 worker
//!   各自依觸發訊息決定。

mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, put};
use common::{Client, ScriptedApi, TestServer};
use pots_llm::testing::{FnProvider, ScriptedTurn};
use pots_llm::{Item, ModelRequest};
use pots_proto::{
    ApprovalActionView, ApprovalId, ApprovalState, ApprovalView, ArtifactKind, Card, ClientCommand,
    ExecKind, GithubDraft, GoalState, GoalView, ListenerState, MergeMethod, NotificationSettings,
    ProjectId, ProviderId, SendMode, ServerEvent, StreamItemKind, TaskStateView, WorkerBackendKind,
    WorkerSettingsDraft,
};
use pots_server::{Config, Deps, FakeModelAuth, ManualClock, WatcherConfig};
use serde_json::{Value, json};

const PLANNER: &str = "你是 Pots 的 Planner";
const WORKER: &str = "你是 Pots 的 coding worker";
const TOKEN: &str = "ghp_test_token_1234";

// ---------------------------------------------------------------------------
// 假的 GitHub API

#[derive(Default)]
struct GhState {
    remotes: PathBuf,
    /// `owner/repo` → 開過的 PR（POST 的 body 加上編號）。
    pulls: HashMap<String, Vec<Value>>,
    /// `owner/repo#n` → merge 的 body。
    merged: HashMap<String, Value>,
    /// `owner/repo#n` → 假裝 PR 之後又被 push 了這個 head。
    head_override: HashMap<String, String>,
    deleted: Vec<String>,
    ci_fail: bool,
}

#[derive(Clone)]
struct FakeGithub {
    addr: SocketAddr,
    state: Arc<Mutex<GhState>>,
}

type Gh = State<Arc<Mutex<GhState>>>;

impl FakeGithub {
    async fn start(remotes: &Path) -> Self {
        let state = Arc::new(Mutex::new(GhState {
            remotes: remotes.to_path_buf(),
            ..Default::default()
        }));
        let app = Router::new()
            .route(
                "/user",
                get(|| async { axum::Json(json!({"login": "me"})) }),
            )
            .route(
                "/repos/{o}/{r}/events",
                get(|| async { ([("etag", "\"0\"")], axum::Json(json!([]))).into_response() }),
            )
            .route(
                "/repos/{o}/{r}/commits/{git_ref}/check-suites",
                get(gh_suites),
            )
            .route(
                "/repos/{o}/{r}/pulls",
                get(gh_list_pulls).post(gh_open_pull),
            )
            .route("/repos/{o}/{r}/pulls/{n}", get(gh_pull))
            .route("/repos/{o}/{r}/pulls/{n}/merge", put(gh_merge))
            .route(
                "/repos/{o}/{r}/git/refs/heads/{*branch}",
                delete(gh_delete_ref),
            )
            .route(
                "/repos/{o}/{r}",
                get(|UrlPath((o, r)): UrlPath<(String, String)>| async move {
                    axum::Json(json!({"full_name": format!("{o}/{r}"), "default_branch": "main"}))
                }),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { addr, state }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

async fn gh_suites(
    State(s): Gh,
    UrlPath((_o, _r, git_ref)): UrlPath<(String, String, String)>,
) -> Response {
    let fail = s.lock().unwrap().ci_fail;
    axum::Json(json!({
        "total_count": 1,
        "check_suites": [{
            "head_sha": git_ref, "head_branch": "x", "status": "completed",
            "conclusion": if fail { "failure" } else { "success" },
            "latest_check_runs_count": 1, "updated_at": "2026-11-08T01:00:00Z",
        }],
    }))
    .into_response()
}

async fn gh_list_pulls(
    State(s): Gh,
    UrlPath((o, r)): UrlPath<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let head = q.get("head").cloned().unwrap_or_default();
    let found: Vec<Value> = s
        .lock()
        .unwrap()
        .pulls
        .get(&format!("{o}/{r}"))
        .into_iter()
        .flatten()
        .filter(|p| format!("{o}:{}", p["head"].as_str().unwrap_or_default()) == head)
        .cloned()
        .collect();
    axum::Json(Value::Array(found)).into_response()
}

async fn gh_open_pull(
    State(s): Gh,
    UrlPath((o, r)): UrlPath<(String, String)>,
    axum::Json(mut body): axum::Json<Value>,
) -> Response {
    let mut st = s.lock().unwrap();
    let list = st.pulls.entry(format!("{o}/{r}")).or_default();
    let n = 40 + list.len() as u64;
    body["number"] = json!(n);
    body["html_url"] = json!(format!("https://github.com/{o}/{r}/pull/{n}"));
    list.push(body.clone());
    (StatusCode::CREATED, axum::Json(body)).into_response()
}

/// PR 現在的 head：被覆寫過的，或 remote 上那個分支的 commit。
fn head_of(st: &GhState, o: &str, r: &str, n: u64, branch: &str) -> String {
    if let Some(h) = st.head_override.get(&format!("{o}/{r}#{n}")) {
        return h.clone();
    }
    git(
        &st.remotes.join(format!("{o}/{r}.git")),
        &["rev-parse", branch],
    )
    .trim()
    .to_string()
}

async fn gh_pull(State(s): Gh, UrlPath((o, r, n)): UrlPath<(String, String, u64)>) -> Response {
    let st = s.lock().unwrap();
    let Some(p) = st
        .pulls
        .get(&format!("{o}/{r}"))
        .and_then(|l| l.iter().find(|p| p["number"] == json!(n)).cloned())
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let branch = p["head"].as_str().unwrap_or_default().to_string();
    let merged = st.merged.contains_key(&format!("{o}/{r}#{n}"));
    let sha = head_of(&st, &o, &r, n, &branch);
    axum::Json(json!({
        "number": n,
        "title": p["title"],
        "html_url": p["html_url"],
        "state": if merged { "closed" } else { "open" },
        "merged": merged,
        "head": {"ref": branch, "sha": sha},
        "base": {"ref": p["base"]},
    }))
    .into_response()
}

async fn gh_merge(
    State(s): Gh,
    UrlPath((o, r, n)): UrlPath<(String, String, u64)>,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    let mut st = s.lock().unwrap();
    let Some(p) = st
        .pulls
        .get(&format!("{o}/{r}"))
        .and_then(|l| l.iter().find(|p| p["number"] == json!(n)).cloned())
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let branch = p["head"].as_str().unwrap_or_default().to_string();
    if body["sha"].as_str() != Some(head_of(&st, &o, &r, n, &branch).as_str()) {
        return (
            StatusCode::CONFLICT,
            axum::Json(
                json!({"message": "Head branch was modified. Review and try the merge again."}),
            ),
        )
            .into_response();
    }
    st.merged.insert(format!("{o}/{r}#{n}"), body);
    axum::Json(json!({"sha": "e".repeat(40), "merged": true, "message": "merged"})).into_response()
}

async fn gh_delete_ref(
    State(s): Gh,
    UrlPath((_o, _r, branch)): UrlPath<(String, String, String)>,
) -> Response {
    s.lock().unwrap().deleted.push(branch);
    StatusCode::NO_CONTENT.into_response()
}

// ---------------------------------------------------------------------------
// git 與服務

fn git(dir: &Path, args: &[&str]) -> String {
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
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn make_remote(root: &Path) -> PathBuf {
    let remotes = root.join("remotes");
    let remote = remotes.join("me/app.git");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "--quiet", "--bare", "-b", "main"]);
    let seed = root.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "--quiet", "-b", "main"]);
    std::fs::write(seed.join("README.md"), "hello pots\n").unwrap();
    git(&seed, &["add", "."]);
    git(
        &seed,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "--quiet",
            "-m",
            "init",
        ],
    );
    git(
        &seed,
        &["push", "--quiet", remote.to_str().unwrap(), "main"],
    );
    remotes
}

struct Env {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    remotes: PathBuf,
    gh: FakeGithub,
    clock: ManualClock,
}

async fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let remotes = make_remote(&dir);
    let gh = FakeGithub::start(&remotes).await;
    Env {
        _tmp: tmp,
        dir,
        remotes,
        gh,
        clock: ManualClock::new(pots_proto::Timestamp::now()),
    }
}

fn config(env: &Env) -> Config {
    let mut config =
        Config::desktop(env.dir.join("data")).with_default_provider(ProviderId::CHATGPT);
    config.watcher = WatcherConfig {
        poll_interval: Duration::from_millis(100),
        sweep_interval: Duration::from_millis(100),
        github_api: env.gh.url(),
        clock: env.clock.clock(),
    };
    config.worker.git_base = Some(format!("file://{}", env.remotes.display()));
    config.worker.defaults = WorkerSettingsDraft {
        backend: WorkerBackendKind::Builtin,
        exec: ExecKind::Local,
        runtime: "docker".into(),
        image: "pots-worker:latest".into(),
        claude_model: String::new(),
        max_parallel: 2,
    };
    config
}

async fn launch(config: Config, model: Arc<FnProvider>) -> TestServer {
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

async fn setup(c: &mut Client) -> ProjectId {
    c.snapshot().await;
    let project = c.create_project("app").await;
    c.send(&ClientCommand::SaveGithub {
        draft: GithubDraft {
            token: Some(TOKEN.into()),
            api_base: None,
            webhook_secret: None,
        },
    })
    .await;
    c.until(|e| matches!(e, ServerEvent::Github { github, .. } if github.token_hint.is_some()))
        .await;
    c.send(&ClientCommand::SetProjectRepos {
        project,
        repos: vec!["me/app".into()],
    })
    .await;
    c.until(|e| matches!(e, ServerEvent::ProjectRepos { .. }))
        .await;
    c.send(&ClientCommand::SetProjectWorkdir {
        project,
        workdir: Some(pots_proto::WorkDir::Github {
            repo: "me/app".into(),
        }),
    })
    .await;
    c.until(|e| matches!(e, ServerEvent::ProjectWorkdir { .. }))
        .await;
    project
}

// ---------------------------------------------------------------------------
// 假模型

fn instructions(req: &ModelRequest) -> &str {
    req.instructions.as_deref().unwrap_or_default()
}

fn first_user(req: &ModelRequest) -> String {
    match req.input.first() {
        Some(Item::User { text }) => text.clone(),
        _ => String::new(),
    }
}

fn tool_outputs(req: &ModelRequest) -> Vec<String> {
    req.input
        .iter()
        .filter_map(|i| match i {
            Item::ToolResult { output, .. } => Some(output.clone()),
            _ => None,
        })
        .collect()
}

/// 對話 agent：依序執行最後一則使用者訊息裡的 `CALL <工具> <JSON>`。
fn chat_turn(req: &ModelRequest) -> ScriptedTurn {
    let Some(at) = req
        .input
        .iter()
        .rposition(|i| matches!(i, Item::User { .. }))
    else {
        return ScriptedTurn::reply("完成");
    };
    let Item::User { text } = &req.input[at] else {
        unreachable!()
    };
    let done = req.input[at..]
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

/// Planner：開 Goal 時派一個任務，任務完成時結束 Goal。
fn planner_turn(req: &ModelRequest) -> ScriptedTurn {
    let trigger = first_user(req);
    let outputs = tool_outputs(req);
    if trigger.contains("剛建立這個 Goal") && outputs.is_empty() {
        return ScriptedTurn::new().tool_call(
            "p0",
            "create_task",
            json!({"title": "Add hello", "summary": "加上 hello.txt"}).to_string(),
        );
    }
    if trigger.contains("完成：") && outputs.is_empty() {
        return ScriptedTurn::new().tool_call(
            "p1",
            "complete_goal",
            json!({"summary": "hello 加好了"}).to_string(),
        );
    }
    ScriptedTurn::reply("好")
}

/// worker 改一次檔案、commit、開 PR、等 CI。`n` 是第幾次（檔案內容不同，sha 才會變）。
fn worker_push(outputs: &[String], n: usize) -> ScriptedTurn {
    match outputs.len() {
        0 => ScriptedTurn::new().tool_call(
            "w0",
            "edit_file",
            json!({"path": "hello.txt", "new_text": format!("hi {n}\n")}).to_string(),
        ),
        1 => ScriptedTurn::new().tool_call(
            "w1",
            "run",
            json!({"command": "git add -A && git commit -qm 'Add hello'"}).to_string(),
        ),
        2 => ScriptedTurn::new().tool_call(
            "w2",
            "open_pr",
            json!({"title": "Add hello", "body": "加上 hello.txt"}).to_string(),
        ),
        3 => {
            let pr: Value = serde_json::from_str(&outputs[2]).expect("open_pr 的結果");
            ScriptedTurn::new().tool_call(
                "w3",
                "wait_for",
                json!({
                    "on": {"kind": "ci_finished", "repo": "me/app", "sha": pr["sha"]},
                    "timeout_minutes": 30,
                })
                .to_string(),
            )
        }
        _ => ScriptedTurn::reply("等 CI"),
    }
}

fn request_approval(outputs: &[String]) -> ScriptedTurn {
    match outputs.len() {
        0 => ScriptedTurn::new().tool_call(
            "a0",
            "request_approval",
            json!({"action": "merge_pr", "reason": "加上 hello.txt，CI 通過"}).to_string(),
        ),
        _ => ScriptedTurn::reply("等核准"),
    }
}

// ---------------------------------------------------------------------------
// 事件

async fn until_approval(
    c: &mut Client,
    mut pred: impl FnMut(&ApprovalView) -> bool,
) -> (ApprovalView, Vec<ServerEvent>) {
    let events = c
        .until(|e| matches!(e, ServerEvent::ApprovalChanged { approval, .. } if pred(approval)))
        .await;
    match events.last() {
        Some(ServerEvent::ApprovalChanged { approval, .. }) => ((**approval).clone(), events),
        _ => unreachable!(),
    }
}

async fn until_goal(c: &mut Client, mut pred: impl FnMut(&GoalView) -> bool) -> GoalView {
    let events = c
        .until(|e| matches!(e, ServerEvent::GoalChanged { goal, .. } if pred(goal)))
        .await;
    match events.last() {
        Some(ServerEvent::GoalChanged { goal, .. }) => goal.clone(),
        _ => unreachable!(),
    }
}

fn is_merge(a: &ApprovalView) -> bool {
    matches!(a.action, ApprovalActionView::MergePr { .. })
}

fn decide(id: ApprovalId, approve: bool, note: Option<&str>) -> ClientCommand {
    ClientCommand::Decide {
        approval: id,
        approve,
        note: note.map(str::to_string),
    }
}

/// 送出後的下一個 `Error`。
async fn until_error(c: &mut Client) -> String {
    let events = c.until(|e| matches!(e, ServerEvent::Error { .. })).await;
    match events.last() {
        Some(ServerEvent::Error { message }) => message.clone(),
        _ => unreachable!(),
    }
}

// ---------------------------------------------------------------------------

/// 一句話 → Goal → worker 開 PR、等 CI → CI 通過後申請 merge → 你核准 → 平台用 project 設定的方式
/// merge（帶核准的 sha）→ 任務完成 → Planner 結束 Goal；merge 後自動開一筆不推播的「刪除分支」，核准後刪掉。
#[tokio::test]
async fn approved_merge_completes_task_and_offers_branch_cleanup() {
    let env = env().await;
    let model = FnProvider::new("m", |req| {
        let instr = instructions(req);
        let trigger = first_user(req);
        let outputs = tool_outputs(req);
        if instr.starts_with(PLANNER) {
            return planner_turn(req);
        }
        if instr.starts_with(WORKER) {
            if trigger.contains("CI 通過") {
                assert!(trigger.contains("request_approval"), "{trigger}");
                return request_approval(&outputs);
            }
            return worker_push(&outputs, 1);
        }
        chat_turn(req)
    });
    let srv = launch(config(&env), model).await;
    let mut c = srv.client().await;
    let project = setup(&mut c).await;
    c.send(&ClientCommand::SetMergeMethod {
        project,
        method: MergeMethod::Rebase,
    })
    .await;
    c.until(|e| {
        matches!(
            e,
            ServerEvent::ProjectMergeMethod {
                method: MergeMethod::Rebase,
                ..
            }
        )
    })
    .await;

    let text = call(
        "create_goal",
        json!({"title": "加 hello", "brief_md": "## 目標\n\n加一個 hello.txt"}),
    );
    c.say(project, &text, SendMode::Queue).await;
    let (a, events) = until_approval(&mut c, |a| is_merge(a) && a.is_pending()).await;
    let ApprovalActionView::MergePr {
        pr,
        sha,
        method,
        branch,
        base,
        url,
    } = &a.action
    else {
        unreachable!()
    };
    assert_eq!(pr, "github.com/me/app#40");
    assert_eq!(*method, MergeMethod::Rebase);
    assert_eq!(base, "main");
    assert!(branch.starts_with("pots/"), "{branch}");
    assert_eq!(url.as_deref(), Some("https://github.com/me/app/pull/40"));
    let remote_sha = git(&env.remotes.join("me/app.git"), &["rev-parse", branch]);
    assert_eq!(sha, remote_sha.trim());
    assert!(a.notify);
    assert_eq!(a.action_hash.len(), 12);
    assert_eq!(a.requested_by, "worker");
    assert_eq!(a.reason, "加上 hello.txt，CI 通過");
    assert!(a.title.starts_with("merge PR #40"), "{}", a.title);
    assert_eq!(a.expires_at.0 - a.created_at.0, 72 * 3600);
    // 對話流出現核准卡片；任務在等你核准。
    let more = c
        .until(|e| {
            matches!(e, ServerEvent::Item { item, .. }
                if matches!(&item.kind, StreamItemKind::Card(Card::Approval { id }) if *id == a.id))
        })
        .await;
    let events = [events, more].concat();
    let waiting = events.iter().rev().find_map(|e| match e {
        ServerEvent::GoalChanged { goal, .. } => goal.tasks.first().cloned(),
        _ => None,
    });
    assert!(
        matches!(&waiting.unwrap().state, TaskStateView::Waiting { on } if on.starts_with("等你核准：merge PR #40")),
    );

    // 核准：平台 merge（rebase、核准的 sha），任務完成，Planner 結束 Goal。
    c.send(&decide(a.id, true, Some("看過了"))).await;
    let (executing, _) = until_approval(&mut c, |x| x.id == a.id).await;
    assert_eq!(executing.state, ApprovalState::Executing);
    assert_eq!(executing.note.as_deref(), Some("看過了"));
    let (done, _) = until_approval(&mut c, |x| {
        x.id == a.id && x.state != ApprovalState::Executing
    })
    .await;
    assert_eq!(done.state, ApprovalState::Executed, "{:?}", done.result);
    assert!(
        done.result.as_deref().unwrap().contains("merge 了 PR #40"),
        "{:?}",
        done.result
    );
    let merged = env
        .gh
        .state
        .lock()
        .unwrap()
        .merged
        .get("me/app#40")
        .cloned()
        .unwrap();
    assert_eq!(merged["sha"], json!(sha));
    assert_eq!(merged["merge_method"], json!("rebase"));

    let (cleanup, _) = until_approval(&mut c, |x| {
        matches!(x.action, ApprovalActionView::DeleteBranch { .. }) && x.is_pending()
    })
    .await;
    assert!(!cleanup.notify, "刪分支不推播");
    let g = until_goal(&mut c, |g| g.state != GoalState::Active).await;
    assert_eq!(
        g.state,
        GoalState::Done {
            summary: "hello 加好了".into()
        }
    );
    assert!(
        matches!(&g.tasks[0].state, TaskStateView::Done { outcome } if outcome.contains("merge 了 PR #40")),
        "{:?}",
        g.tasks[0].state
    );

    c.send(&decide(cleanup.id, true, None)).await;
    let (deleted, _) = until_approval(&mut c, |x| {
        x.id == cleanup.id && matches!(x.state, ApprovalState::Executed | ApprovalState::Failed)
    })
    .await;
    assert_eq!(
        deleted.state,
        ApprovalState::Executed,
        "{:?}",
        deleted.result
    );
    assert_eq!(env.gh.state.lock().unwrap().deleted, vec![branch.clone()]);

    // 已經決定過的不能再決定。
    c.send(&decide(a.id, false, None)).await;
    assert!(until_error(&mut c).await.contains("已經"));

    // 重連的快照帶著最近的核准與 project 的 merge 方式。
    let mut c2 = srv.client().await;
    let (_, snap) = c2.snapshot().await;
    assert!(
        snap.approvals
            .iter()
            .any(|x| x.id == a.id && x.state == ApprovalState::Executed)
    );
    assert_eq!(snap.projects[0].merge_method, MergeMethod::Rebase);
    srv.stop().await;
}

/// 拒絕時 worker 帶著你的理由被叫醒、改完再申請；PR 在核准前又被推了新的 commit 時平台不 merge，
/// 叫醒 worker。
#[tokio::test]
async fn rejection_and_moved_head_go_back_to_the_worker() {
    let env = env().await;
    let model = FnProvider::new("m", |req| {
        let instr = instructions(req);
        let trigger = first_user(req);
        let outputs = tool_outputs(req);
        if instr.starts_with(PLANNER) {
            return planner_turn(req);
        }
        if instr.starts_with(WORKER) {
            if trigger.contains("沒有 merge") {
                return match outputs.len() {
                    0 => ScriptedTurn::new().tool_call(
                        "e0",
                        "escalate",
                        json!({"reason": "PR 被別人推了新的 commit"}).to_string(),
                    ),
                    _ => ScriptedTurn::reply("交回"),
                };
            }
            if trigger.contains("CI 通過") {
                return request_approval(&outputs);
            }
            if trigger.contains("使用者拒絕了 merge") {
                assert!(trigger.contains("理由：請加上換行"), "{trigger}");
                return worker_push(&outputs, 2);
            }
            return worker_push(&outputs, 1);
        }
        chat_turn(req)
    });
    let srv = launch(config(&env), model).await;
    let mut c = srv.client().await;
    let project = setup(&mut c).await;
    c.say(
        project,
        &call(
            "create_goal",
            json!({"title": "加 hello", "brief_md": "## 目標\n\nhello"}),
        ),
        SendMode::Queue,
    )
    .await;
    let (first, _) = until_approval(&mut c, |a| is_merge(a) && a.is_pending()).await;
    c.send(&decide(first.id, false, Some("請加上換行"))).await;
    let (rejected, _) = until_approval(&mut c, |a| a.id == first.id).await;
    assert_eq!(rejected.state, ApprovalState::Rejected);

    // worker 改完、CI 通過後重新申請：新的 sha。
    let (second, _) = until_approval(&mut c, |a| {
        is_merge(a) && a.is_pending() && a.id != first.id
    })
    .await;
    let (ApprovalActionView::MergePr { sha: s1, .. }, ApprovalActionView::MergePr { sha: s2, .. }) =
        (&first.action, &second.action)
    else {
        unreachable!()
    };
    assert_ne!(s1, s2);

    // 核准前 PR 又被推了：平台不 merge，worker 被叫醒後交回。
    env.gh
        .state
        .lock()
        .unwrap()
        .head_override
        .insert("me/app#40".into(), "f".repeat(40));
    c.send(&decide(second.id, true, None)).await;
    let (failed, _) = until_approval(&mut c, |a| {
        a.id == second.id && matches!(a.state, ApprovalState::Failed | ApprovalState::Executed)
    })
    .await;
    assert_eq!(failed.state, ApprovalState::Failed);
    assert!(
        failed.result.as_deref().unwrap().contains("新的 commit"),
        "{:?}",
        failed.result
    );
    assert!(env.gh.state.lock().unwrap().merged.is_empty());
    let g = until_goal(&mut c, |g| {
        g.tasks
            .first()
            .is_some_and(|t| matches!(t.state, TaskStateView::NeedsHuman { .. }))
    })
    .await;
    let TaskStateView::NeedsHuman { reason } = &g.tasks[0].state else {
        unreachable!()
    };
    assert!(reason.contains("新的 commit"), "{reason}");
    srv.stop().await;
}

/// CI 失敗時平台叫醒 worker 自己修（告訴它第幾次），連續失敗 3 次轉 `NeedsHuman`。
#[tokio::test]
async fn ci_failures_are_fixed_by_the_worker_up_to_three_times() {
    let env = env().await;
    env.gh.state.lock().unwrap().ci_fail = true;
    let wakes: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = wakes.clone();
    let model = FnProvider::new("m", move |req| {
        let instr = instructions(req);
        let trigger = first_user(req);
        let outputs = tool_outputs(req);
        if instr.starts_with(PLANNER) {
            return planner_turn(req);
        }
        if instr.starts_with(WORKER) {
            if trigger.contains("CI 失敗") {
                if outputs.is_empty() {
                    seen.lock().unwrap().push(trigger.clone());
                }
                let n = seen.lock().unwrap().len() + 1;
                return worker_push(&outputs, n);
            }
            return worker_push(&outputs, 1);
        }
        chat_turn(req)
    });
    let srv = launch(config(&env), model).await;
    let mut c = srv.client().await;
    let project = setup(&mut c).await;
    c.say(
        project,
        &call(
            "create_goal",
            json!({"title": "加 hello", "brief_md": "## 目標\n\nhello"}),
        ),
        SendMode::Queue,
    )
    .await;
    let g = until_goal(&mut c, |g| {
        g.tasks
            .first()
            .is_some_and(|t| matches!(t.state, TaskStateView::NeedsHuman { .. }))
    })
    .await;
    let TaskStateView::NeedsHuman { reason } = &g.tasks[0].state else {
        unreachable!()
    };
    assert!(reason.contains("CI 連續失敗 3 次"), "{reason}");
    let wakes = wakes.lock().unwrap().clone();
    assert_eq!(wakes.len(), 2, "{wakes:?}");
    assert!(wakes[0].contains("第 1 次 CI 失敗"), "{}", wakes[0]);
    assert!(wakes[1].contains("第 2 次 CI 失敗"), "{}", wakes[1]);
    srv.stop().await;
}

/// 72 小時沒有決定：核准逾時、任務轉 `NeedsHuman`。重試後 worker 再申請；你取消任務時還沒決定的
/// 核准自動撤回。
#[tokio::test]
async fn approvals_expire_and_are_withdrawn_with_their_task() {
    let env = env().await;
    let model = FnProvider::new("m", |req| {
        let instr = instructions(req);
        let trigger = first_user(req);
        let outputs = tool_outputs(req);
        if instr.starts_with(PLANNER) {
            return planner_turn(req);
        }
        if instr.starts_with(WORKER) {
            if trigger.contains("CI 通過") || trigger.contains("使用者要求重試") {
                return request_approval(&outputs);
            }
            return worker_push(&outputs, 1);
        }
        chat_turn(req)
    });
    let srv = launch(config(&env), model).await;
    let mut c = srv.client().await;
    let project = setup(&mut c).await;
    c.say(
        project,
        &call(
            "create_goal",
            json!({"title": "加 hello", "brief_md": "## 目標\n\nhello"}),
        ),
        SendMode::Queue,
    )
    .await;
    let (first, _) = until_approval(&mut c, |a| is_merge(a) && a.is_pending()).await;
    env.clock.advance(72 * 3600 + 1);
    let (expired, _) = until_approval(&mut c, |a| a.id == first.id).await;
    assert_eq!(expired.state, ApprovalState::Expired);
    let g = until_goal(&mut c, |g| {
        g.tasks
            .first()
            .is_some_and(|t| matches!(t.state, TaskStateView::NeedsHuman { .. }))
    })
    .await;
    let task = g.tasks[0].id;
    let TaskStateView::NeedsHuman { reason } = &g.tasks[0].state else {
        unreachable!()
    };
    assert!(reason.contains("逾時"), "{reason}");
    // 逾時之後不能再核准。
    c.send(&decide(first.id, true, None)).await;
    assert!(until_error(&mut c).await.contains("已逾時"));

    c.send(&ClientCommand::RetryTask { task }).await;
    let (second, _) = until_approval(&mut c, |a| {
        is_merge(a) && a.is_pending() && a.id != first.id
    })
    .await;
    c.send(&ClientCommand::Cancel { task }).await;
    let (withdrawn, _) = until_approval(&mut c, |a| a.id == second.id).await;
    assert_eq!(withdrawn.state, ApprovalState::Cancelled);
    srv.stop().await;
}

/// agent 提案的 listener 要核准才生效；拒絕就撤掉；你刪掉它時提案撤回。
#[tokio::test]
async fn listener_proposals_need_approval() {
    let env = env().await;
    let model = FnProvider::new("m", chat_turn);
    let srv = launch(config(&env), model).await;
    let mut c = srv.client().await;
    let project = setup(&mut c).await;
    let propose = call(
        "create_listener",
        json!({"kinds": ["issue.labeled"], "fields": [{"field": "label", "any_of": ["agent"]}], "action": "create_goal", "template": "處理這個 issue"}),
    );

    c.say(project, &propose, SendMode::Queue).await;
    let (a, events) = until_approval(&mut c, |a| {
        matches!(a.action, ApprovalActionView::CreateListener { .. })
    })
    .await;
    let ApprovalActionView::CreateListener { listener, summary } = &a.action else {
        unreachable!()
    };
    assert!(summary.contains("issue"), "{summary}");
    let more = c.until_idle(project).await;
    let events = [events, more].concat();
    // 卡片附在回覆下面；自動化頁的 listener 等這筆核准。
    assert!(events.iter().any(|e| matches!(e,
        ServerEvent::ItemDone { item, .. } if matches!(&item.kind,
            StreamItemKind::Agent(m) if m.cards.iter().any(|c| matches!(c, Card::Approval { id } if *id == a.id))))));
    assert!(events.iter().any(|e| matches!(e,
        ServerEvent::Automation { automation, .. } if automation.listeners.iter().any(|l|
            l.id == *listener && l.state == ListenerState::PendingApproval(a.id)))));
    // 不能用「啟用」略過核准。
    c.send(&ClientCommand::SetListenerEnabled {
        id: *listener,
        enabled: true,
    })
    .await;
    assert!(until_error(&mut c).await.contains("等你核准"));

    c.send(&decide(a.id, true, None)).await;
    let (executed, events) = until_approval(&mut c, |x| x.id == a.id).await;
    assert_eq!(executed.state, ApprovalState::Executed);
    assert!(events.iter().any(|e| matches!(e, ServerEvent::Automation { automation, .. }
        if automation.listeners.iter().any(|l| l.id == *listener && l.state == ListenerState::Active))));

    // 第二個提案：拒絕就撤掉。
    c.say(project, &propose, SendMode::Queue).await;
    let (b, _) = until_approval(&mut c, |x| {
        matches!(x.action, ApprovalActionView::CreateListener { .. }) && x.is_pending()
    })
    .await;
    let ApprovalActionView::CreateListener { listener: lb, .. } = b.action.clone() else {
        unreachable!()
    };
    c.until_idle(project).await;
    c.send(&decide(b.id, false, Some("不需要"))).await;
    c.until(|e| {
        matches!(e, ServerEvent::Automation { automation, .. }
        if !automation.listeners.iter().any(|l| l.id == lb))
    })
    .await;

    // 第三個提案：你直接刪掉，核准撤回。
    c.say(project, &propose, SendMode::Queue).await;
    let (d, _) = until_approval(&mut c, |x| {
        matches!(x.action, ApprovalActionView::CreateListener { .. }) && x.is_pending()
    })
    .await;
    let ApprovalActionView::CreateListener { listener: ld, .. } = d.action.clone() else {
        unreachable!()
    };
    c.until_idle(project).await;
    c.send(&ClientCommand::DeleteListener { id: ld }).await;
    let (cancelled, _) = until_approval(&mut c, |x| x.id == d.id).await;
    assert_eq!(cancelled.state, ApprovalState::Cancelled);
    srv.stop().await;
}

/// USER.md 與 Project Notes：你直接編輯；agent 只能提案，核准後才寫入，之後進 agent 的 instructions。
/// 提案之後你又改過時不寫入。通知設定與 merge 方式的指令也在這裡驗。
#[tokio::test]
async fn user_md_and_project_notes_need_approval_and_reach_the_preamble() {
    let env = env().await;
    let model = FnProvider::new("m", chat_turn);
    let srv = launch(config(&env), model.clone()).await;
    let mut c = srv.client().await;
    let project = setup(&mut c).await;

    c.send(&ClientCommand::SaveUserMd {
        base_rev: 0,
        body: "## 語言\n\n用繁體中文".into(),
    })
    .await;
    c.until(|e| matches!(e, ServerEvent::UserMd { user_md, .. } if user_md.rev == 1))
        .await;
    c.send(&ClientCommand::SaveUserMd {
        base_rev: 0,
        body: "舊的草稿".into(),
    })
    .await;
    assert!(until_error(&mut c).await.contains("被改過"));

    // 提案修改 USER.md：卡片帶 diff，核准後寫入。
    let edits = json!([{"op": "append_section", "heading": "commit", "body_md": "用 Conventional Commits"}]);
    c.say(
        project,
        &call(
            "propose_user_md_edit",
            json!({"edits": edits, "reason": "你說過兩次"}),
        ),
        SendMode::Queue,
    )
    .await;
    let (a, _) = until_approval(&mut c, |a| {
        matches!(a.action, ApprovalActionView::EditUserMd { .. })
    })
    .await;
    let ApprovalActionView::EditUserMd { base_rev, diff } = &a.action else {
        unreachable!()
    };
    assert_eq!(*base_rev, 1);
    assert!(diff.contains("+用 Conventional Commits"), "{diff}");
    c.until_idle(project).await;
    // 同一個目標同時只能有一個提案。
    c.say(
        project,
        &call(
            "propose_user_md_edit",
            json!({"edits": edits, "reason": "再一次"}),
        ),
        SendMode::Queue,
    )
    .await;
    c.until_idle(project).await;
    let last = model.requests().last().cloned().unwrap();
    assert!(
        tool_outputs(&last).iter().any(|o| o.contains("已經有一筆")),
        "{:?}",
        tool_outputs(&last)
    );
    c.send(&decide(a.id, true, None)).await;
    c.until(|e| matches!(e, ServerEvent::UserMd { user_md, .. }
        if user_md.rev == 2 && user_md.body.contains("Conventional Commits") && user_md.body.contains("繁體中文")))
        .await;

    // Project Notes：提案、核准後 artifact 有新版本。
    c.say(
        project,
        &call(
            "propose_project_note",
            json!({"edits": [{"op": "append_section", "heading": "測試", "body_md": "跑 `cargo test`"}], "reason": "踩過"}),
        ),
        SendMode::Queue,
    )
    .await;
    let (n, _) = until_approval(&mut c, |a| {
        matches!(a.action, ApprovalActionView::EditProjectNotes { .. })
    })
    .await;
    c.until_idle(project).await;
    c.send(&decide(n.id, true, None)).await;
    c.until(|e| {
        matches!(e, ServerEvent::ArtifactChanged { artifact, .. }
        if artifact.kind == ArtifactKind::ProjectNotes && artifact.body.contains("cargo test"))
    })
    .await;

    // 之後的對話：USER.md 在 instructions 最前面，Project Notes 也在。
    c.say(project, "嗨", SendMode::Queue).await;
    c.until_idle(project).await;
    let last = model.requests().last().cloned().unwrap();
    let instr = instructions(&last);
    assert!(instr.starts_with("# 使用者偏好（USER.md，rev 2"), "{instr}");
    assert!(instr.contains("Conventional Commits"));
    assert!(instr.contains("# 專案知識（Project Notes"), "{instr}");
    assert!(instr.contains("cargo test"));
    assert!(instr.contains("# 最近的核准"), "{instr}");

    // 提案之後你又改了 USER.md：核准時不寫入。
    c.say(
        project,
        &call(
            "propose_user_md_edit",
            json!({"edits": [{"op": "append_section", "heading": "回覆", "body_md": "簡短"}], "reason": "x"}),
        ),
        SendMode::Queue,
    )
    .await;
    let (p, _) = until_approval(&mut c, |x| {
        matches!(x.action, ApprovalActionView::EditUserMd { .. }) && x.is_pending()
    })
    .await;
    c.until_idle(project).await;
    c.send(&ClientCommand::SaveUserMd {
        base_rev: 2,
        body: "## 語言\n\n英文也可以".into(),
    })
    .await;
    c.until(|e| matches!(e, ServerEvent::UserMd { user_md, .. } if user_md.rev == 3))
        .await;
    c.send(&decide(p.id, true, None)).await;
    // 廣播的 `ApprovalChanged` 與只回給這條連線的 `Error` 不保證先後。
    let (mut failed, mut error) = (None, None);
    while failed.is_none() || error.is_none() {
        match c.next().await {
            ServerEvent::ApprovalChanged { approval, .. } if approval.id == p.id => {
                failed = Some(approval.state)
            }
            ServerEvent::Error { message } => error = Some(message),
            _ => {}
        }
    }
    assert_eq!(failed, Some(ApprovalState::Failed));
    assert!(error.unwrap().contains("被改過"));

    // 通知設定：時間要是 HH:MM，存起來後在快照裡。
    c.send(&ClientCommand::SaveNotifications {
        settings: NotificationSettings {
            enabled: true,
            quiet: true,
            quiet_start: "25:00".into(),
            quiet_end: "07:00".into(),
        },
    })
    .await;
    assert!(until_error(&mut c).await.contains("HH:MM"));
    c.send(&ClientCommand::SaveNotifications {
        settings: NotificationSettings {
            enabled: true,
            quiet: true,
            quiet_start: "23:00".into(),
            quiet_end: "7:00".into(),
        },
    })
    .await;
    c.until(|e| matches!(e, ServerEvent::Notifications { settings, .. } if settings.quiet_end == "07:00"))
        .await;
    let mut c2 = srv.client().await;
    let (_, snap) = c2.snapshot().await;
    assert!(snap.notifications.quiet);
    assert_eq!(snap.user_md.rev, 3);
    assert!(
        snap.projects[0]
            .artifacts
            .iter()
            .any(|a| a.kind == ArtifactKind::ProjectNotes)
    );
    srv.stop().await;
}

// ---------------------------------------------------------------------------
// 對話 agent 管理自動化與 artifact

/// 送出一則帶 `CALL` 行的訊息，等這輪結束；回傳這輪的工具結果與期間的事件。
async fn run_calls(
    c: &mut Client,
    model: &FnProvider,
    project: ProjectId,
    calls: &[String],
) -> (Vec<String>, Vec<ServerEvent>) {
    c.say(project, &calls.join("\n"), SendMode::Queue).await;
    let events = c.until_idle(project).await;
    let last = model.requests().last().cloned().unwrap();
    let at = last
        .input
        .iter()
        .rposition(|i| matches!(i, Item::User { .. }))
        .unwrap_or(0);
    let outputs = last.input[at..]
        .iter()
        .filter_map(|i| match i {
            Item::ToolResult { output, .. } => Some(output.clone()),
            _ => None,
        })
        .collect();
    (outputs, events)
}

fn field(output: &str, pointer: &str) -> String {
    let v: Value = serde_json::from_str(output).unwrap_or_else(|_| panic!("不是 JSON：{output}"));
    match v.pointer(pointer) {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => panic!("{output} 沒有 {pointer}"),
    }
}

/// 停用、改時間直接生效；刪除排程、listener 與封存 artifact 要核准。版本紀錄與還原直接做。
#[tokio::test]
async fn chat_agent_manages_automations_and_artifacts() {
    let env = env().await;
    let model = FnProvider::new("m", chat_turn);
    let srv = launch(config(&env), model.clone()).await;
    let mut c = srv.client().await;
    let project = setup(&mut c).await;

    // 排程：建立、列出、停用。
    let (out, _) = run_calls(
        &mut c,
        &model,
        project,
        &[call(
            "schedule",
            json!({"kind": "once", "in_minutes": 60, "prompt": "提醒我看 CI"}),
        )],
    )
    .await;
    let schedule = field(&out[0], "/schedule_id");
    let (out, _) = run_calls(
        &mut c,
        &model,
        project,
        &[call("list_automations", json!({}))],
    )
    .await;
    assert!(
        out[0].contains(&schedule) && out[0].contains("提醒我看 CI"),
        "{}",
        out[0]
    );
    let (out, events) = run_calls(
        &mut c,
        &model,
        project,
        &[call(
            "update_schedule",
            json!({"schedule_id": schedule, "enabled": false}),
        )],
    )
    .await;
    assert_eq!(field(&out[0], "/schedule/enabled"), "false", "{}", out[0]);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ServerEvent::Automation { automation, .. }
        if automation.schedules.iter().any(|s| s.id.to_string() == schedule && !s.enabled)))
    );

    // 刪除排程要核准；同一個排程同時只能有一筆申請。
    let delete = call(
        "delete_schedule",
        json!({"schedule_id": schedule, "reason": "不需要了"}),
    );
    let (out, events) = run_calls(&mut c, &model, project, &[delete.clone(), delete]).await;
    assert!(out[1].contains("已經有一筆"), "{}", out[1]);
    let a = events
        .iter()
        .find_map(|e| match e {
            ServerEvent::ApprovalChanged { approval, .. }
                if matches!(approval.action, ApprovalActionView::DeleteSchedule { .. }) =>
            {
                Some((**approval).clone())
            }
            _ => None,
        })
        .expect("刪除排程的核准");
    assert_eq!(a.id.to_string(), field(&out[0], "/approval_id"));
    c.send(&decide(a.id, true, None)).await;
    let (done, events) = until_approval(&mut c, |x| x.id == a.id).await;
    assert_eq!(done.state, ApprovalState::Executed, "{:?}", done.result);
    // 同一個 transaction 裡先送 `Automation`，再送 `ApprovalChanged`。
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ServerEvent::Automation { automation, .. }
        if !automation.schedules.iter().any(|s| s.id.to_string() == schedule)))
    );

    // listener：等核准的提案不能直接啟用；核准後可以停用；刪除被拒絕時保留。
    let (out, _) = run_calls(
        &mut c,
        &model,
        project,
        &[call(
            "create_listener",
            json!({"kinds": ["issue.labeled"], "fields": [{"field": "label", "any_of": ["agent"]}], "action": "notify_chat"}),
        )],
    )
    .await;
    let listener = field(&out[0], "/listener_id");
    let proposal: ApprovalId = field(&out[0], "/approval_id").parse().unwrap();
    let (out, _) = run_calls(
        &mut c,
        &model,
        project,
        &[call(
            "update_listener",
            json!({"listener_id": listener, "enabled": true}),
        )],
    )
    .await;
    assert!(out[0].contains("等使用者核准"), "{}", out[0]);
    c.send(&decide(proposal, true, None)).await;
    let (approved, _) = until_approval(&mut c, |x| x.id == proposal).await;
    assert_eq!(approved.state, ApprovalState::Executed);
    let (_, events) = run_calls(
        &mut c,
        &model,
        project,
        &[call(
            "update_listener",
            json!({"listener_id": listener, "enabled": false}),
        )],
    )
    .await;
    assert!(events.iter().any(|e| matches!(e, ServerEvent::Automation { automation, .. }
        if automation.listeners.iter().any(|l| l.id.to_string() == listener && l.state == ListenerState::Disabled))));
    let (out, _) = run_calls(
        &mut c,
        &model,
        project,
        &[call(
            "delete_listener",
            json!({"listener_id": listener, "reason": "改用排程"}),
        )],
    )
    .await;
    let del: ApprovalId = field(&out[0], "/approval_id").parse().unwrap();
    c.send(&decide(del, false, Some("先留著"))).await;
    let (rejected, _) = until_approval(&mut c, |x| x.id == del).await;
    assert_eq!(rejected.state, ApprovalState::Rejected);
    let (out, _) = run_calls(
        &mut c,
        &model,
        project,
        &[call("list_automations", json!({}))],
    )
    .await;
    assert!(
        out[0].contains(&listener) && out[0].contains("disabled"),
        "{}",
        out[0]
    );

    // artifact：Report 可以寫；版本紀錄、讀舊版、還原（base_rev 要是目前的版本）。
    let (out, _) = run_calls(
        &mut c,
        &model,
        project,
        &[call(
            "write_artifact",
            json!({"kind": "Report", "title": "調查", "edits": [{"op": "replace_all", "body_md": "## 結論\n\n舊的結論"}]}),
        )],
    )
    .await;
    let artifact = field(&out[0], "/written/artifact_id");
    run_calls(
        &mut c,
        &model,
        project,
        &[call(
            "write_artifact",
            json!({"artifact_id": artifact, "base_rev": 1, "edits": [{"op": "replace_section", "heading": "結論", "body_md": "新的結論"}]}),
        )],
    )
    .await;
    let (out, _) = run_calls(
        &mut c,
        &model,
        project,
        &[
            call("artifact_history", json!({"artifact_id": artifact})),
            call(
                "artifact_history",
                json!({"artifact_id": artifact, "rev": 1}),
            ),
            call(
                "restore_artifact",
                json!({"artifact_id": artifact, "rev": 1, "base_rev": 1}),
            ),
            call(
                "restore_artifact",
                json!({"artifact_id": artifact, "rev": 1, "base_rev": 2}),
            ),
        ],
    )
    .await;
    assert!(
        out[0].contains("## v2") && out[0].contains("+新的結論"),
        "{}",
        out[0]
    );
    assert!(out[1].contains("舊的結論"), "{}", out[1]);
    assert!(out[2].contains("不是目前的版本"), "{}", out[2]);
    assert_eq!(field(&out[3], "/rev"), "3", "{}", out[3]);

    // 封存要核准；核准後從列表與快照消失，也讀不到了。
    let (out, _) = run_calls(
        &mut c,
        &model,
        project,
        &[call(
            "archive_artifact",
            json!({"artifact_id": artifact, "reason": "已經過時"}),
        )],
    )
    .await;
    let archive: ApprovalId = field(&out[0], "/approval_id").parse().unwrap();
    c.send(&decide(archive, true, None)).await;
    c.until(|e| matches!(e, ServerEvent::ArtifactArchived { artifact: a, .. } if a.to_string() == artifact))
        .await;
    let mut c2 = srv.client().await;
    let (_, snap) = c2.snapshot().await;
    assert!(
        !snap.projects[0]
            .artifacts
            .iter()
            .any(|a| a.id.to_string() == artifact)
    );
    let (out, _) = run_calls(
        &mut c,
        &model,
        project,
        &[call("read_artifact", json!({"artifact_id": artifact}))],
    )
    .await;
    assert!(out[0].contains("沒有 artifact"), "{}", out[0]);
    srv.stop().await;
}

// ---------------------------------------------------------------------------
// agent 問你問題（ask_user）

/// 這個請求看得到的所有文字（instructions 與使用者訊息），用來找測試的記號。
fn all_text(req: &ModelRequest) -> String {
    let mut out = instructions(req).to_string();
    for i in &req.input {
        if let Item::User { text } = i {
            out.push('\n');
            out.push_str(text);
        }
    }
    out
}

fn is_question(a: &ApprovalView) -> bool {
    matches!(a.action, ApprovalActionView::AskUser { .. })
}

/// worker、Planner 與對話 agent 都能用 ask_user 問你：問題是一筆核准（「核准」是回答、「拒絕」是略過）。
/// worker 的任務轉成等回答、回答後被叫醒；Planner 一次只能問一個、回答或略過後由新的 run 收到；
/// 對話 agent 的卡片附在回覆下面、回答以系統觸發送回對話；Goal 取消時 Planner 的問題撤回。
#[tokio::test]
async fn ask_user_wakes_whoever_asked() {
    let env = env().await;
    let model = FnProvider::new("m", |req| {
        let instr = instructions(req);
        let trigger = first_user(req);
        let outputs = tool_outputs(req);
        let text = all_text(req);
        if instr.starts_with(PLANNER) {
            if text.contains("PLANNER_ASK") && trigger.contains("剛建立這個 Goal") {
                return match outputs.len() {
                    0 => ScriptedTurn::new().tool_call(
                        "pq0",
                        "ask_user",
                        json!({"question": "登入要支援哪些 provider？", "options": ["只要 Google", "Google 與 GitHub"]})
                            .to_string(),
                    ),
                    // 已經有一個在等：第二個問題被擋。
                    1 => ScriptedTurn::new().tool_call(
                        "pq1",
                        "ask_user",
                        json!({"question": "還有別的嗎？"}).to_string(),
                    ),
                    _ => ScriptedTurn::reply("等使用者回答"),
                };
            }
            if text.contains("PLANNER_ASK") {
                return ScriptedTurn::reply("收到回答");
            }
            if trigger.contains("剛建立這個 Goal") && outputs.is_empty() {
                return ScriptedTurn::new().tool_call(
                    "p0",
                    "create_task",
                    json!({"title": "問一下環境", "summary": "WORKER_ASK：先問使用者要部署到哪裡"})
                        .to_string(),
                );
            }
            return ScriptedTurn::reply("好");
        }
        if instr.starts_with(WORKER) {
            if trigger.contains("使用者回答了你的問題") {
                return match outputs.len() {
                    0 => ScriptedTurn::new().tool_call(
                        "wd",
                        "fail_task",
                        json!({"error": "測試到這裡就好"}).to_string(),
                    ),
                    _ => ScriptedTurn::reply("結束"),
                };
            }
            return match outputs.len() {
                0 => ScriptedTurn::new().tool_call(
                    "wq",
                    "ask_user",
                    json!({
                        "question": "要部署到哪個環境？",
                        "options": ["staging", "production"],
                        "allow_other": false,
                        "reason": "任務沒寫",
                    })
                    .to_string(),
                ),
                _ => ScriptedTurn::reply("等回答"),
            };
        }
        chat_turn(req)
    });
    let srv = launch(config(&env), model.clone()).await;
    let mut c = srv.client().await;
    let project = setup(&mut c).await;

    // worker：任務轉成等回答。
    c.say(
        project,
        &call(
            "create_goal",
            json!({"title": "部署", "brief_md": "## 目標\n\n部署"}),
        ),
        SendMode::Queue,
    )
    .await;
    let (wq, _) = until_approval(&mut c, |a| is_question(a) && a.task.is_some()).await;
    assert!(wq.is_pending());
    assert_eq!(wq.requested_by, "worker");
    let ApprovalActionView::AskUser {
        question,
        options,
        allow_other,
    } = &wq.action
    else {
        unreachable!()
    };
    assert_eq!(question, "要部署到哪個環境？");
    assert_eq!(options, &["staging", "production"]);
    assert!(!allow_other);
    let g = until_goal(&mut c, |g| {
        g.tasks.first().is_some_and(
            |t| matches!(&t.state, TaskStateView::Waiting { on, .. } if on.contains("等你回答")),
        )
    })
    .await;
    let task = g.tasks[0].id;
    // 回答要有內容；不能自由回答時要是其中一個選項。
    c.send(&decide(wq.id, true, None)).await;
    assert!(until_error(&mut c).await.contains("答案"));
    c.send(&decide(wq.id, true, Some("dev"))).await;
    assert!(until_error(&mut c).await.contains("選項"));
    c.send(&decide(wq.id, true, Some("staging"))).await;
    let (answered, _) = until_approval(&mut c, |a| a.id == wq.id && !a.is_pending()).await;
    assert_eq!(answered.state, ApprovalState::Executed);
    assert_eq!(answered.note.as_deref(), Some("staging"));
    until_goal(&mut c, |g| {
        g.tasks
            .iter()
            .any(|t| t.id == task && matches!(t.state, TaskStateView::Failed { .. }))
    })
    .await;
    assert!(model.requests().iter().any(|r| {
        instructions(r).starts_with(WORKER)
            && first_user(r).contains("使用者回答了你的問題「要部署到哪個環境？」：staging")
    }));

    // Planner：一次只能問一個；略過時帶著理由叫醒它。
    c.say(
        project,
        &call(
            "create_goal",
            json!({"title": "登入", "brief_md": "## 目標\n\nPLANNER_ASK"}),
        ),
        SendMode::Queue,
    )
    .await;
    let (pq, _) = until_approval(&mut c, |a| {
        is_question(a) && a.task.is_none() && a.goal.is_some() && a.is_pending()
    })
    .await;
    assert_eq!(pq.requested_by, "Planner");
    let blocked = |req: &ModelRequest| {
        instructions(req).starts_with(PLANNER)
            && tool_outputs(req)
                .iter()
                .any(|o| o.contains("已經有一個問題在等使用者回答"))
    };
    for _ in 0..100 {
        if model.requests().iter().any(blocked) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(model.requests().iter().any(blocked), "第二個問題被擋");
    c.send(&decide(pq.id, false, Some("你決定就好"))).await;
    let (skipped, _) = until_approval(&mut c, |a| a.id == pq.id && !a.is_pending()).await;
    assert_eq!(skipped.state, ApprovalState::Rejected);
    let woke = |req: &ModelRequest| {
        instructions(req).starts_with(PLANNER)
            && first_user(req).contains("沒有得到回答（使用者略過了，理由：你決定就好）")
    };
    for _ in 0..100 {
        if model.requests().iter().any(woke) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(model.requests().iter().any(woke), "略過後叫醒 Planner");

    // Planner 的問題在 Goal 取消時撤回。
    c.say(
        project,
        &call(
            "create_goal",
            json!({"title": "登入 2", "brief_md": "## 目標\n\nPLANNER_ASK"}),
        ),
        SendMode::Queue,
    )
    .await;
    let (pq2, seen) = until_approval(&mut c, |a| {
        is_question(a) && a.is_pending() && a.id != pq.id
    })
    .await;
    let goal = pq2.goal.expect("Planner 的問題有 Goal");
    // 對話 agent 可能在 Planner 問問題之前就結束了（Goal 開始之後 Planner 才跑）。
    if !seen
        .iter()
        .any(|e| common::is_state(e, project, pots_proto::AgentState::Idle))
    {
        c.until_idle(project).await;
    }
    c.say(
        project,
        &call(
            "cancel_goal",
            json!({"goal_id": goal.to_string(), "reason": "不做了"}),
        ),
        SendMode::Queue,
    )
    .await;
    let (withdrawn, _) = until_approval(&mut c, |a| a.id == pq2.id && !a.is_pending()).await;
    assert_eq!(withdrawn.state, ApprovalState::Cancelled);
    c.until_idle(project).await;

    // 對話 agent：卡片附在回覆下面；回答以系統觸發送回對話。
    c.say(
        project,
        &call(
            "ask_user",
            json!({"question": "要叫你什麼？", "options": ["老闆", "你"]}),
        ),
        SendMode::Queue,
    )
    .await;
    let (cq, events) = until_approval(&mut c, |a| {
        is_question(a) && a.goal.is_none() && a.is_pending()
    })
    .await;
    let more = c.until_idle(project).await;
    let events = [events, more].concat();
    assert!(events.iter().any(|e| matches!(e,
        ServerEvent::ItemDone { item, .. } if matches!(&item.kind,
            StreamItemKind::Agent(m) if m.cards.iter().any(|c| matches!(c, Card::Approval { id } if *id == cq.id))))));
    c.send(&decide(cq.id, true, Some("叫我阿冰"))).await;
    until_approval(&mut c, |a| {
        a.id == cq.id && a.state == ApprovalState::Executed
    })
    .await;
    c.until_idle(project).await;
    assert!(model.requests().iter().any(|r| {
        r.input.iter().any(|i| matches!(i, Item::User { text }
            if text.contains("問題卡片的回答") && text.contains("要叫你什麼？") && text.contains("叫我阿冰")))
    }));
    srv.stop().await;
}
