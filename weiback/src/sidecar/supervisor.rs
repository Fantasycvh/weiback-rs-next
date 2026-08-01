//! Sidecar 进程监督控制。
//!
//! 负责启动 `weiback-collector` 子进程，维持 stdin/stdout/stderr 三路管道，
//! 完成握手（等待 `ready` + `capabilities`），并支持取消、优雅关闭、
//! 无响应超时和强制终止。
//!
//! 实现要点：
//! - 使用 `std::process::Command` 而非 Tauri shell plugin 直接调用，
//!   保持 supervisor 可脱离 Tauri 单独测试。
//! - stdout 与 stderr 由独立线程并行消费，避免管道缓冲阻塞子进程。
//! - 单条非法 JSON 只记录错误并继续，不污染库。
//! - 进程意外退出时记录退出码，供上层将运行任务标记为 `interrupted`。
use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use thiserror::Error;
use tracing::{debug, warn};

use super::protocol::{
    CommandEnvelope, CommandType, EventEnvelope, EventType, MAX_MESSAGE_BYTES, ProtocolError,
    new_uuid_v7, parse_event_line,
};

/// Sidecar 命令解析：优先使用环境变量 `WEIBACK_COLLECTOR_CMD`，
/// 否则使用当前可执行文件同目录下的 `weiback-collector.exe`（Windows）
/// 或 `weiback-collector`（其它平台）。
pub fn resolve_sidecar_command() -> Option<PathBuf> {
    if let Ok(cmd) = std::env::var("WEIBACK_COLLECTOR_CMD") {
        let p = PathBuf::from(cmd);
        if p.is_file() {
            return Some(p);
        }
        warn!("WEIBACK_COLLECTOR_CMD set but not a file: {p:?}");
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let base = if cfg!(windows) {
        "weiback-collector.exe"
    } else {
        "weiback-collector"
    };
    let p = dir.join(base);
    p.is_file().then_some(p)
}

/// 构造生产采集使用的 Sidecar 启动参数。
///
/// 认证文件路径只通过环境变量传给子进程，不进入命令行和日志。
pub fn collection_spawn_options(session_path: &Path) -> Result<SpawnOptions, SidecarError> {
    let program = resolve_sidecar_command().ok_or(SidecarError::SidecarNotFound)?;
    let args = std::env::var("WEIBACK_COLLECTOR_ARGS")
        .map(|value| value.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    Ok(SpawnOptions {
        program,
        args,
        env: vec![
            ("PYTHONUTF8".into(), "1".into()),
            (
                "WEIBACK_COLLECTOR_SESSION_PATH".into(),
                session_path.to_string_lossy().into_owned(),
            ),
        ],
        cwd: None,
        handshake_timeout: Duration::from_secs(10),
    })
}

/// Sidecar 启动选项。
#[derive(Debug, Clone)]
pub struct SpawnOptions {
    /// Sidecar 可执行文件路径。
    pub program: PathBuf,
    /// 附加命令行参数。
    pub args: Vec<String>,
    /// 附加环境变量。
    pub env: Vec<(String, String)>,
    /// 工作目录。
    pub cwd: Option<PathBuf>,
    /// 握手超时。
    pub handshake_timeout: Duration,
}

impl Default for SpawnOptions {
    fn default() -> Self {
        Self {
            program: PathBuf::new(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
            handshake_timeout: Duration::from_secs(10),
        }
    }
}

/// Sidecar 相关的错误。
#[derive(Debug, Error)]
pub enum SidecarError {
    /// I/O 错误（spawn、读写管道）。
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// 无法解析 Sidecar 可执行文件路径。
    #[error("sidecar executable not found")]
    SidecarNotFound,
    /// 协议解析错误。
    #[error("protocol error: {0}")]
    Protocol(#[from] super::protocol::ProtocolError),
    /// 握手超时，进程可能仍在运行。
    #[error("sidecar handshake timed out after {0:?}")]
    HandshakeTimeout(Duration),
    /// 进程在握手或事件读取期间提前退出。
    #[error("sidecar exited with code {code:?}: {detail}")]
    Exited { code: Option<i32>, detail: String },
    /// 握手失败（对方返回 error 事件或协议不兼容）。
    #[error("sidecar handshake failed: {0}")]
    HandshakeFailed(String),
    /// 接收事件超时。
    #[error("sidecar event receive timed out after {0:?}")]
    RecvTimeout(Duration),
    /// stdout 管道已关闭（子进程退出）。
    #[error("sidecar stdout closed")]
    StdoutClosed,
    /// 待发送的命令行超过大小上限。
    #[error("command line exceeds {MAX_MESSAGE_BYTES} bytes")]
    CommandTooLarge,
}

impl From<serde_json::Error> for SidecarError {
    fn from(e: serde_json::Error) -> Self {
        SidecarError::Protocol(super::protocol::ProtocolError::InvalidJson(e))
    }
}

/// 单次从 stdout 读取得到的结果。
enum StdoutItem {
    /// 一行事件 JSON。
    Line(String),
    /// stdout 已关闭（EOF）。
    Eof,
}

/// Sidecar 子进程句柄。
///
/// spawn 后立即接管 stdin/stdout/stderr 三路管道。stdout 由独立线程逐行
/// 读取并投递到 channel，stderr 由另一线程逐行写入日志。
pub struct Sidecar {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout_rx: Receiver<StdoutItem>,
    _stdout_thread: JoinHandle<()>,
    _stderr_thread: JoinHandle<()>,
}

impl std::fmt::Debug for Sidecar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sidecar").finish_non_exhaustive()
    }
}

impl Sidecar {
    /// 启动 Sidecar 并立即接管三路管道，随后等待握手完成。
    ///
    /// 握手流程：spawn 后先发送 `hello`，再等待 `ready` 与 `capabilities`。
    /// 握手成功时返回 `(ready_payload, capabilities_payload)`。
    pub fn spawn_with_handshake(
        options: &SpawnOptions,
    ) -> Result<(Self, Value, Value), SidecarError> {
        let mut process = Self::spawn_raw(options)?;
        process.send_hello()?;
        let (ready, capabilities) = process.wait_handshake(options.handshake_timeout)?;
        Ok((process, ready, capabilities))
    }

    /// 启动 Sidecar 并接管管道，不等待握手。
    fn spawn_raw(options: &SpawnOptions) -> Result<Self, SidecarError> {
        let mut command = Command::new(&options.program);
        command
            .args(&options.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &options.cwd {
            command.current_dir(cwd);
        }
        for (k, v) in &options.env {
            command.env(k, v);
        }

        debug!("spawning sidecar: {:?}", options.program);
        let mut child = command.spawn()?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let (tx, rx) = mpsc::channel();
        let stdout_thread = std::thread::spawn(move || {
            drain_stdout(stdout, tx);
        });

        let stderr_thread = std::thread::spawn(move || {
            drain_stderr(stderr);
        });

        Ok(Self {
            child,
            stdin,
            stdout_rx: rx,
            _stdout_thread: stdout_thread,
            _stderr_thread: stderr_thread,
        })
    }

    /// 等待握手：连续读取事件，直到 `ready` 与 `capabilities` 都收到，
    /// 或超时 / 进程退出 / 收到 error。
    ///
    /// 返回 `(ready_payload, capabilities_payload)`。
    fn wait_handshake(&mut self, timeout: Duration) -> Result<(Value, Value), SidecarError> {
        let deadline = Instant::now() + timeout;
        let mut ready: Option<Value> = None;
        let mut capabilities: Option<Value> = None;

        while ready.is_none() || capabilities.is_none() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = self.kill();
                return Err(SidecarError::HandshakeTimeout(timeout));
            }
            match self.next_event(remaining) {
                Ok(EventEnvelope {
                    event_type: EventType::Ready,
                    payload,
                    ..
                }) => {
                    debug!("sidecar ready received");
                    ready = Some(payload);
                }
                Ok(EventEnvelope {
                    event_type: EventType::Capabilities,
                    payload,
                    ..
                }) => {
                    debug!("sidecar capabilities received");
                    capabilities = Some(payload);
                }
                Ok(EventEnvelope {
                    event_type: EventType::Error,
                    payload,
                    ..
                }) => {
                    let _ = self.kill();
                    return Err(SidecarError::HandshakeFailed(format!(
                        "sidecar reported error: {payload}"
                    )));
                }
                Ok(_) => {
                    // 忽略握手前的其它事件（progress/warning 等）
                }
                Err(SidecarError::RecvTimeout(_)) => {
                    let _ = self.kill();
                    return Err(SidecarError::HandshakeTimeout(timeout));
                }
                Err(e) => return Err(e),
            }
        }

        Ok((ready.unwrap(), capabilities.unwrap()))
    }

    /// 等待下一个事件，带超时。
    ///
    /// 语法级坏行（非 JSON、非对象）会被记录并跳过；协议级错误
    /// （版本不兼容、非法 UUID、未知事件类型等）直接向上传播，
    /// 由调用方决定处理。
    pub fn next_event(&mut self, mut timeout: Duration) -> Result<EventEnvelope, SidecarError> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.stdout_rx.recv_timeout(timeout) {
                Ok(StdoutItem::Line(line)) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    match parse_event_line(&line) {
                        Ok(event) => return Ok(event),
                        Err(ProtocolError::InvalidJson(_) | ProtocolError::NotAnObject) => {
                            warn!("discarding invalid sidecar event line: {line}");
                            let remaining = deadline.saturating_duration_since(Instant::now());
                            if remaining.is_zero() {
                                return Err(SidecarError::RecvTimeout(timeout));
                            }
                            timeout = remaining;
                        }
                        Err(e) => return Err(SidecarError::Protocol(e)),
                    }
                }
                Ok(StdoutItem::Eof) => {
                    let code = self.wait_code()?;
                    return Err(SidecarError::Exited {
                        code,
                        detail: "stdout closed".to_string(),
                    });
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(SidecarError::RecvTimeout(timeout));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let code = self.wait_code()?;
                    return Err(SidecarError::Exited {
                        code,
                        detail: "stdout channel disconnected".to_string(),
                    });
                }
            }
        }
    }

    /// 向 Sidecar 发送一条命令。
    pub fn send_command(&mut self, command: &CommandEnvelope) -> Result<(), SidecarError> {
        let line = command.to_line()?;
        if line.len() > MAX_MESSAGE_BYTES {
            return Err(SidecarError::CommandTooLarge);
        }
        let stdin = self.stdin.as_mut().ok_or(SidecarError::StdoutClosed)?;
        stdin.write_all(line.as_bytes())?;
        stdin.flush()?;
        Ok(())
    }

    /// 发送 hello 命令。
    pub fn send_hello(&mut self) -> Result<(), SidecarError> {
        let command = CommandEnvelope::new(
            new_uuid_v7(),
            CommandType::Hello,
            json!({"client_name": "weiback-next"}),
        );
        self.send_command(&command)
    }

    /// 优雅关闭：发送 shutdown 并等待进程退出，超过宽限期则强制终止。
    pub fn shutdown(&mut self, grace: Duration) -> Result<(), SidecarError> {
        let command = CommandEnvelope::new(
            new_uuid_v7(),
            CommandType::Shutdown,
            json!({"grace_ms": grace.as_millis() as u64}),
        );
        let _ = self.send_command(&command);

        let deadline = Instant::now() + grace + Duration::from_secs(2);
        loop {
            if let Some(status) = self.child.try_wait()? {
                debug!("sidecar exited with status {status:?}");
                return Ok(());
            }
            if Instant::now() >= deadline {
                warn!("sidecar did not exit gracefully, killing");
                self.child.kill()?;
                let _ = self.child.wait();
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// 强制终止子进程。
    pub fn kill(&mut self) -> Result<(), SidecarError> {
        let _ = self.child.kill();
        let _ = self.child.wait();
        Ok(())
    }

    /// 检查子进程是否已退出，返回退出码。
    pub fn try_exit_code(&mut self) -> Result<Option<i32>, SidecarError> {
        Ok(self.child.try_wait()?.and_then(|s| s.code()))
    }

    /// 阻塞等待子进程退出并返回退出码。
    fn wait_code(&mut self) -> Result<Option<i32>, SidecarError> {
        let status = self.child.wait()?;
        Ok(status.code())
    }
}

/// 逐行读取 stdout 并投递到 channel。
fn drain_stdout(stdout: Option<ChildStdout>, tx: mpsc::Sender<StdoutItem>) {
    let Some(stdout) = stdout else {
        let _ = tx.send(StdoutItem::Eof);
        return;
    };
    let reader = BufReader::new(stdout);
    for line in reader.lines() {
        match line {
            Ok(line) => {
                if tx.send(StdoutItem::Line(line)).is_err() {
                    break;
                }
            }
            Err(e) => {
                warn!("sidecar stdout read error: {e}");
                break;
            }
        }
    }
    let _ = tx.send(StdoutItem::Eof);
}

/// 逐行读取 stderr 并写入日志。
fn drain_stderr(stderr: Option<std::process::ChildStderr>) {
    let Some(stderr) = stderr else {
        return;
    };
    let reader = BufReader::new(stderr);
    for line in reader.lines() {
        match line {
            Ok(line) => {
                if !line.trim().is_empty() {
                    warn!("[sidecar stderr] {line}");
                }
            }
            Err(e) => {
                warn!("sidecar stderr read error: {e}");
                break;
            }
        }
    }
}
