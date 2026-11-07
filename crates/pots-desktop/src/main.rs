//! Pots 桌面 app。
//!
//! - 預設：連本機的內嵌服務；服務沒在跑就以子行程啟動自己（`--serve`），關掉視窗後
//!   服務繼續常駐（`proposal/deployment.md#桌面版app-內嵌服務`）。
//! - `--serve [--data-dir <dir>]`：只跑服務，不開視窗。
//! - `--mock`：用假資料看畫面，不連任何服務。
//!
//! 資料目錄預設是系統的應用程式資料目錄下的 `pots`，可用 `POTS_DATA_DIR` 或
//! `--data-dir` 覆寫。

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use gpui_kit::{App, AppContext as _, Bounds, WindowBounds, WindowOptions, px, size};
use pots_desktop::connection::{Endpoint, Target};
use pots_desktop::{app, mock};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let data_dir = arg_value(&args, "--data-dir")
        .map(PathBuf::from)
        .unwrap_or_else(default_data_dir);

    if args.iter().any(|a| a == "--serve") {
        if let Err(e) = pots_server::service::run_blocking(&data_dir) {
            eprintln!("pots 服務結束：{e:#}");
            std::process::exit(1);
        }
        return;
    }
    let mock = args.iter().any(|a| a == "--mock");

    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx: &mut App| {
            gpui_kit::init(cx);
            app::init(cx);

            let bounds = Bounds::centered(None, size(px(1440.), px(900.)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                window.set_window_title("Pots");
                if mock {
                    cx.new(|cx| app::PotsApp::new(mock::store(), window, cx))
                } else {
                    let endpoint = LocalService::new(data_dir.clone());
                    cx.new(|cx| app::PotsApp::new_live(endpoint, window, cx))
                }
            })
            .expect("無法開啟視窗");
            cx.activate(true);
        });
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn default_data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("POTS_DATA_DIR") {
        return dir.into();
    }
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("pots")
}

/// 本機的內嵌服務：讀資料目錄裡的連線資訊，連不上就啟動子行程。
struct LocalService {
    data_dir: PathBuf,
    child: Option<Child>,
}

impl LocalService {
    fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            child: None,
        }
    }

    /// 資料目錄裡的服務現在的狀況。
    fn probe(&self) -> Probe {
        let Some(info) = pots_server::read_connection_info(&self.data_dir) else {
            return Probe::None;
        };
        // `server.json` 可能是上次異常結束留下的，那個 port 現在也許是別的程式在用：
        // 先問不需要 token 的 `/healthz`，實例 ID 對得上才把 token 送過去。
        if !healthz_matches(info.addr, &info.instance_id.to_string()) {
            return Probe::None;
        }
        // 常駐的服務可能是舊版 app 啟動的（關掉視窗後服務繼續跑），協定對不上。
        if !info.compatible_with(&pots_server::BuildInfo::on_disk()) {
            return Probe::Outdated(info);
        }
        Probe::Running(Target {
            addr: info.addr,
            token: info.token.as_str().to_string(),
        })
    }

    fn running(&self) -> Option<Target> {
        match self.probe() {
            Probe::Running(target) => Some(target),
            Probe::None | Probe::Outdated(_) => None,
        }
    }

    /// 請舊版服務正常結束（SIGTERM：取消進行中的 run、刪掉 `server.json`、放開資料目錄鎖），
    /// 等它真的停下來。
    fn stop_outdated(&self, info: &pots_server::ConnectionInfo) -> Result<(), String> {
        tracing::info!(pid = info.pid, "本機服務是舊版，重新啟動");
        #[cfg(unix)]
        {
            let pid = i32::try_from(info.pid).map_err(|_| "pid 不合理".to_string())?;
            // SAFETY: 只是送訊號；pid 來自 server.json，且剛用 /healthz 確認過是這個實例。
            if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
                return Err(format!(
                    "無法結束舊版服務（pid {}）：{}",
                    info.pid,
                    std::io::Error::last_os_error()
                ));
            }
        }
        #[cfg(not(unix))]
        {
            return Err(format!(
                "本機服務是舊版（pid {}），請先結束它再開 app",
                info.pid
            ));
        }
        #[allow(unreachable_code)]
        {
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline {
                if !healthz_matches(info.addr, &info.instance_id.to_string()) {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(150));
            }
            Err(format!("舊版服務（pid {}）沒有在時間內結束", info.pid))
        }
    }

    fn spawn_service(&mut self) -> Result<(), String> {
        let exe = std::env::current_exe().map_err(|e| format!("找不到執行檔：{e}"))?;
        std::fs::create_dir_all(&self.data_dir)
            .map_err(|e| format!("無法建立資料目錄 {}：{e}", self.data_dir.display()))?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path(&self.data_dir))
            .map_err(|e| format!("無法開啟服務日誌：{e}"))?;
        let mut command = Command::new(exe);
        command
            .arg("--serve")
            .arg("--data-dir")
            .arg(&self.data_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log);
        // 自己一個 process group：app 結束時服務不會跟著收到訊號。
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let child = command.spawn().map_err(|e| format!("無法啟動服務：{e}"))?;
        self.child = Some(child);
        Ok(())
    }
}

/// `GET /healthz`，回應裡要有這個實例 ID。
fn healthz_matches(addr: std::net::SocketAddr, instance_id: &str) -> bool {
    use std::io::{Read as _, Write as _};
    let timeout = Duration::from_millis(500);
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&addr, timeout) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let request = format!("GET /healthz HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut response = Vec::new();
    let _ = stream.take(16 * 1024).read_to_end(&mut response);
    let response = String::from_utf8_lossy(&response);
    response.starts_with("HTTP/1.1 200") && response.contains(instance_id)
}

fn log_path(data_dir: &Path) -> PathBuf {
    data_dir.join("server.log")
}

/// [`LocalService::probe`] 的結果。
enum Probe {
    None,
    Running(Target),
    /// 在跑，但和這個 app 的 build 對不上。
    Outdated(pots_server::ConnectionInfo),
}

impl Endpoint for LocalService {
    fn locate(&mut self) -> Result<Target, String> {
        match self.probe() {
            Probe::Running(target) => return Ok(target),
            Probe::Outdated(info) => self.stop_outdated(&info)?,
            Probe::None => {}
        }
        // 上次啟動的子行程還在就等它，結束了就回報並重新啟動。
        let starting = match self.child.as_mut().map(Child::try_wait) {
            Some(Ok(None)) => true,
            Some(Ok(Some(status))) => {
                self.child = None;
                return Err(format!(
                    "服務意外結束（{status}），詳見 {}",
                    log_path(&self.data_dir).display()
                ));
            }
            Some(Err(_)) | None => false,
        };
        if !starting {
            self.spawn_service()?;
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if let Some(target) = self.running() {
                return Ok(target);
            }
            if let Some(Ok(Some(status))) = self.child.as_mut().map(Child::try_wait) {
                self.child = None;
                return Err(format!(
                    "服務啟動失敗（{status}），詳見 {}",
                    log_path(&self.data_dir).display()
                ));
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        Err("服務啟動逾時".into())
    }
}
