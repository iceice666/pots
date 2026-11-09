//! `pots-server`：獨立執行的 Pots 服務。
//!
//! ```text
//! pots-server [--data-dir <path>] [--port <port>]
//! ```
//!
//! 預設資料目錄是平台的資料目錄（macOS：`~/Library/Application Support/pots`）。
//! 連線資訊（位址與 token）寫在資料目錄的 `server.json`，不會出現在日誌裡。
//! 模型用 ChatGPT 方案（SIWC 授權），`POTS_CHAT_MODEL` 可指定模型 slug。

use std::path::PathBuf;
use std::process::ExitCode;

use pots_server::{Config, Error};

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

#[tokio::main]
async fn main() -> ExitCode {
    pots_server::service::init_tracing();

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

    let model = std::env::var("POTS_CHAT_MODEL")
        .ok()
        .filter(|m| !m.trim().is_empty());
    let config = Config::desktop(&args.data_dir).with_port(args.port);
    let code = match pots_server::service::run(config, model).await {
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
    };
    pots_server::service::shutdown_tracing();
    code
}
