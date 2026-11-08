//! Claude Code adapter：以雙向 stream-json 驅動未修改的官方 `claude` CLI
//! （`proposal/agent-runtime.md#worker-adapter內建-worker-與外部-coding-cli`）。
//!
//! 一次 [`run`] 是一個 CLI 行程：
//!
//! 1. 在 `run.command` 後面附加參數（[`cli_args`]，不經 shell），stdio 全部接管；本機行程放進
//!    自己的行程群組，必要時整組砍掉。
//! 2. 送 `initialize` control request，等到回應（[`INIT_TIMEOUT`]）才送第一則使用者訊息；
//!    stdin 保持開著，之後的 interrupt、權限回覆與提醒都從這裡送。寫入由獨立的 task 負責，
//!    不會卡住讀 stdout。
//! 3. stdout 是 NDJSON：空行與非 JSON 的雜訊略過（debug log），不認得的類型與欄位一律容忍；
//!    單行超過 [`MAX_LINE_BYTES`] 時整行丟掉並記 warn（不當成失敗——如果丟掉的剛好是
//!    `result`，最後會以「沒有 result」的協定錯誤收場）。stderr 同時讀掉，只留最後
//!    [`STDERR_TAIL_BYTES`] 給錯誤訊息，不寫進 info log。
//! 4. 結束條件：
//!    - 平台的結束工具成功（`finished` 變成 `true` 並通知 `stop`）或 `cancel` 觸發：送
//!      interrupt，繼續讀到下一個 `result` 或 `grace` 用完，然後關 stdin、等行程結束，
//!      不結束就 SIGTERM → SIGKILL 整個群組並呼叫 `hard_stop`；分別是
//!      [`ClaudeEnd::Finished`]、[`ClaudeEnd::Cancelled`]。
//!    - 一輪結束（`result`）：`finished` 已經是 `true` → `Finished`；CLI 回報錯誤 → `Failed`；
//!      否則有提醒且還沒用過就送一次提醒繼續，不然關 stdin → [`ClaudeEnd::TurnEnded`]。
//!    - stdout 結束卻沒有 `result`：非零結束碼是 [`ClaudeFailure::Exit`]，零是
//!      [`ClaudeFailure::Protocol`]。
//!
//! 錯誤分類（[`classify`]）優先看結構化欄位（assistant 的 `error`、`rate_limit_event` 的
//! `status`、`result.api_error_status`），英文訊息只是後備。續接不存在的 session 時 CLI 會以
//! 錯誤 `result`（`No conversation found with session ID: …`）結束，這裡回 [`ClaudeFailure::Cli`]，
//! 由呼叫端決定要不要改開新的 session。
//!
//! 統計：`cost_usd` 取最後一個 `result` 的 `total_cost_usd`（CLI 回報的是整個行程的累計），
//! `num_turns` 是各個 `result` 的 `num_turns` 加總（每輪各自計算）。

use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// 等 `initialize` 回應的上限。
const INIT_TIMEOUT: Duration = Duration::from_secs(30);
/// stdout 單行（一筆紀錄）的上限；大型圖片或工具輸出也放得下。
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;
/// 保留的 stderr 尾巴。
const STDERR_TAIL_BYTES: usize = 4 * 1024;
/// SIGTERM 之後等多久才 SIGKILL。
const TERM_WAIT: Duration = Duration::from_secs(1);
/// SIGKILL 之後等行程被回收的上限。
const KILL_WAIT: Duration = Duration::from_secs(5);
/// 工具摘要最多幾個字。
const SUMMARY_CHARS: usize = 120;
/// 錯誤訊息最多幾個字。
const MAX_ERROR_CHARS: usize = 2000;
/// 雜訊行在 debug log 裡最多留幾個字。
const NOISE_CHARS: usize = 200;

const INIT_REQUEST_ID: &str = "init-1";
const INTERRUPT_REQUEST_ID: &str = "interrupt-1";

/// 這次 run 的 session：新開（由平台先產生 ID，在啟動前就持久化）或續接。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionMode {
    New(String),
    Resume(String),
}

/// 原生工具的權限決定（`can_use_tool`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Permission {
    Allow,
    Deny(String),
}

pub type PermissionPolicy = Arc<dyn Fn(&str, &Value) -> Permission + Send + Sync>;

