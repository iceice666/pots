//! 手動 smoke test：對正式的 `auth.openai.com`／`api.openai.com` 實際跑一次
//! Sign in with ChatGPT 與 Responses 推論。不在 CI 裡跑。
//!
//! 用法：
//!
//! ```text
//! cargo run -p pots-llm --example siwc_smoke -- [DATA_DIR] [MODEL]
//! cargo run -p pots-llm --example siwc_smoke -- [DATA_DIR] --logout
//! ```
//!
//! - `DATA_DIR`：Host ID 與憑證存放的目錄，預設 `./target/siwc-smoke`（檔案權限 0600）。
//! - `MODEL`：模型 slug；省略時用模型清單的第一個。
//! - `--logout`：撤銷 refresh token、清掉本機 token（保留 issued client ID 與 Host ID）。
//!
//! 流程：載入／建立 Host ID 與憑證 → 沒登入就開瀏覽器登入（macOS 用 `open`）→ 列出模型 →
//! 串流一句招呼 → 用 `get_time` 工具做一次工具往返。不會印出任何 token；授權 URL 只在
//! 瀏覽器打不開、必須手動開啟時才完整印出（且不含 ID token 提示），其他地方只印遮蔽過的版本。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use pots_llm::agent::{self, AgentEvent, AllowAll, RunConfig, Tool, ToolError};
use pots_llm::responses::{DEFAULT_BASE_URL, ResponsesProvider};
use pots_llm::siwc::{
    AuthStatus, FileCredentialStore, SiwcAuth, SiwcConfig, load_or_create_host_id,
    redact_authorize_url,
};
use pots_llm::{Item, ModelProvider, ModelRequest, StreamEvent, ToolSpec};
use tokio_util::sync::CancellationToken;
use url::Url;

