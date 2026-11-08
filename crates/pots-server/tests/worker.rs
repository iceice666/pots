//! M4 coding worker 的整合測試（`proposal/agent-runtime.md#worker-adapter內建-worker-與外部-coding-cli`）。
//!
//! - GitHub API 換成假的伺服器（開 PR、CI 一律通過），git remote 換成本機的 bare repo
//!   （`WorkerConfig::git_base`），worker 在本機執行（沒有容器）。
//! - 模型換成依請求內容回應的假 provider：對話 agent 執行使用者訊息裡的 `CALL` 行；Planner、
//!   內建 worker 與 subagent 各自依觸發訊息決定。
//! - Claude Code 換成 `pots-fake-claude`（重播劇本，並經 MCP 呼叫平台工具）；實際的 Claude Code
//!   這個里程碑不測。

mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use common::{Client, ScriptedApi, TestServer};
use pots_llm::testing::{FnProvider, ScriptedTurn};
use pots_llm::{Item, ModelRequest};
use pots_proto::{
    Card, ClientCommand, ExecKind, GithubDraft, GoalState, GoalView, ProjectId, ProviderId,
    SendMode, ServerEvent, StreamItemKind, TaskStateView, TeamPostKind, WorkerBackendKind,
    WorkerSettingsDraft,
};
use pots_server::{Config, Deps, FakeModelAuth, WatcherConfig};
use serde_json::{Value, json};

const PLANNER: &str = "你是 Pots 的 Planner";
const WORKER: &str = "你是 Pots 的 coding worker";
const EXPLORER: &str = "你是 Pots 的 explorer subagent";
const TOKEN: &str = "ghp_test_token_1234";

// ---------------------------------------------------------------------------
// 假的 GitHub API：沒有事件、CI 一律通過、可以開 PR。

#[derive(Default)]
struct GhState {
    /// `owner/repo` → 開過的 PR（POST 的 body 加上編號）。
    pulls: HashMap<String, Vec<Value>>,
    auth_ok: bool,
    calls: Vec<String>,
}

#[derive(Clone)]
struct FakeGithub {
    addr: SocketAddr,
    state: Arc<Mutex<GhState>>,
}

type Gh = State<Arc<Mutex<GhState>>>;

fn record(s: &Arc<Mutex<GhState>>, headers: &HeaderMap, call: String) {
    let ok = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == format!("Bearer {TOKEN}"));
    let mut s = s.lock().unwrap();
    s.calls.push(call);
    s.auth_ok = ok;
}