/// 啟動一次 CLI 所需的一切。
pub struct ClaudeRun {
    /// 已設好程式（`claude` 或 `docker exec -i <容器> claude`）、工作目錄與環境變數的指令；
    /// adapter 只附加 CLI 參數並設定 stdio。
    pub command: tokio::process::Command,
    pub session: SessionMode,
    /// 第一則使用者訊息（任務、觸發原因與平台規則）。
    pub prompt: String,
    /// 附加在 Claude Code 預設 system prompt 之後的 worker 規則。
    pub append_system_prompt: String,
    /// CLI 看得到的 MCP 設定檔路徑（容器內路徑）。
    pub mcp_config_path: String,
    /// `--setting-sources` 的值：本機用你自己的設定是 `user`；容器裡共用的 home 不可信，給空字串
    /// （不讀任何 settings.json），改用平台產生的 `settings_path`。
    pub setting_sources: String,
    /// 平台產生的設定檔（`--settings`，例如關掉所有 hooks）。
    pub settings_path: Option<String>,
    pub model: Option<String>,
    /// 自動核准的平台工具，例如 `mcp__pots__complete_task`。
    pub allowed_tools: Vec<String>,
    /// 一律禁止的原生工具。
    pub disallowed_tools: Vec<String>,
    /// 原生工具的權限判斷。
    pub policy: PermissionPolicy,
    /// 這輪結束（`result`）但平台的結束工具還沒被呼叫時，再送一次的提醒；`None` 表示不提醒。
    pub nudge: Option<String>,
    /// 平台的結束工具（`complete_task`、`wait_for`…）成功時設為 `true` 並通知 `stop`。
    pub finished: Arc<AtomicBool>,
    pub stop: Arc<Notify>,
    /// 中斷後等 CLI 收尾的時間。
    pub grace: Duration,
    /// 行程不肯結束時的最後手段（例如 `docker rm -f`）；本機行程由 adapter 自己殺整個群組。
    pub hard_stop: Option<Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send>>,
}

/// 給狀態面板與任務步驟的事件。
#[derive(Clone, Debug, PartialEq)]
pub enum ClaudeEvent {
    /// `system/init`：session 與 CLI 版本。
    Started {
        session_id: String,
        version: Option<String>,
    },
    /// assistant 的文字（完整的一段，不是增量）。
    Text(String),
    /// 使用工具；`summary` 是給人看的一行。
    ToolUse { name: String, summary: String },
    /// 原生工具的權限決定。
    PermissionDecided { tool: String, allowed: bool },
}