type BoxError = Box<dyn std::error::Error>;

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let mut positional = Vec::new();
    let mut logout = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--logout" => logout = true,
            "-h" | "--help" => {
                println!("用法：siwc_smoke [DATA_DIR] [MODEL] [--logout]");
                return Ok(());
            }
            _ => positional.push(arg),
        }
    }
    let data_dir = positional
        .first()
        .map_or_else(|| PathBuf::from("./target/siwc-smoke"), PathBuf::from);
    let model_arg = positional.get(1).cloned();

    let host_id = load_or_create_host_id(&data_dir.join("host-id"))?;
    println!("資料目錄：{}", data_dir.display());
    println!("Host ID：{host_id}");
    let store = Arc::new(FileCredentialStore::new(&data_dir));
    let http = pots_llm::reqwest::Client::builder().build()?;
    let auth = SiwcAuth::open(SiwcConfig::new(host_id), store, http.clone()).await?;

    if logout {
        let outcome = auth.logout().await?;
        println!("已登出：{outcome:?}");
        return Ok(());
    }

    if !matches!(*auth.status().borrow(), AuthStatus::SignedIn { .. }) {
        sign_in(&auth).await?;
    }
    match auth.status().borrow().clone() {
        AuthStatus::SignedIn { email, plan_usage } => {
            println!("已登入：{}", email.as_deref().unwrap_or("（沒有 email）"));
            if !plan_usage {
                println!("這次授權沒有 chatgpt.tokens.use.direct，不能用 ChatGPT plan 推論。");
                return Ok(());
            }
        }
        other => return Err(format!("登入狀態：{other:?}").into()),
    }

    let provider = Arc::new(ResponsesProvider::new(
        Url::parse(DEFAULT_BASE_URL)?,
        auth.clone(),
        http,
    ));
    let models = provider.list_models().await?;
    println!("\n可用模型：");
    for (index, model) in models.iter().enumerate() {
        println!("  {index}. {}（{}）", model.slug, model.display_name);
    }
    let model = match model_arg {
        Some(model) => model,
        None => models
            .first()
            .map(|m| m.slug.clone())
            .ok_or("模型清單是空的")?,
    };
    println!("使用模型：{model}");

    println!("\n== 串流 ==");
    let request = ModelRequest {
        reasoning: None,
        model: model.clone(),
        instructions: Some("You are a friendly assistant.".into()),
        input: vec![Item::User {
            text: "Say hello in one short sentence.".into(),
        }],
        tools: vec![],
        session_id: None,
    };
    let mut stream = provider.stream(request, CancellationToken::new()).await?;
    while let Some(event) = stream.next().await {
        match event? {
            StreamEvent::TextDelta(delta) => print!("{delta}"),
            StreamEvent::ItemDone(_) => {}
            StreamEvent::Completed { usage } => println!(
                "\n（完成；input {} / output {} tokens）",
                usage.input_tokens, usage.output_tokens
            ),
        }
    }

    println!("\n== 工具往返 ==");
    let config = RunConfig {
        reasoning: None,
        provider: provider.clone(),
        model,
        instructions: Some(
            "You are a terse assistant. When asked about the time, call the get_time tool.".into(),
        ),
        tools: vec![Arc::new(GetTime)],
        hook: Arc::new(AllowAll),
        max_turns: 4,
        session_id: None,
    };
    let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let printer = tokio::spawn(async move {
        while let Some(event) = receiver.recv().await {
            match event {
                AgentEvent::TextDelta(delta) => print!("{delta}"),
                AgentEvent::ToolStarted(call) => {
                    println!("[工具] {}({})", call.name, call.arguments);
                }
                AgentEvent::ToolFinished { call, ok } => {
                    println!("[工具] {} 結束，ok = {ok}", call.name);
                }
                AgentEvent::TurnCompleted { turn, usage } => println!(
                    "\n[第 {turn} 輪完成；input {} / output {} tokens]",
                    usage.input_tokens, usage.output_tokens
                ),
                AgentEvent::Item(_) => {}
            }
        }
    });
    let result = agent::run(
        &config,
        vec![Item::User {
            text: "What time is it in UTC right now? Use the tool.".into(),
        }],
        events,
        CancellationToken::new(),
    )
    .await;
    drop(config);
    let _ = printer.await;
    let result = result?;
    println!(
        "結果：{:?}，新增 {} 筆歷史",
        result.outcome,
        result.new_items.len()
    );
    Ok(())
}

/// 開瀏覽器登入，等到登入完成或失敗。
async fn sign_in(auth: &Arc<SiwcAuth>) -> Result<(), BoxError> {
    let url = auth.start_login().await?;
    let has_hint = Url::parse(&url)?
        .query_pairs()
        .any(|(key, _)| key == "id_token_hint");
    if open_browser(&url) {
        println!("已在瀏覽器開啟登入頁：{}", redact_authorize_url(&url));
    } else if !has_hint {
        // 只有這裡必須印出完整 URL：使用者要自己開。
        println!("無法開啟瀏覽器，請手動開啟下列網址完成登入：\n{url}");
    } else {
        return Err(
            "無法開啟瀏覽器，而登入 URL 含 ID token 提示不便印出；請先 --logout 再試".into(),
        );
    }
    let mut status = auth.status();
    let status = status
        .wait_for(|s| !matches!(s, AuthStatus::Pending { .. }))
        .await?
        .clone();
    match status {
        AuthStatus::SignedIn { .. } => Ok(()),
        other => Err(format!("登入沒有完成：{other:?}").into()),
    }
}

fn open_browser(url: &str) -> bool {
    let command = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    std::process::Command::new(command)
        .arg(url)
        .status()
        .is_ok_and(|status| status.success())
}

/// 回傳現在的 UTC 時間（HH:MM:SS）。
struct GetTime;

#[async_trait::async_trait]
impl Tool for GetTime {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "get_time".into(),
            description: "Get the current time in UTC.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, _arguments: &str) -> Result<String, ToolError> {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| ToolError(e.to_string()))?
            .as_secs()
            % 86_400;
        Ok(format!(
            "{:02}:{:02}:{:02} UTC",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        ))
    }
}
