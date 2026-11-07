//! `pots-server`：獨立執行的 Pots 服務。
//!
//! ```text
//! pots-server [--data-dir <path>] [--port <port>]
//! ```
//!
//! 預設資料目錄是平台的資料目錄（macOS：`~/Library/Application Support/pots`）。
//! 連線資訊（位址與 token）寫在資料目錄的 `server.json`，不會出現在日誌裡。

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use pots_llm::{EventStream, LlmError, ModelProvider, ModelRequest};
use pots_server::{Config, Deps, Error, FakeModelAuth};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "用法：pots-server [--data-dir <路徑>] [--port <port>]";

struct Args {
    data_dir: PathBuf,
    port: u16,
}

fn parse_args() -> Result<Args, String> {
    let mut data_dir = None;
    let mut port = 0;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data-dir" => {
                data_dir = Some(PathBuf::from(args.next().ok_or("--data-dir 需要路徑")?));
            }
            "--port" => {
                let v = args.next().ok_or("--port 需要數字")?;
                port = v
                    .parse()
                    .map_err(|_| format!("--port 不是合法的 port：{v}"))?;
            }
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("不認得的參數：{other}")),
        }
    }
    let data_dir = match data_dir {
        Some(d) => d,
        None => dirs::data_dir()
            .ok_or("找不到平台的資料目錄，請用 --data-dir 指定")?
            .join("pots"),
    };
    Ok(Args { data_dir, port })
}

/// 還沒接上 ChatGPT（SIWC）時的 provider：每次推論都回「需要登入」。
struct NotSignedIn;

#[async_trait::async_trait]
impl ModelProvider for NotSignedIn {
    fn name(&self) -> &str {
        "unconfigured"
    }

    async fn stream(
        &self,
        _request: ModelRequest,
        _cancel: CancellationToken,
    ) -> Result<EventStream, LlmError> {
        Err(LlmError::NeedsLogin)
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            if !msg.is_empty() {
                eprintln!("{msg}");
            }
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    // TODO(parent)：接上真正的 SIWC：用 `pots_llm` 從 data_dir 載入 host ID、建立 SiwcAuth
    // 與 ResponsesProvider，再把 SiwcAuth 包成 `ModelAuthControl`。在那之前推論一律回
    // 「需要登入」，登入按鈕只會顯示假的網址。
    let deps = Deps {
        provider: Arc::new(NotSignedIn),
        model: "gpt-5".into(),
        model_auth: Arc::new(FakeModelAuth::signed_out()),
    };
    let config = Config::desktop(&args.data_dir).with_port(args.port);
    match pots_server::run_until_signal(config, deps).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(Error::AlreadyRunning(info)) => {
            match info {
                Some(info) => eprintln!(
                    "這個資料目錄已經有 Pots 服務在執行（pid {}，{}）",
                    info.pid, info.addr
                ),
                None => eprintln!("這個資料目錄已經有 Pots 服務在執行"),
            }
            ExitCode::from(3)
        }
        Err(e) => {
            eprintln!("Pots 服務失敗：{e}");
            ExitCode::FAILURE
        }
    }
}
