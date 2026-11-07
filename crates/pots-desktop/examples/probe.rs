//! 不開視窗、直接用桌面客戶端的連線模組和服務說話，方便除錯協定。
//!
//! ```sh
//! cargo run -p pots-desktop -- --serve --data-dir /tmp/pots-probe &   # 先起服務
//! cargo run -p pots-desktop --example probe -- /tmp/pots-probe [指令...]
//! ```
//!
//! 指令：`project <名稱>`、`send <文字>`（送到最後一個 project）、`login`、`logout`、
//! `wait <秒>`。印出收到的事件，不會印出 token。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt as _;
use pots_desktop::connection::{self, ConnEvent, Endpoint, Target};
use pots_desktop::model::*;

struct FromDataDir(PathBuf);

impl Endpoint for FromDataDir {
    fn locate(&mut self) -> Result<Target, String> {
        let info = pots_server::read_connection_info(&self.0).ok_or("找不到 server.json")?;
        Ok(Target {
            addr: info.addr,
            token: info.token.as_str().to_string(),
        })
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(args.next().expect("要給資料目錄"));
    let script: Vec<String> = args.collect();

    let (conn, mut events) = connection::spawn(FromDataDir(dir));
    let latest: Arc<Mutex<Option<ProjectId>>> = Arc::default();
    let seen = latest.clone();
    std::thread::spawn(move || {
        futures::executor::block_on(async {
            while let Some(event) = events.next().await {
                match &event {
                    ConnEvent::Server(ServerEvent::Snapshot { instance, .. }) => {
                        *seen.lock().unwrap() = instance.projects.last().map(|p| p.id);
                    }
                    ConnEvent::Server(ServerEvent::ProjectAdded { project, .. }) => {
                        *seen.lock().unwrap() = Some(project.id);
                    }
                    _ => {}
                }
                println!("{}", describe(&event));
            }
        });
    });
    let last_project = || *latest.lock().unwrap();

    std::thread::sleep(Duration::from_millis(500));
    let mut i = 0;
    while i < script.len() {
        match script[i].as_str() {
            "project" => {
                i += 1;
                conn.send(ClientCommand::CreateProject {
                    name: script[i].clone(),
                });
            }
            "send" => {
                i += 1;
                match last_project() {
                    Some(project) => conn.send(ClientCommand::Send {
                        project,
                        id: ItemId::new(),
                        text: script[i].clone(),
                        mode: SendMode::Queue,
                    }),
                    None => println!("!! 還沒有 project"),
                }
            }
            "login" => conn.send(ClientCommand::StartModelLogin),
            "logout" => conn.send(ClientCommand::ModelLogout),
            "wait" => {
                i += 1;
                let secs: f64 = script[i].parse().unwrap_or(1.0);
                std::thread::sleep(Duration::from_secs_f64(secs));
            }
            other => println!("!! 不認得的指令 {other}"),
        }
        i += 1;
        std::thread::sleep(Duration::from_millis(300));
    }
    std::thread::sleep(Duration::from_secs(2));
    drop(conn);
}

fn describe(event: &ConnEvent) -> String {
    match event {
        ConnEvent::State(s) => format!("[連線] {s:?}"),
        ConnEvent::Problem(p) => format!("[問題] {p}"),
        ConnEvent::Server(ev) => {
            let json = serde_json::to_string(ev).unwrap_or_default();
            // 登入網址很長，只留開頭。
            let mut json: String = json.chars().take(300).collect();
            if json.len() < serde_json::to_string(ev).unwrap_or_default().len() {
                json.push('…');
            }
            format!("[事件] {json}")
        }
    }
}