#[derive(Clone, Debug, PartialEq)]
pub enum ClaudeEnd {
    /// 平台的結束工具被呼叫，CLI 已收尾。
    Finished,
    /// CLI 結束這輪（含提醒之後）仍沒有呼叫結束工具。
    TurnEnded,
    /// 被取消（取消任務、Goal 結束、逾時、服務關閉）。
    Cancelled,
    Failed(ClaudeFailure),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ClaudeFailure {
    /// 需要使用者重新登入 Claude Code。
    NeedsLogin(String),
    /// 訂閱額度或用量上限。
    UsageLimit(String),
    /// CLI 回報的其他錯誤（`result.is_error`、assistant 的 `error`）。
    Cli(String),
    /// 輸出不合協定（例如沒有 `result` 就結束）。
    Protocol(String),
    /// 行程異常結束。
    Exit { code: Option<i32>, stderr: String },
    /// 無法啟動。
    Spawn(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClaudeOutcome {
    pub end: ClaudeEnd,
    pub session_id: Option<String>,
    pub version: Option<String>,
    /// CLI 的成本估計（不是帳單）。
    pub cost_usd: Option<f64>,
    pub num_turns: u32,
    /// 最後一段 assistant 文字。
    pub last_text: String,
}

/// 執行一次 CLI 直到結束。
pub async fn run(
    run: ClaudeRun,
    events: mpsc::UnboundedSender<ClaudeEvent>,
    cancel: CancellationToken,
) -> ClaudeOutcome {
    let args = cli_args(&run);
    let ClaudeRun {
        mut command,
        prompt,
        policy,
        mut nudge,
        finished,
        stop,
        grace,
        hard_stop,
        ..
    } = run;
    let mut state = State::new(events);

    command
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut proc = match Proc::spawn(&mut command) {
        Ok(proc) => proc,
        Err(e) => return state.outcome(ClaudeEnd::Failed(ClaudeFailure::Spawn(e.to_string()))),
    };

    proc.send(control_request(
        INIT_REQUEST_ID,
        json!({"subtype": "initialize"}),
    ));
    let mut phase = Phase::Init {
        deadline: Instant::now() + INIT_TIMEOUT,
    };

    // `None`：stdout 結束且沒有其他依據，結局看結束碼決定。
    let end: Option<ClaudeEnd> = loop {
        let notified = stop.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        let stopping = matches!(phase, Phase::Stopping { .. });
        if !stopping {
            let reason = if cancel.is_cancelled() {
                Some(StopReason::Cancelled)
            } else if finished.load(Ordering::SeqCst) {
                Some(StopReason::Finished)
            } else {
                None
            };
            if let Some(reason) = reason {
                if matches!(phase, Phase::Init { .. }) {
                    // 還沒送出任務，沒有要中斷的一輪。
                    break Some(reason.end());
                }
                tracing::debug!(?reason, "中斷 Claude Code");
                proc.send(control_request(
                    INTERRUPT_REQUEST_ID,
                    json!({"subtype": "interrupt"}),
                ));
                phase = Phase::Stopping {
                    reason,
                    deadline: Instant::now() + grace,
                };
                continue;
            }
        }
        let deadline = match phase {
            Phase::Init { deadline } | Phase::Stopping { deadline, .. } => Some(deadline),
            Phase::Turn => None,
        };

        tokio::select! {
            _ = cancel.cancelled(), if !stopping => continue,
            _ = &mut notified, if !stopping => continue,
            _ = sleep_until(deadline) => match phase {
                Phase::Init { .. } => {
                    break Some(ClaudeEnd::Failed(ClaudeFailure::Protocol(
                        "等 initialize 回應逾時".into(),
                    )));
                }
                Phase::Stopping { reason, .. } => {
                    tracing::debug!(?reason, "Claude Code 沒有在時限內結束這輪");
                    break Some(reason.end());
                }
                Phase::Turn => continue,
            },
            line = proc.lines.recv() => {
                let value = match line {
                    None => {
                        // stdout 結束。
                        break if let Phase::Stopping { reason, .. } = phase {
                            Some(reason.end())
                        } else if finished.load(Ordering::SeqCst) {
                            Some(ClaudeEnd::Finished)
                        } else {
                            state.pending.take().map(ClaudeEnd::Failed)
                        };
                    }
                    Some(Line::Oversized(len)) => {
                        tracing::warn!(len, "Claude Code 輸出的一行超過上限，已略過");
                        continue;
                    }
                    Some(Line::Json(value)) => value,
                };
                match state.handle(&value) {
                    Handled::Nothing => {}
                    Handled::Reply(frame) => proc.send(frame),
                    Handled::ControlResponse { id, error } => {
                        if id != INIT_REQUEST_ID || !matches!(phase, Phase::Init { .. }) {
                            continue;
                        }
                        if let Some(error) = error {
                            break Some(ClaudeEnd::Failed(ClaudeFailure::Protocol(format!(
                                "initialize 失敗：{error}"
                            ))));
                        }
                        proc.send(user_message(&prompt));
                        phase = Phase::Turn;
                    }
                    Handled::Permission { id, tool, input } => {
                        let decision = policy(&tool, &input);
                        let allowed = decision == Permission::Allow;
                        tracing::debug!(%tool, allowed, "原生工具權限");
                        state.emit(ClaudeEvent::PermissionDecided { tool, allowed });
                        proc.send(permission_reply(&id, decision, input));
                    }
                    Handled::Result(failure) => {
                        if let Phase::Stopping { reason, .. } = phase {
                            break Some(reason.end());
                        }
                        if finished.load(Ordering::SeqCst) {
                            break Some(ClaudeEnd::Finished);
                        }
                        if let Some(failure) = failure {
                            break Some(ClaudeEnd::Failed(failure));
                        }
                        if matches!(phase, Phase::Turn)
                            && let Some(text) = nudge.take()
                        {
                            proc.send(user_message(&text));
                            continue;
                        }
                        break Some(ClaudeEnd::TurnEnded);
                    }
                }
            }
        }
    };

    let (status, forced) = proc.shutdown(grace).await;
    if forced && let Some(hard_stop) = hard_stop {
        hard_stop().await;
    }
    let end = end.unwrap_or_else(|| {
        let stderr = proc.stderr_tail();
        match status {
            Some(status) if status.success() => ClaudeEnd::Failed(ClaudeFailure::Protocol(
                "Claude Code 結束了但沒有送出 result".into(),
            )),
            status => ClaudeEnd::Failed(ClaudeFailure::Exit {
                code: status.and_then(|s| s.code()),
                stderr,
            }),
        }
    });
    proc.abort();
    state.outcome(end)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopReason {
    Finished,
    Cancelled,
}

impl StopReason {
    fn end(self) -> ClaudeEnd {
        match self {
            Self::Finished => ClaudeEnd::Finished,
            Self::Cancelled => ClaudeEnd::Cancelled,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Phase {
    /// 等 `initialize` 回應，任務還沒送出。
    Init { deadline: Instant },
    /// 任務已送出，CLI 在工作。
    Turn,
    /// 已送 interrupt，等這輪的 `result`。
    Stopping {
        reason: StopReason,
        deadline: Instant,
    },
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// 組出附加在 `run.command` 後面的 CLI 參數。
fn cli_args(run: &ClaudeRun) -> Vec<String> {
    let mut args: Vec<String> = [
        "--print",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--permission-prompt-tool",
        "stdio",
        "--permission-mode",
        "default",
        "--strict-mcp-config",
        "--mcp-config",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    args.push(run.mcp_config_path.clone());
    // `=` 的形式：空字串（不讀任何設定來源）也能表示。
    args.push(format!("--setting-sources={}", run.setting_sources));
    if let Some(path) = &run.settings_path {
        args.push("--settings".into());
        args.push(path.clone());
    }
    if !run.append_system_prompt.is_empty() {
        args.push("--append-system-prompt".into());
        args.push(run.append_system_prompt.clone());
    }
    if let Some(model) = &run.model {
        args.push("--model".into());
        args.push(model.clone());
    }
    if !run.allowed_tools.is_empty() {
        args.push("--allowedTools".into());
        args.push(run.allowed_tools.join(","));
    }
    if !run.disallowed_tools.is_empty() {
        args.push("--disallowedTools".into());
        args.push(run.disallowed_tools.join(","));
    }
    // `=` 的形式：避免以 `-` 開頭的值被當成旗標。
    args.push(match &run.session {
        SessionMode::New(id) => format!("--session-id={id}"),
        SessionMode::Resume(id) => format!("--resume={id}"),
    });
    args
}

fn control_request(id: &str, request: Value) -> Value {
    json!({"type": "control_request", "request_id": id, "request": request})
}

fn user_message(text: &str) -> Value {
    json!({
        "type": "user",
        "message": {"role": "user", "content": text},
        "parent_tool_use_id": null,
        "session_id": "",
    })
}

fn control_success(id: &str, response: Value) -> Value {
    json!({
        "type": "control_response",
        "response": {"subtype": "success", "request_id": id, "response": response},
    })
}

fn control_error(id: &str, error: &str) -> Value {
    json!({
        "type": "control_response",
        "response": {"subtype": "error", "request_id": id, "error": error},
    })
}

fn permission_reply(id: &str, decision: Permission, input: Value) -> Value {
    control_success(
        id,
        match decision {
            Permission::Allow => json!({"behavior": "allow", "updatedInput": input}),
            Permission::Deny(message) => json!({"behavior": "deny", "message": message}),
        },
    )
}

/// 讀 stdout 的 task 交出的一行。
#[derive(Debug)]
enum Line {
    Json(Value),
    /// 超過 [`MAX_LINE_BYTES`] 而被丟掉的一行（原本的長度）。
    Oversized(usize),
}

/// 把一行解析成 JSON 物件；空行與雜訊回 `None`。
fn parse_line(line: &[u8]) -> Option<Value> {
    let line = line.trim_ascii();
    if line.is_empty() {
        return None;
    }
    match serde_json::from_slice::<Value>(line) {
        Ok(value) if value.is_object() => Some(value),
        _ => {
            let text = String::from_utf8_lossy(line);
            tracing::debug!(line = %truncate(&text, NOISE_CHARS), "略過 Claude Code 的非協定輸出");
            None
        }
    }
}

/// 讀一行（不含換行）到 `buf`，最多 `cap` 位元組。`Ok(None)` 是 EOF；超過上限時把整行讀掉丟棄，
/// 回 `Err(長度)`。
async fn read_line_capped<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    cap: usize,
) -> std::io::Result<Option<Result<(), usize>>> {
    buf.clear();
    let mut dropped = 0usize;
    let mut any = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if !any {
                return Ok(None);
            }
            break;
        }
        any = true;
        let newline = available.iter().position(|&b| b == b'\n');
        let chunk = &available[..newline.unwrap_or(available.len())];
        if dropped > 0 {
            dropped += chunk.len();
        } else if buf.len() + chunk.len() > cap {
            dropped = buf.len() + chunk.len();
            buf.clear();
        } else {
            buf.extend_from_slice(chunk);
        }
        let used = newline.map_or(available.len(), |i| i + 1);
        reader.consume(used);
        if newline.is_some() {
            break;
        }
    }
    Ok(if dropped > 0 {
        Some(Err(dropped))
    } else {
        Some(Ok(()))
    })
}

async fn read_stdout(stdout: ChildStdout, lines: mpsc::Sender<Line>) {
    let mut reader = BufReader::new(stdout);
    let mut buf = Vec::new();
    loop {
        let line = match read_line_capped(&mut reader, &mut buf, MAX_LINE_BYTES).await {
            Ok(None) => return,
            Ok(Some(Ok(()))) => match parse_line(&buf) {
                Some(value) => Line::Json(value),
                None => continue,
            },
            Ok(Some(Err(len))) => Line::Oversized(len),
            Err(e) => {
                tracing::debug!(error = %e, "讀 Claude Code stdout 失敗");
                return;
            }
        };
        if lines.send(line).await.is_err() {
            return;
        }
    }
}

async fn read_stderr(mut stderr: ChildStderr, tail: Arc<Mutex<Vec<u8>>>) {
    let mut chunk = [0u8; 4096];
    loop {
        match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                let mut tail = tail.lock().unwrap_or_else(|e| e.into_inner());
                tail.extend_from_slice(&chunk[..n]);
                if tail.len() > STDERR_TAIL_BYTES {
                    let excess = tail.len() - STDERR_TAIL_BYTES;
                    tail.drain(..excess);
                }
            }
        }
    }
}

async fn write_stdin(mut stdin: ChildStdin, mut frames: mpsc::UnboundedReceiver<Value>) {
    while let Some(frame) = frames.recv().await {
        let mut line = frame.to_string();
        line.push('\n');
        let written = async {
            stdin.write_all(line.as_bytes()).await?;
            stdin.flush().await
        }
        .await;
        if let Err(e) = written {
            tracing::debug!(error = %e, "寫 Claude Code stdin 失敗");
            return;
        }
    }
    // 所有 sender 都 drop 了：stdin 隨 `stdin` drop 關閉。
}

/// 執行中的 CLI 行程與它的 stdio task。
struct Proc {
    child: Child,
    pid: Option<u32>,
    /// `None` 表示 stdin 已關（writer task 送完排隊的內容後關閉）。
    stdin: Option<mpsc::UnboundedSender<Value>>,
    lines: mpsc::Receiver<Line>,
    lines_closed: bool,
    stderr: Arc<Mutex<Vec<u8>>>,
    tasks: Vec<JoinHandle<()>>,
}

impl Proc {
    fn spawn(command: &mut tokio::process::Command) -> std::io::Result<Self> {
        let mut child = command.spawn()?;
        let pid = child.id();
        let (stdin, stdout, stderr) =
            match (child.stdin.take(), child.stdout.take(), child.stderr.take()) {
                (Some(stdin), Some(stdout), Some(stderr)) => (stdin, stdout, stderr),
                _ => return Err(std::io::Error::other("無法取得 Claude Code 的 stdio")),
            };
        let (frames_tx, frames_rx) = mpsc::unbounded_channel();
        let (lines_tx, lines_rx) = mpsc::channel(64);
        let tail = Arc::new(Mutex::new(Vec::new()));
        let tasks = vec![
            tokio::spawn(write_stdin(stdin, frames_rx)),
            tokio::spawn(read_stdout(stdout, lines_tx)),
            tokio::spawn(read_stderr(stderr, tail.clone())),
        ];
        Ok(Self {
            child,
            pid,
            stdin: Some(frames_tx),
            lines: lines_rx,
            lines_closed: false,
            stderr: tail,
            tasks,
        })
    }

    fn send(&self, frame: Value) {
        if let Some(stdin) = &self.stdin {
            let _ = stdin.send(frame);
        }
    }

    fn stderr_tail(&self) -> String {
        let tail = self.stderr.lock().unwrap_or_else(|e| e.into_inner());
        String::from_utf8_lossy(&tail).trim().to_string()
    }

    /// 等行程結束，同時把 stdout 讀掉（免得管線塞滿卡住 CLI）。逾時回 `None`。
    async fn wait_draining(&mut self, limit: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + limit;
        loop {
            tokio::select! {
                status = self.child.wait() => return status.ok(),
                line = self.lines.recv(), if !self.lines_closed => {
                    if line.is_none() {
                        self.lines_closed = true;
                    }
                }
                _ = tokio::time::sleep_until(deadline) => return None,
            }
        }
    }

    /// 關 stdin、等行程結束，必要時整組砍掉。回傳結束狀態與是否被強制終止。
    async fn shutdown(&mut self, grace: Duration) -> (Option<ExitStatus>, bool) {
        self.stdin = None;
        if let Some(status) = self.wait_draining(grace).await {
            // 清掉 CLI 留下的背景行程（例如背景執行的 Bash）。
            self.signal_group(Signal::Kill);
            return (Some(status), false);
        }
        tracing::warn!("Claude Code 沒有在時限內結束，強制終止");
        self.signal_group(Signal::Term);
        if let Some(status) = self.wait_draining(TERM_WAIT).await {
            self.signal_group(Signal::Kill);
            return (Some(status), true);
        }
        self.signal_group(Signal::Kill);
        let _ = self.child.start_kill();
        let status = tokio::time::timeout(KILL_WAIT, self.child.wait())
            .await
            .ok()
            .and_then(Result::ok);
        (status, true)
    }

    #[cfg(unix)]
    fn signal_group(&self, signal: Signal) {
        let Some(pid) = self.pid.and_then(|pid| i32::try_from(pid).ok()) else {
            return;
        };
        let signal = match signal {
            Signal::Term => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
        };
        // SAFETY：killpg 只送訊號；群組是 spawn 時以 `process_group(0)` 建的（pgid 等於 pid）。
        // 群組已經不存在時回 ESRCH，忽略即可。
        unsafe {
            libc::killpg(pid, signal);
        }
    }

    #[cfg(not(unix))]
    fn signal_group(&self, _signal: Signal) {
        // 沒有行程群組：只能靠 `shutdown` 最後的 `start_kill` 與 `kill_on_drop`。
    }

    fn abort(&self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

#[derive(Clone, Copy)]
enum Signal {
    Term,
    Kill,
}

/// 處理一筆 stdout 紀錄之後要做的事。
#[derive(Debug, PartialEq)]
enum Handled {
    Nothing,
    /// 直接回給 CLI 的 frame。
    Reply(Value),
    /// CLI 回覆我們的 control request。
    ControlResponse {
        id: String,
        error: Option<String>,
    },
    /// `can_use_tool`：要問權限政策。
    Permission {
        id: String,
        tool: String,
        input: Value,
    },
    /// 一輪結束；帶著這輪的錯誤（如果有）。
    Result(Option<ClaudeFailure>),
}

/// 從 stdout 紀錄累積出來的結果。
struct State {
    events: mpsc::UnboundedSender<ClaudeEvent>,
    session_id: Option<String>,
    version: Option<String>,
    started: bool,
    cost_usd: Option<f64>,
    num_turns: u32,
    last_text: String,
    /// 在 `result` 之前就看到的錯誤（assistant 的 `error`、用量上限事件）。
    pending: Option<ClaudeFailure>,
}

impl State {
    fn new(events: mpsc::UnboundedSender<ClaudeEvent>) -> Self {
        Self {
            events,
            session_id: None,
            version: None,
            started: false,
            cost_usd: None,
            num_turns: 0,
            last_text: String::new(),
            pending: None,
        }
    }

    fn emit(&self, event: ClaudeEvent) {
        let _ = self.events.send(event);
    }

    fn outcome(self, end: ClaudeEnd) -> ClaudeOutcome {
        ClaudeOutcome {
            end,
            session_id: self.session_id,
            version: self.version,
            cost_usd: self.cost_usd,
            num_turns: self.num_turns,
            last_text: self.last_text,
        }
    }

    fn handle(&mut self, value: &Value) -> Handled {
        let kind = value["type"].as_str().unwrap_or_default();
        match kind {
            "system" => {
                if value["subtype"] == "init" {
                    self.on_init(value);
                }
                Handled::Nothing
            }
            "assistant" => {
                self.on_assistant(value);
                Handled::Nothing
            }
            "result" => Handled::Result(self.on_result(value)),
            "rate_limit_event" => {
                self.on_rate_limit(&value["rate_limit_info"]);
                Handled::Nothing
            }
            "control_request" => {
                let id = value["request_id"].as_str().unwrap_or_default().to_string();
                let request = &value["request"];
                match request["subtype"].as_str().unwrap_or_default() {
                    "can_use_tool" => Handled::Permission {
                        id,
                        tool: request["tool_name"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        input: match &request["input"] {
                            Value::Null => json!({}),
                            input => input.clone(),
                        },
                    },
                    subtype => {
                        tracing::debug!(subtype, "不支援的 Claude Code control request");
                        Handled::Reply(control_error(
                            &id,
                            &format!("不支援的 control request：{subtype}"),
                        ))
                    }
                }
            }
            "control_response" => {
                let response = &value["response"];
                let id = response["request_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let error = (response["subtype"] == "error").then(|| {
                    response["error"]
                        .as_str()
                        .unwrap_or("未知的錯誤")
                        .to_string()
                });
                Handled::ControlResponse { id, error }
            }
            "control_cancel_request" => {
                // 權限是同步回覆的，沒有在等的請求可以取消。
                tracing::debug!(request_id = ?value["request_id"], "Claude Code 取消 control request");
                Handled::Nothing
            }
            // `user`（工具結果、重播）、`stream_event`（增量）與未來的新類型都略過。
            _ => {
                tracing::trace!(kind, "略過 Claude Code 紀錄");
                Handled::Nothing
            }
        }
    }

    fn on_init(&mut self, value: &Value) {
        let session_id = value["session_id"].as_str().map(str::to_string);
        let version = value["claude_code_version"].as_str().map(str::to_string);
        if version.is_some() {
            self.version = version.clone();
        }
        let Some(session_id) = session_id else {
            return;
        };
        let changed = self.session_id.as_deref() != Some(&session_id);
        self.session_id = Some(session_id.clone());
        if !self.started || changed {
            self.started = true;
            self.emit(ClaudeEvent::Started {
                session_id,
                version,
            });
        }
    }

    fn on_assistant(&mut self, value: &Value) {
        let blocks = value["message"]["content"].as_array();
        let subagent = !value["parent_tool_use_id"].is_null();
        if let Some(code) = value["error"].as_str() {
            // CLI 自己產生的錯誤訊息：不當成 assistant 的文字。
            let text: Vec<&str> = blocks
                .into_iter()
                .flatten()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect();
            let failure = classify(Some(code), None, &text.join("\n"));
            // `rate_limit_event` 的訊息有重置時間，比之後的通用訊息有用。
            if !matches!(
                (&self.pending, &failure),
                (
                    Some(ClaudeFailure::UsageLimit(_)),
                    ClaudeFailure::UsageLimit(_)
                )
            ) {
                self.pending = Some(failure);
            }
            return;
        }
        for block in blocks.into_iter().flatten() {
            match block["type"].as_str() {
                Some("text") if !subagent => {
                    let Some(text) = block["text"].as_str() else {
                        continue;
                    };
                    if text.trim().is_empty() {
                        continue;
                    }
                    self.last_text = text.to_string();
                    self.emit(ClaudeEvent::Text(text.to_string()));
                }
                Some("tool_use") => {
                    let name = block["name"].as_str().unwrap_or_default();
                    self.emit(ClaudeEvent::ToolUse {
                        name: name.to_string(),
                        summary: tool_summary(name, &block["input"]),
                    });
                }
                _ => {}
            }
        }
    }

    fn on_result(&mut self, value: &Value) -> Option<ClaudeFailure> {
        if let Some(id) = value["session_id"].as_str().filter(|id| !id.is_empty()) {
            self.session_id = Some(id.to_string());
        }
        if let Some(cost) = value["total_cost_usd"].as_f64() {
            self.cost_usd = Some(cost);
        }
        if let Some(turns) = value["num_turns"].as_u64() {
            self.num_turns = self
                .num_turns
                .saturating_add(u32::try_from(turns).unwrap_or(u32::MAX));
        }
        let pending = self.pending.take();
        let failure = result_failure(value);
        pending.or(failure)
    }

    fn on_rate_limit(&mut self, info: &Value) {
        match info["status"].as_str() {
            Some("rejected") => {
                let mut text = String::from("Claude 用量已達上限");
                if let Some(kind) = info["rateLimitType"].as_str() {
                    text.push_str(&format!("（{kind}）"));
                }
                if let Some(at) = info["resetsAt"]
                    .as_i64()
                    .and_then(|at| chrono::DateTime::from_timestamp(at, 0))
                {
                    text.push_str(&format!("，{} 重置", at.format("%Y-%m-%d %H:%M UTC")));
                }
                self.pending = Some(ClaudeFailure::UsageLimit(text));
            }
            status => tracing::debug!(?status, "Claude Code 用量狀態"),
        }
    }
}

/// 從 `result` 判斷這輪是否失敗。
fn result_failure(value: &Value) -> Option<ClaudeFailure> {
    let subtype = value["subtype"].as_str().unwrap_or("success");
    let text = value["result"].as_str().unwrap_or_default();
    let is_error = value["is_error"].as_bool().unwrap_or(false) || subtype != "success";
    if !is_error {
        // 舊版 CLI 用量到頂時是「成功」的 result 加上這段文字。
        return text
            .starts_with("Claude AI usage limit reached")
            .then(|| ClaudeFailure::UsageLimit(truncate(text, MAX_ERROR_CHARS)));
    }
    let mut message = text.trim().to_string();
    if message.is_empty()
        && let Some(errors) = value["errors"].as_array()
    {
        let errors: Vec<String> = errors
            .iter()
            .map(|e| match e.as_str() {
                Some(s) => s.to_string(),
                None => e.to_string(),
            })
            .collect();
        message = errors.join("；");
    }
    if message.is_empty() {
        message = subtype.to_string();
    }
    Some(classify(None, value["api_error_status"].as_u64(), &message))
}

const LOGIN_HINTS: &[&str] = &[
    "invalid api key",
    "please run /login",
    "oauth token has expired",
    "oauth token revoked",
    "not logged in",
    "authentication_error",
    "invalid authentication credentials",
];

const LIMIT_HINTS: &[&str] = &[
    "usage limit reached",
    "rate limit",
    "rate_limit_error",
    "credit balance is too low",
    "limit reached",
];

/// 把 CLI 的錯誤分類：先看結構化的錯誤碼與 HTTP 狀態，最後才看文字。
fn classify(code: Option<&str>, status: Option<u64>, text: &str) -> ClaudeFailure {
    let message = match text.trim() {
        "" => code.unwrap_or("Claude Code 回報錯誤").to_string(),
        text => truncate(text, MAX_ERROR_CHARS),
    };
    match code {
        Some("authentication_failed" | "oauth_org_not_allowed") => {
            return ClaudeFailure::NeedsLogin(message);
        }
        Some("rate_limit" | "billing_error") => return ClaudeFailure::UsageLimit(message),
        _ => {}
    }
    match status {
        Some(401) => return ClaudeFailure::NeedsLogin(message),
        Some(429) => return ClaudeFailure::UsageLimit(message),
        _ => {}
    }
    let lower = text.to_lowercase();
    if LOGIN_HINTS.iter().any(|hint| lower.contains(hint)) {
        ClaudeFailure::NeedsLogin(message)
    } else if LIMIT_HINTS.iter().any(|hint| lower.contains(hint)) {
        ClaudeFailure::UsageLimit(message)
    } else {
        ClaudeFailure::Cli(message)
    }
}

/// 給人看的一行工具摘要。
fn tool_summary(name: &str, input: &Value) -> String {
    let field = |key: &str| input[key].as_str().map(one_line);
    let summary = match name {
        "Bash" => field("command"),
        "Read" | "Edit" | "Write" | "MultiEdit" => field("file_path"),
        "NotebookEdit" => field("notebook_path"),
        "Glob" | "Grep" => field("pattern").map(|pattern| match field("path") {
            Some(path) => format!("{pattern}（{path}）"),
            None => pattern,
        }),
        "WebFetch" => field("url"),
        "WebSearch" => field("query"),
        "Task" | "Agent" => field("description"),
        "TodoWrite" => Some("更新待辦清單".to_string()),
        _ => None,
    };
    if let Some(summary) = summary {
        return truncate(&summary, SUMMARY_CHARS);
    }
    if let Some(tool) = name.strip_prefix("mcp__pots__") {
        return format!("平台工具 {tool}");
    }
    if let Some((server, tool)) = name
        .strip_prefix("mcp__")
        .and_then(|rest| rest.split_once("__"))
    {
        return format!("MCP 工具 {server}/{tool}");
    }
    name.to_string()
}

/// 壓成一行：取第一個非空行，多行時加上「…」。
fn one_line(text: &str) -> String {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines.next().unwrap_or_default().to_string();
    if lines.next().is_some() {
        format!("{first} …")
    } else {
        first
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}

#[cfg(test)]
#[path = "claude/tests.rs"]
mod tests;
