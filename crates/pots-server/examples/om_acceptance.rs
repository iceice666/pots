//! 手動驗收 M1 Observational Memory：對真的模型 provider 跑一次「早期做了決定 → 對話累積
//! 超過 100k tokens → 再問那個決定」。不在 CI 裡跑，會花真的 token。
//!
//! ```text
//! POTS_OM_API_KEY=sk-... cargo run -p pots-server --example om_acceptance -- \
//!     --kind openai --base-url https://api.openai.com/v1 --model gpt-4.1-mini \
//!     [--memory-model gpt-4.1-nano] [--rounds 30] [--data-dir ./target/om-acceptance]
//!
//! cargo run -p pots-server --example om_acceptance -- --kind chatgpt [--model gpt-5.1]
//! ```
//!
//! - `--kind`：`openai`（OpenAI 相容）、`anthropic`（Anthropic 相容）、`opencode-zen`／
//!   `opencode-go`（內建目錄）或 `chatgpt`（ChatGPT 方案，
//!   第一次會開瀏覽器登入，授權存在資料目錄，下次沿用）。
//! - API key 從環境變數 `POTS_OM_API_KEY` 讀，不放在命令列；OpenAI 相容的本機服務可以不給。
//!   執行期間 key 存在 `--data-dir` 裡（`0600`），跑完會刪掉這次新增的 provider 與 key；中途中斷時
//!   請自己刪掉資料目錄。
//! - `--memory-model`：Observer／Reflector 用的模型（同一個 provider）；省略時和對話相同。
//! - `--rounds`：填充對話的輪數，每輪約 4000 字（CJK 一字約一個 token）；預設 30 輪約 12 萬 token。
//!
//! 服務用正式的組裝（`service::chatgpt_deps`）與預設閾值，在 `--data-dir`（預設
//! `./target/om-acceptance`）跑；每次建立一個新的 project。結束時印出 agent 的回答、記憶裡的
//! 觀察，以及早期的決定有沒有被整理進觀察。

use std::path::PathBuf;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use pots_proto::{
    AgentState, ClientCommand, ItemId, MemoryView, ModelAuthView, ProjectId, ProviderDraft,
    ProviderId, ProviderKind, SendMode, ServerEvent, StreamItemKind,
};
use pots_server::{Config, service};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type BoxError = Box<dyn std::error::Error>;

const EARLY: &str = "先記下幾個決定：資料庫用 SQLite（不用 Postgres），服務的 port 固定用 7421，\
    專案代號叫「藍鰭」。之後的討論都照這個來。這則只要回覆「收到」。";
const QUESTION: &str = "我們最一開始決定的資料庫、服務 port 和專案代號分別是什麼？只回答這三項。";
/// 一則填充訊息的目標字數。
const FILLER_CHARS: usize = 4000;
/// 一則訊息最多等多久。
const REPLY_TIMEOUT: Duration = Duration::from_secs(600);

struct Args {
    kind: ProviderKind,
    base_url: String,
    model: String,
    memory_model: String,
    rounds: usize,
    data_dir: PathBuf,
}