impl FakeGithub {
    async fn start() -> Self {
        let state = Arc::new(Mutex::new(GhState::default()));
        let app = Router::new()
            .route(
                "/user",
                get(|| async { axum::Json(json!({"login": "me"})) }),
            )
            .route("/repos/{o}/{r}/events", get(gh_events))
            .route(
                "/repos/{o}/{r}/commits/{git_ref}/check-suites",
                get(gh_suites),
            )
            .route(
                "/repos/{o}/{r}/pulls",
                get(gh_list_pulls).post(gh_open_pull),
            )
            .route("/repos/{o}/{r}/pulls/{n}", get(gh_pull))
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

    fn pulls(&self, repo: &str) -> Vec<Value> {
        self.state
            .lock()
            .unwrap()
            .pulls
            .get(repo)
            .cloned()
            .unwrap_or_default()
    }
}

async fn gh_events(State(s): Gh, headers: HeaderMap) -> Response {
    record(&s, &headers, "events".into());
    ([("etag", "\"0\"")], axum::Json(json!([]))).into_response()
}

/// CI：任何 commit 都是一個跑完、成功的 suite。
async fn gh_suites(
    State(s): Gh,
    UrlPath((_o, _r, git_ref)): UrlPath<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    record(&s, &headers, format!("suites {git_ref}"));
    axum::Json(json!({
        "total_count": 1,
        "check_suites": [{
            "head_sha": git_ref, "head_branch": "x", "status": "completed", "conclusion": "success",
            "latest_check_runs_count": 1, "updated_at": "2026-11-08T01:00:00Z",
        }],
    }))
    .into_response()
}

async fn gh_list_pulls(
    State(s): Gh,
    UrlPath((o, r)): UrlPath<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    record(&s, &headers, "list pulls".into());
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
    headers: HeaderMap,
    axum::Json(mut body): axum::Json<Value>,
) -> Response {
    record(&s, &headers, "open pull".into());
    let mut st = s.lock().unwrap();
    let list = st.pulls.entry(format!("{o}/{r}")).or_default();
    let n = 40 + list.len() as u64;
    body["number"] = json!(n);
    body["html_url"] = json!(format!("https://github.com/{o}/{r}/pull/{n}"));
    list.push(body.clone());
    (StatusCode::CREATED, axum::Json(body)).into_response()
}

async fn gh_pull(State(s): Gh, UrlPath((o, r, n)): UrlPath<(String, String, u64)>) -> Response {
    let found = s
        .lock()
        .unwrap()
        .pulls
        .get(&format!("{o}/{r}"))
        .and_then(|l| l.iter().find(|p| p["number"] == json!(n)).cloned());
    match found {
        Some(mut p) => {
            p["merged"] = json!(false);
            axum::Json(p).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---------------------------------------------------------------------------
// git

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

/// `remotes/me/app.git`：有一個 main 分支、README.md 寫著 `hello pots`。
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

// ---------------------------------------------------------------------------
// 服務與假模型

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

/// 觸發訊息裡的 `task_id：<id>`。
fn task_id_of(trigger: &str) -> String {
    trigger
        .lines()
        .find_map(|l| l.trim().strip_prefix("- task_id："))
        .unwrap_or_default()
        .trim()
        .to_string()
}

struct Env {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    remotes: PathBuf,
    gh: FakeGithub,
}

async fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let remotes = make_remote(&dir);
    let gh = FakeGithub::start().await;
    Env {
        _tmp: tmp,
        dir,
        remotes,
        gh,
    }
}

fn config(env: &Env, backend: WorkerBackendKind, claude: Option<&Path>) -> Config {
    let mut config =
        Config::desktop(env.dir.join("data")).with_default_provider(ProviderId::CHATGPT);
    config.watcher = WatcherConfig {
        poll_interval: Duration::from_millis(100),
        sweep_interval: Duration::from_millis(100),
        github_api: env.gh.url(),
        ..WatcherConfig::default()
    };
    config.worker.git_base = Some(format!("file://{}", env.remotes.display()));
    config.worker.defaults = WorkerSettingsDraft {
        backend,
        exec: ExecKind::Local,
        runtime: "docker".into(),
        image: "pots-worker:latest".into(),
        claude_model: String::new(),
        max_parallel: 2,
    };
    config.worker.grace = Duration::from_secs(3);
    if let Some(c) = claude {
        config.worker.claude_bin = c.display().to_string();
    }
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

/// 設好 GitHub token 與 project 的 repo。
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
    project
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

fn milestones(events: &[ServerEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            ServerEvent::Item { item, .. } => match &item.kind {
                StreamItemKind::Card(Card::Milestone { text, .. }) => Some(text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------

/// 一句話 → Goal → Planner 派工 → 內建 worker 改檔、在 team 發問、開 PR、等 CI → CI 通過被喚醒 →
/// 完成任務 → Planner 被叫醒、結束 Goal。PR 推到 remote、token 只送給 GitHub API。
#[tokio::test]
async fn builtin_worker_opens_pr_waits_for_ci_and_completes() {
    let env = env().await;
    let model = FnProvider::new("m", |req| {
        let instr = instructions(req);
        let trigger = first_user(req);
        let outputs = tool_outputs(req);
        if instr.starts_with(PLANNER) {
            if trigger.contains("剛建立這個 Goal") {
                return match outputs.len() {
                    0 => ScriptedTurn::new().tool_call(
                        "p0",
                        "create_task",
                        json!({"title": "Add hello", "summary": "加上 hello.txt"}).to_string(),
                    ),
                    _ => ScriptedTurn::reply("派好了"),
                };
            }
            if trigger.contains("在 team 發了問題") {
                return match outputs.len() {
                    0 => ScriptedTurn::new().tool_call(
                        "p1",
                        "post_to_team",
                        json!({"kind": "decision", "text": "用 hi 就好"}).to_string(),
                    ),
                    _ => ScriptedTurn::reply("回答了"),
                };
            }
            if trigger.contains("完成：") {
                return match outputs.len() {
                    0 => ScriptedTurn::new().tool_call(
                        "p2",
                        "complete_goal",
                        json!({"summary": "hello.txt 加好了"}).to_string(),
                    ),
                    _ => ScriptedTurn::reply("結束"),
                };
            }
            return ScriptedTurn::reply("看過了");
        }
        if instr.starts_with(WORKER) {
            if trigger.contains("現在有結果了") {
                assert!(trigger.contains("CI 通過"), "{trigger}");
                return match outputs.len() {
                    0 => ScriptedTurn::new().tool_call(
                        "w9",
                        "complete_task",
                        json!({"summary": "加上 hello.txt，CI 通過"}).to_string(),
                    ),
                    _ => ScriptedTurn::reply("好了"),
                };
            }
            let task = task_id_of(&trigger);
            return match outputs.len() {
                0 => ScriptedTurn::new().tool_call(
                    "w0",
                    "edit_file",
                    json!({"path": "hello.txt", "new_text": "hi\n"}).to_string(),
                ),
                1 => ScriptedTurn::new().tool_call(
                    "w1",
                    "post_to_team",
                    json!({"kind": "question", "text": "內容要寫什麼？"}).to_string(),
                ),
                2 => ScriptedTurn::new().tool_call(
                    "w2",
                    "run",
                    json!({"command": "git add -A && git commit -qm 'Add hello' && git log --oneline | wc -l"})
                        .to_string(),
                ),
                3 => ScriptedTurn::new().tool_call(
                    "w3",
                    "open_pr",
                    json!({"title": "Add hello", "body": "加上 hello.txt"}).to_string(),
                ),
                4 => {
                    let pr: Value = serde_json::from_str(&outputs[3]).expect("open_pr 的結果");
                    ScriptedTurn::new().tool_call(
                        "w4",
                        "wait_for",
                        json!({
                            "task_id": task,
                            "on": {"kind": "ci_finished", "repo": "me/app", "sha": pr["sha"]},
                            "timeout_minutes": 30,
                        })
                        .to_string(),
                    )
                }
                _ => ScriptedTurn::reply("不該到這裡"),
            };
        }
        chat_turn(req)
    });
    let srv = launch(
        config(&env, WorkerBackendKind::Builtin, None),
        model.clone(),
    )
    .await;
    let mut c = srv.client().await;
    let project = setup(&mut c).await;

    let text = call(
        "create_goal",
        json!({"title": "加 hello", "brief_md": "## 目標\n\n加一個 hello.txt"}),
    );
    c.say(project, &text, SendMode::Queue).await;
    let g = until_goal(&mut c, |g| g.state != GoalState::Active).await;
    assert_eq!(
        g.state,
        GoalState::Done {
            summary: "hello.txt 加好了".into()
        }
    );
    let t = &g.tasks[0];
    assert_eq!(
        t.state,
        TaskStateView::Done {
            outcome: "加上 hello.txt，CI 通過".into()
        }
    );
    assert_eq!(t.assignee, "worker · 內建");
    let branch = t.branch.clone().expect("分支");
    assert!(branch.starts_with("pots/add-hello-"), "{branch}");
    assert_eq!(t.pr.as_deref(), Some("github.com/me/app#40"));
    assert_eq!(
        t.pr_url.as_deref(),
        Some("https://github.com/me/app/pull/40")
    );
    let steps: Vec<&str> = t.steps.iter().map(|s| s.text.as_str()).collect();
    assert!(
        steps.iter().any(|s| s.starts_with("開始等待等 CI")),
        "{steps:?}"
    );
    assert!(steps.iter().any(|s| s.starts_with("喚醒")), "{steps:?}");

    // team：worker 的問題與 Planner 的決定。
    let kinds: Vec<_> = g.team.iter().map(|p| p.kind).collect();
    assert_eq!(kinds, [TeamPostKind::Question, TeamPostKind::Decision]);
    assert!(g.team[0].from.starts_with("worker · "), "{:?}", g.team[0]);

    // PR 開在假的 GitHub，帶著平台的 token；分支推到 remote，內容是 worker 的 commit。
    let pulls = env.gh.pulls("me/app");
    assert_eq!(pulls.len(), 1);
    assert_eq!(pulls[0]["head"], json!(branch));
    assert_eq!(pulls[0]["base"], json!("main"));
    assert!(env.gh.state.lock().unwrap().auth_ok);
    let remote = env.remotes.join("me/app.git");
    assert_eq!(
        git(&remote, &["show", &format!("{branch}:hello.txt")]),
        "hi\n"
    );
    assert_eq!(
        git(&remote, &["log", "--format=%s", "-1", &branch]).trim(),
        "Add hello"
    );
    srv.stop().await;
}

/// 快速修改經 Claude Code（假的 CLI）：原生 Bash 改檔並 commit、經 MCP 開 PR 與等待；
/// 喚醒時用同一個 session `--resume` 續接（新的行程，就像重建了容器），完成後 quick Goal 結束。
#[tokio::test]
async fn claude_code_quick_fix_resumes_session_after_wait() {
    let env = env().await;
    let script = env.dir.join("script.json");
    std::fs::write(
        &script,
        json!({
            "new": [
                {"text": "我來修"},
                {"native": {"tool": "Bash", "input": {"command": "printf 'fixed\\n' > fix.txt && git add -A && git commit -qm 'Fix typo'"}}},
                {"native": {"tool": "Bash", "input": {"command": "git push origin HEAD"}}},
                {"expect_is_error": true},
                {"mcp": {"tool": "open_pr", "args": {"title": "Fix typo"}}},
                {"expect_contains": "\"sha\""},
                {"mcp": {"tool": "wait_for", "args": {"on": {"kind": "at", "at": "2000-01-01T00:00:00Z"}, "timeout_minutes": 60}}},
                {"expect_contains": "suspended"},
                {"text": "這行不該出現（等待後應該被中斷）"},
                {"sleep_ms": 30000},
                {"end_turn": "等"},
            ],
            "resume": [
                {"mcp": {"tool": "complete_task", "args": {"summary": "修好錯字"}}},
                {"sleep_ms": 30000},
            ],
        })
        .to_string(),
    )
    .unwrap();
    let log = env.dir.join("claude.log");
    let claude = fake_claude(&env.dir, &script, &log);
    let model = FnProvider::new("m", chat_turn);
    let srv = launch(
        config(&env, WorkerBackendKind::ClaudeCode, Some(&claude)),
        model,
    )
    .await;
    let mut c = srv.client().await;
    let project = setup(&mut c).await;

    let text = call(
        "quick_fix",
        json!({"title": "Fix typo", "description": "README 有錯字"}),
    );
    c.say(project, &text, SendMode::Queue).await;
    let events = c
        .until(|e| matches!(e, ServerEvent::GoalChanged { goal, .. } if goal.state != GoalState::Active))
        .await;
    let Some(ServerEvent::GoalChanged { goal: g, .. }) = events.last().cloned() else {
        unreachable!()
    };
    assert!(g.quick);
    assert_eq!(
        g.state,
        GoalState::Done {
            summary: "修好錯字".into()
        }
    );
    let t = &g.tasks[0];
    assert_eq!(t.assignee, "worker · Claude Code");
    assert_eq!(
        t.state,
        TaskStateView::Done {
            outcome: "修好錯字".into()
        }
    );
    let more = c
        .until(|e| matches!(e, ServerEvent::Item { item, .. } if matches!(&item.kind, StreamItemKind::Card(Card::Milestone { .. }))))
        .await;
    let cards = milestones(&[events, more].concat());
    assert!(
        cards
            .iter()
            .any(|m| m.starts_with("快速修改完成：修好錯字")),
        "{cards:?}"
    );

    // 兩次啟動：第一次新的 session，第二次 --resume 同一個。
    let lines: Vec<Value> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let argvs: Vec<Vec<String>> = lines
        .iter()
        .filter(|l| l["kind"] == "argv")
        .map(|l| serde_json::from_value(l["argv"].clone()).unwrap())
        .collect();
    assert_eq!(argvs.len(), 2, "{argvs:?}");
    let session = argvs[0]
        .iter()
        .find_map(|a| a.strip_prefix("--session-id="))
        .expect("--session-id")
        .to_string();
    assert!(
        argvs[1].contains(&format!("--resume={session}")),
        "{:?}",
        argvs[1]
    );
    assert!(argvs[0].iter().any(|a| a == "--strict-mcp-config"));
    // push 被平台擋下；等待之後送了 interrupt，後面的步驟沒有執行。
    let tools: Vec<&Value> = lines.iter().filter(|l| l["kind"] == "tool").collect();
    let push = tools
        .iter()
        .find(|t| t["input"]["command"] == json!("git push origin HEAD"))
        .expect("push");
    assert_eq!(push["is_error"], json!(true));
    assert!(
        lines.iter().any(|l| l["kind"] == "stdin"
            && l["frame"]["request"]["subtype"] == json!("interrupt"))
    );
    // 結束工具（wait_for、complete_task）一成功平台就中斷 CLI，假的 CLI 不一定來得及記下結果；
    // 它們有執行到以任務狀態為準（上面已經驗過）。
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"mcp__pots__open_pr"), "{names:?}");
    let steps: Vec<&str> = t.steps.iter().map(|s| s.text.as_str()).collect();
    assert!(
        steps.iter().any(|s| s.starts_with("開始等待等到")),
        "{steps:?}"
    );

    let pulls = env.gh.pulls("me/app");
    assert_eq!(pulls.len(), 1);
    let branch = pulls[0]["head"].as_str().unwrap().to_string();
    let remote = env.remotes.join("me/app.git");
    assert_eq!(
        git(&remote, &["show", &format!("{branch}:fix.txt")]),
        "fixed\n"
    );
    srv.stop().await;
}

/// 包一層 shell：帶上劇本、log 與 Claude 的設定目錄再執行假的 CLI。
fn fake_claude(dir: &Path, script: &Path, log: &Path) -> PathBuf {
    let path = dir.join("claude");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nPOTS_FAKE_CLAUDE_SCRIPT='{}' POTS_FAKE_CLAUDE_LOG='{}' CLAUDE_CONFIG_DIR='{}' exec '{}' \"$@\"\n",
            script.display(),
            log.display(),
            dir.join("claude-config").display(),
            env!("CARGO_BIN_EXE_pots-fake-claude"),
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// 你取消正在跑的任務：Claude Code 收到 interrupt 並結束，任務停在取消。Claude Code 要重新登入時
/// 任務轉成需要決定，說明要跑的登入指令。
#[tokio::test]
async fn cancel_interrupts_claude_and_login_errors_need_human() {
    let env = env().await;
    let script = env.dir.join("script.json");
    let log = env.dir.join("claude.log");
    let claude = fake_claude(&env.dir, &script, &log);
    std::fs::write(
        &script,
        json!({"new": [{"text": "開始"}, {"sleep_ms": 60000}, {"end_turn": "x"}]}).to_string(),
    )
    .unwrap();
    let model = FnProvider::new("m", chat_turn);
    let srv = launch(
        config(&env, WorkerBackendKind::ClaudeCode, Some(&claude)),
        model,
    )
    .await;
    let mut c = srv.client().await;
    let project = setup(&mut c).await;
    c.say(
        project,
        &call("quick_fix", json!({"title": "慢", "description": "跑很久"})),
        SendMode::Queue,
    )
    .await;
    let g = until_goal(&mut c, |g| {
        g.tasks
            .first()
            .is_some_and(|t| t.state == TaskStateView::Running && t.progress == "開始")
    })
    .await;
    let task = g.tasks[0].id;
    let started = std::time::Instant::now();
    c.send(&ClientCommand::Cancel { task }).await;
    let g = until_goal(&mut c, |g| {
        g.tasks
            .first()
            .is_some_and(|t| t.state == TaskStateView::Cancelled && t.progress.is_empty())
    })
    .await;
    assert!(started.elapsed() < Duration::from_secs(10));
    // 快速修改只有這個任務：任務取消，Goal 也跟著取消。
    assert!(
        matches!(g.state, GoalState::Cancelled { .. }),
        "{:?}",
        g.state
    );
    let log_text = std::fs::read_to_string(&log).unwrap();
    assert!(log_text.contains("\"interrupt\""), "{log_text}");

    // 需要重新登入。
    std::fs::write(
        &script,
        json!({"new": [{"error": "authentication_failed"}]}).to_string(),
    )
    .unwrap();
    c.say(
        project,
        &call("quick_fix", json!({"title": "登入", "description": "x"})),
        SendMode::Queue,
    )
    .await;
    let g = until_goal(&mut c, |g| {
        g.title == "登入"
            && g.tasks
                .first()
                .is_some_and(|t| matches!(t.state, TaskStateView::NeedsHuman { .. }))
    })
    .await;
    let TaskStateView::NeedsHuman { reason } = &g.tasks[0].state else {
        unreachable!()
    };
    assert!(
        reason.contains("重新登入") && reason.contains("/login"),
        "{reason}"
    );
    assert!(
        !reason.contains(env.dir.to_str().unwrap()),
        "原因不帶主機路徑：{reason}"
    );

    // 你登入之後按「重試」：同一個任務回到佇列，這次做完。
    std::fs::write(
        &script,
        json!({"new": [{"mcp": {"tool": "complete_task", "args": {"summary": "登入後完成"}}}],
               "resume": [{"mcp": {"tool": "complete_task", "args": {"summary": "登入後完成"}}}]})
        .to_string(),
    )
    .unwrap();
    c.send(&ClientCommand::RetryTask {
        task: g.tasks[0].id,
    })
    .await;
    let g = until_goal(&mut c, |g| {
        g.title == "登入" && g.state != GoalState::Active
    })
    .await;
    assert_eq!(
        g.state,
        GoalState::Done {
            summary: "登入後完成".into()
        }
    );
    srv.stop().await;
}

/// worker 遇到要決定的事：在 team 發問並 escalate；Planner 以指定任務的 decision 回答，叫醒同一個
/// 任務，回答出現在觸發訊息裡。改到 CI 設定的 PR 平台不推。
#[tokio::test]
async fn planner_decision_wakes_worker_and_ci_config_is_refused() {
    let env = env().await;
    let worker_triggers = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = worker_triggers.clone();
    let model = FnProvider::new("m", move |req| {
        let instr = instructions(req);
        let trigger = first_user(req);
        let outputs = tool_outputs(req);
        if instr.starts_with(PLANNER) {
            if trigger.contains("剛建立這個 Goal") {
                return match outputs.len() {
                    0 => ScriptedTurn::new().tool_call(
                        "p0",
                        "create_task",
                        json!({"title": "改 CI", "summary": "x"}).to_string(),
                    ),
                    _ => ScriptedTurn::reply("派好了"),
                };
            }
            if trigger.contains("需要決定：") && outputs.is_empty() {
                let task = trigger
                    .split("task_id：")
                    .nth(1)
                    .and_then(|r| r.split('）').next())
                    .unwrap_or_default()
                    .to_string();
                return ScriptedTurn::new().tool_call(
                    "p1",
                    "post_to_team",
                    json!({"kind": "decision", "text": "用 v2", "to_task": task}).to_string(),
                );
            }
            return ScriptedTurn::reply("看過了");
        }
        if instr.starts_with(WORKER) {
            if outputs.is_empty() {
                seen.lock().unwrap().push(trigger.clone());
            }
            if trigger.contains("用 v2") {
                return match outputs.len() {
                    0 => ScriptedTurn::new().tool_call(
                        "w2",
                        "edit_file",
                        json!({"path": ".github/workflows/ci.yml", "new_text": "on: push\n"})
                            .to_string(),
                    ),
                    1 => ScriptedTurn::new().tool_call(
                        "w3",
                        "open_pr",
                        json!({"title": "CI v2"}).to_string(),
                    ),
                    _ => {
                        assert!(outputs[1].contains(".github"), "{}", outputs[1]);
                        ScriptedTurn::new().tool_call(
                            "w4",
                            "fail_task",
                            json!({"error": "要改 CI 設定，交給使用者"}).to_string(),
                        )
                    }
                };
            }
            return match outputs.len() {
                0 => ScriptedTurn::new().tool_call(
                    "w0",
                    "post_to_team",
                    json!({"kind": "question", "text": "用 v1 還是 v2？"}).to_string(),
                ),
                1 => ScriptedTurn::new().tool_call(
                    "w1",
                    "escalate",
                    json!({"reason": "等 Planner 決定版本"}).to_string(),
                ),
                _ => ScriptedTurn::reply("不該到這裡"),
            };
        }
        chat_turn(req)
    });
    let srv = launch(config(&env, WorkerBackendKind::Builtin, None), model).await;
    let mut c = srv.client().await;
    let project = setup(&mut c).await;
    c.say(
        project,
        &call(
            "create_goal",
            json!({"title": "CI", "brief_md": "## 目標\n\n升級 CI"}),
        ),
        SendMode::Queue,
    )
    .await;
    let g = until_goal(&mut c, |g| {
        g.tasks
            .first()
            .is_some_and(|t| matches!(t.state, TaskStateView::Failed { .. }))
    })
    .await;
    let kinds: Vec<_> = g.team.iter().map(|p| p.kind).collect();
    assert_eq!(kinds, [TeamPostKind::Question, TeamPostKind::Decision]);
    assert_eq!(g.team[1].to.as_deref(), Some("改 CI"));
    let triggers = worker_triggers.lock().unwrap().clone();
    assert_eq!(triggers.len(), 2, "{triggers:?}");
    assert!(
        triggers[1].contains("Planner 回答了：用 v2"),
        "{}",
        triggers[1]
    );
    assert!(env.gh.pulls("me/app").is_empty());
    srv.stop().await;
}

/// 服務在 Claude Code 跑到一半時關閉：run 放回佇列，重啟後用同一個 session 續接並完成。
#[tokio::test]
async fn restart_resumes_interrupted_claude_session() {
    let env = env().await;
    let script = env.dir.join("script.json");
    let log = env.dir.join("claude.log");
    let claude = fake_claude(&env.dir, &script, &log);
    std::fs::write(
        &script,
        json!({
            "new": [{"text": "做到一半"}, {"sleep_ms": 60000}, {"end_turn": "x"}],
            "resume": [{"mcp": {"tool": "complete_task", "args": {"summary": "接著做完"}}}],
        })
        .to_string(),
    )
    .unwrap();
    let model = FnProvider::new("m", chat_turn);
    let srv = launch(
        config(&env, WorkerBackendKind::ClaudeCode, Some(&claude)),
        model.clone(),
    )
    .await;
    let mut c = srv.client().await;
    let project = setup(&mut c).await;
    c.say(
        project,
        &call("quick_fix", json!({"title": "長", "description": "x"})),
        SendMode::Queue,
    )
    .await;
    until_goal(&mut c, |g| {
        g.tasks.first().is_some_and(|t| t.progress == "做到一半")
    })
    .await;
    drop(c);
    srv.stop().await;

    let srv = launch(
        config(&env, WorkerBackendKind::ClaudeCode, Some(&claude)),
        model,
    )
    .await;
    let mut c = srv.client().await;
    c.snapshot().await;
    let g = until_goal(&mut c, |g| g.state != GoalState::Active).await;
    assert_eq!(
        g.state,
        GoalState::Done {
            summary: "接著做完".into()
        }
    );
    let argvs: Vec<String> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|l| l["kind"] == "argv")
        .map(|l| l["argv"].to_string())
        .collect();
    assert_eq!(argvs.len(), 2);
    assert!(argvs[1].contains("--resume="), "{}", argvs[1]);
    // 續接時的觸發寫明上次沒有完成。
    let stdin = std::fs::read_to_string(&log).unwrap();
    assert!(stdin.contains("上一次執行沒有完成"), "{stdin}");
    srv.stop().await;
}

/// 快速修改超出範圍：worker 用 escalate，Goal 升級成一般 Goal（Brief 補上原因），叫醒 Planner。
/// 快速修改不能發問題給 Planner。
#[tokio::test]
async fn quick_fix_escalates_to_planner() {
    let env = env().await;
    let planner_triggers = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = planner_triggers.clone();
    let model = FnProvider::new("m", move |req| {
        let instr = instructions(req);
        let outputs = tool_outputs(req);
        if instr.starts_with(PLANNER) {
            seen.lock().unwrap().push(first_user(req));
            return ScriptedTurn::reply("重新規劃");
        }
        if instr.starts_with(WORKER) {
            assert!(first_user(req).contains("這是快速修改"));
            return match outputs.len() {
                0 => ScriptedTurn::new().tool_call(
                    "q0",
                    "post_to_team",
                    json!({"kind": "question", "text": "要改 API 嗎？"}).to_string(),
                ),
                1 => {
                    assert!(outputs[0].contains("沒有 Planner"), "{}", outputs[0]);
                    ScriptedTurn::new().tool_call(
                        "q1",
                        "escalate",
                        json!({"reason": "要改三個模組的 API"}).to_string(),
                    )
                }
                _ => ScriptedTurn::reply("不該到這裡"),
            };
        }
        chat_turn(req)
    });
    let srv = launch(config(&env, WorkerBackendKind::Builtin, None), model).await;
    let mut c = srv.client().await;
    let project = setup(&mut c).await;
    c.say(
        project,
        &call(
            "quick_fix",
            json!({"title": "改名", "description": "把 foo 改成 bar"}),
        ),
        SendMode::Queue,
    )
    .await;
    let g = until_goal(&mut c, |g| !g.quick && g.planner_note == "重新規劃").await;
    let TaskStateView::NeedsHuman { reason } = &g.tasks[0].state else {
        panic!("{:?}", g.tasks[0].state)
    };
    assert!(reason.contains("要改三個模組的 API"));
    assert!(g.brief.contains("## 升級原因"), "{}", g.brief);
    assert!(
        g.brief.contains("pots/task-") || g.brief.contains("分支"),
        "{}",
        g.brief
    );
    let triggers = planner_triggers.lock().unwrap().clone();
    assert!(triggers.iter().any(|t| t.contains("升級")), "{triggers:?}");
    srv.stop().await;
}

/// 對話 agent 開 explorer subagent 讀 repo 的預設分支（平台的鏡像），結論回到對話 agent。
#[tokio::test]
async fn chat_runs_explorer_subagent_on_mirror() {
    let env = env().await;
    let answers = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = answers.clone();
    let model = FnProvider::new("m", move |req| {
        let instr = instructions(req);
        let outputs = tool_outputs(req);
        if instr.starts_with(EXPLORER) {
            assert!(instr.contains("github.com/me/app 的 main 分支"), "{instr}");
            return match outputs.len() {
                0 => ScriptedTurn::new().tool_call(
                    "e0",
                    "grep",
                    json!({"pattern": "hello"}).to_string(),
                ),
                1 => ScriptedTurn::new().tool_call(
                    "e1",
                    "read_file",
                    json!({"path": "README.md"}).to_string(),
                ),
                _ => ScriptedTurn::reply(format!(
                    "README 寫著：{}（grep：{}）",
                    outputs[1].trim(),
                    outputs[0].trim()
                )),
            };
        }
        if let Some(Item::ToolResult { output, .. }) = req.input.last() {
            seen.lock().unwrap().push(output.clone());
        }
        chat_turn(req)
    });
    let srv = launch(config(&env, WorkerBackendKind::Builtin, None), model).await;
    let mut c = srv.client().await;
    let project = setup(&mut c).await;
    c.say(
        project,
        &call(
            "run_subagent",
            json!({"profile": "explorer", "tasks": [{"prompt": "README 寫什麼？"}]}),
        ),
        SendMode::Queue,
    )
    .await;
    c.until_idle(project).await;
    let answers = answers.lock().unwrap().clone();
    let out = answers.last().expect("subagent 的結果");
    assert!(out.contains("## subagent 1"), "{out}");
    assert!(out.contains("README 寫著：hello pots"), "{out}");
    assert!(out.contains("README.md:1:hello pots"), "{out}");
    srv.stop().await;
}