fn parse_args() -> Result<Args, BoxError> {
    let mut args = Args {
        kind: ProviderKind::OpenAiCompatible,
        base_url: String::new(),
        model: String::new(),
        memory_model: String::new(),
        rounds: 30,
        data_dir: PathBuf::from("./target/om-acceptance"),
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} 需要一個值"));
        match flag.as_str() {
            "--kind" => {
                args.kind = match value()?.as_str() {
                    "openai" => ProviderKind::OpenAiCompatible,
                    "responses" => ProviderKind::OpenAiResponses,
                    "anthropic" => ProviderKind::AnthropicCompatible,
                    "opencode-zen" => ProviderKind::OpenCodeZen,
                    "opencode-go" => ProviderKind::OpenCodeGo,
                    "chatgpt" => ProviderKind::ChatGpt,
                    other => return Err(format!("不認得的 --kind：{other}").into()),
                }
            }
            "--base-url" => args.base_url = value()?,
            "--model" => args.model = value()?,
            "--memory-model" => args.memory_model = value()?,
            "--rounds" => args.rounds = value()?.parse()?,
            "--data-dir" => args.data_dir = PathBuf::from(value()?),
            "-h" | "--help" => {
                println!(
                    "用法：om_acceptance --kind openai|responses|anthropic|opencode-zen|opencode-go|chatgpt [--base-url URL] \
                     [--model M] [--memory-model M] [--rounds N] [--data-dir DIR]\n\
                     API key 放在環境變數 POTS_OM_API_KEY。"
                );
                std::process::exit(0);
            }
            other => return Err(format!("不認得的參數：{other}").into()),
        }
    }
    if args.kind != ProviderKind::ChatGpt {
        if args.base_url.is_empty() {
            args.base_url = args.kind.default_base_url().into();
        }
        if args.model.is_empty() {
            return Err("API provider 需要 --model".into());
        }
    }
    Ok(args)
}

struct Client {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    /// 最新的記憶（`Memory` 事件）。
    memory: Option<MemoryView>,
    project: Option<ProjectId>,
}

impl Client {
    async fn send(&mut self, cmd: &ClientCommand) -> Result<(), BoxError> {
        self.ws
            .send(Message::Text(serde_json::to_string(cmd)?.into()))
            .await?;
        Ok(())
    }

    /// 下一個事件；`wait` 內沒有事件時是 `None`。
    async fn next(&mut self, wait: Duration) -> Result<Option<ServerEvent>, BoxError> {
        loop {
            let msg = match tokio::time::timeout(wait, self.ws.next()).await {
                Err(_) => return Ok(None),
                Ok(None) => return Err("連線被關閉".into()),
                Ok(Some(m)) => m?,
            };
            if let Message::Text(text) = msg {
                let ev: ServerEvent = serde_json::from_str(&text)?;
                match &ev {
                    ServerEvent::Memory {
                        project, memory, ..
                    } if Some(*project) == self.project => {
                        self.memory = Some(memory.clone());
                    }
                    ServerEvent::Error { message } => eprintln!("服務回報錯誤：{message}"),
                    _ => {}
                }
                return Ok(Some(ev));
            }
        }
    }

    async fn until(
        &mut self,
        wait: Duration,
        mut pred: impl FnMut(&ServerEvent) -> bool,
    ) -> Result<ServerEvent, BoxError> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match self.next(left).await? {
                Some(ev) if pred(&ev) => return Ok(ev),
                Some(_) => {}
                None => return Err("等太久了".into()),
            }
        }
    }

    /// 傳一則訊息，回傳 agent 的回覆全文。
    async fn say(&mut self, project: ProjectId, text: &str) -> Result<String, BoxError> {
        self.send(&ClientCommand::Send {
            project,
            id: ItemId::new(),
            text: text.into(),
            mode: SendMode::Queue,
        })
        .await?;
        let mut reply = String::new();
        loop {
            let ev = self
                .until(REPLY_TIMEOUT, |e| {
                    matches!(
                        e,
                        ServerEvent::ItemDone { .. } | ServerEvent::AgentState { .. }
                    )
                })
                .await?;
            match ev {
                ServerEvent::ItemDone { item, .. } => {
                    if let StreamItemKind::Agent(m) = item.kind {
                        reply = m.text;
                    }
                }
                ServerEvent::AgentState {
                    project: p,
                    state: AgentState::Idle,
                    ..
                } if p == project => return Ok(reply),
                _ => {}
            }
        }
    }
}

fn filler(round: usize) -> String {
    let topics = [
        "整理客服常見問題的分類方式",
        "比較幾種排程寫法的優缺點",
        "回顧上週的錯誤日誌與處理過程",
        "討論文件網站的導覽列要放哪些項目",
        "列出新人上手時常卡住的地方",
    ];
    let topic = topics[round % topics.len()];
    let mut out = format!(
        "第 {round} 段背景資料：{topic}。這段只是閒聊與背景，沒有新的決定，只要回覆「收到」。\n"
    );
    let mut n = 0;
    while out.chars().count() < FILLER_CHARS {
        n += 1;
        out.push_str(&format!(
            "第 {n} 點：關於{topic}，我們聊到第 {} 個例子，大家各自說了看法，有人覺得要再觀察一陣子，\
             有人覺得先記下來之後再說，目前沒有結論，也不需要現在決定。\n",
            round * 100 + n
        ));
    }
    out
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let args = parse_args()?;
    service::init_tracing();
    let api_key = std::env::var("POTS_OM_API_KEY")
        .ok()
        .filter(|k| !k.is_empty());
    println!(
        "這會送出約 {} 萬字的對話（{} 輪），請確認 provider 的費用。",
        args.rounds * FILLER_CHARS / 10_000,
        args.rounds
    );

    std::fs::create_dir_all(&args.data_dir)?;
    let deps = service::chatgpt_deps(&args.data_dir, None).await?;
    let handle = pots_server::start(Config::desktop(&args.data_dir), deps).await?;
    let mut req = format!("ws://{}/ws", handle.addr).into_client_request()?;
    req.headers_mut().insert(
        "authorization",
        format!("Bearer {}", handle.token.as_str()).parse()?,
    );
    let (ws, _) = tokio_tungstenite::connect_async(req).await?;
    let mut c = Client {
        ws,
        memory: None,
        project: None,
    };
    c.send(&ClientCommand::Resume { last_seq: 0 }).await?;
    let ServerEvent::Snapshot { instance, .. } = c
        .until(Duration::from_secs(10), |e| {
            matches!(e, ServerEvent::Snapshot { .. })
        })
        .await?
    else {
        unreachable!()
    };

    // 設定 provider。
    let provider = if args.kind == ProviderKind::ChatGpt {
        if !matches!(instance.model_auth, ModelAuthView::SignedIn { .. }) {
            c.send(&ClientCommand::StartModelLogin).await?;
            if let ServerEvent::LoginUrl { url } = c
                .until(Duration::from_secs(30), |e| {
                    matches!(e, ServerEvent::LoginUrl { .. })
                })
                .await?
            {
                println!("在瀏覽器完成 ChatGPT 登入（5 分鐘內）。");
                if std::process::Command::new("open")
                    .arg(&url)
                    .status()
                    .is_err()
                {
                    println!("請手動開啟：{url}");
                }
            }
            c.until(Duration::from_secs(300), |e| {
                matches!(
                    e,
                    ServerEvent::ModelAuth {
                        state: ModelAuthView::SignedIn { .. },
                        ..
                    }
                )
            })
            .await?;
        }
        if !args.model.is_empty() {
            c.send(&ClientCommand::SaveProvider {
                draft: ProviderDraft {
                    id: Some(ProviderId::CHATGPT),
                    kind: ProviderKind::ChatGpt,
                    name: String::new(),
                    base_url: String::new(),
                    model: args.model.clone(),
                    api_key: None,
                    headers: Vec::new(),
                },
            })
            .await?;
            c.until(Duration::from_secs(10), |e| {
                matches!(e, ServerEvent::Providers { .. })
            })
            .await?;
        }
        ProviderId::CHATGPT
    } else {
        let name = format!("om-acceptance {}", pots_proto::Timestamp::now().0);
        c.send(&ClientCommand::SaveProvider {
            draft: ProviderDraft {
                id: None,
                kind: args.kind,
                name: name.clone(),
                base_url: args.base_url.clone(),
                model: args.model.clone(),
                api_key,
                headers: args
                    .kind
                    .preset()
                    .map(|p| p.headers.clone())
                    .unwrap_or_default(),
            },
        })
        .await?;
        let ServerEvent::Providers { providers, .. } = c
            .until(Duration::from_secs(10), |e| {
                matches!(e, ServerEvent::Providers { .. })
            })
            .await?
        else {
            unreachable!()
        };
        providers
            .items
            .iter()
            .find(|p| p.name == name)
            .map(|p| p.id)
            .ok_or("新增 provider 失敗")?
    };
    c.send(&ClientCommand::SetActiveProvider { id: provider })
        .await?;
    c.until(Duration::from_secs(10), |e| {
        matches!(e, ServerEvent::Providers { .. })
    })
    .await?;
    c.send(&ClientCommand::SetMemoryModel {
        provider: (!args.memory_model.is_empty()).then_some(provider),
        model: args.memory_model.clone(),
    })
    .await?;
    c.until(Duration::from_secs(10), |e| {
        matches!(e, ServerEvent::Providers { .. })
    })
    .await?;

    let name = format!("OM 驗收 {}", pots_proto::Timestamp::now().0);
    c.send(&ClientCommand::CreateProject { name }).await?;
    let ServerEvent::ProjectAdded { project, .. } = c
        .until(Duration::from_secs(10), |e| {
            matches!(e, ServerEvent::ProjectAdded { .. })
        })
        .await?
    else {
        unreachable!()
    };
    let project = project.id;
    c.project = Some(project);

    println!("早期的決定：{EARLY}");
    println!("agent：{}", c.say(project, EARLY).await?);
    let mut sent = EARLY.chars().count();
    for round in 0..args.rounds {
        let text = filler(round);
        sent += text.chars().count();
        let reply = c.say(project, &text).await?;
        let observations = c.memory.as_ref().map_or(0, |m| m.observations.len());
        println!(
            "第 {:>2}/{} 輪：累計約 {sent} 字，記憶 {observations} 條；agent：{}",
            round + 1,
            args.rounds,
            reply.lines().next().unwrap_or("")
        );
    }

    // 等背景整理告一段落：20 秒內沒有新事件，最多等 5 分鐘。
    println!("等背景整理記憶…");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    while tokio::time::Instant::now() < deadline {
        if c.next(Duration::from_secs(20)).await?.is_none() {
            break;
        }
    }

    println!("\n問：{QUESTION}");
    let answer = c.say(project, QUESTION).await?;
    println!("答：{answer}\n");

    let memory = c.memory.clone().unwrap_or_default();
    println!(
        "記憶：第 {} 代，{} 條，約 {} token；目前在做什麼：{}",
        memory.generation,
        memory.observations.len(),
        memory.tokens,
        memory.current_task
    );
    if let Some(e) = &memory.error {
        println!("上次整理失敗：{e}");
    }
    for o in &memory.observations {
        println!("  - {} · {} · {}", o.date, o.priority.label(), o.text);
    }
    let observed = memory
        .observations
        .iter()
        .any(|o| o.text.contains("7421") || o.text.contains("藍鰭"));
    let answered = ["SQLite", "7421", "藍鰭"]
        .iter()
        .all(|k| answer.contains(k));
    println!(
        "\n早期決定{}整理進觀察；回答{}三項都對。",
        if observed { "已" } else { "沒有" },
        if answered { "" } else { "沒有" }
    );
    println!("{}", if observed && answered { "PASS" } else { "FAIL" });

    // 這次新增的 API provider 連同存在資料目錄的 key 一起刪掉。
    if provider != ProviderId::CHATGPT {
        c.send(&ClientCommand::DeleteProvider { id: provider })
            .await?;
        c.until(Duration::from_secs(10), |e| {
            matches!(e, ServerEvent::Providers { .. })
        })
        .await?;
    }
    handle.shutdown();
    handle.wait().await?;
    if !(observed && answered) {
        std::process::exit(1);
    }
    Ok(())
}
