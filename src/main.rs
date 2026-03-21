use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read as IoRead, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::cell::Cell;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const APP_NAME: &str = "acpx-rs";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const SOCKET_WAIT_POLL_MS: u64 = 50;
/// How long a daemon idles (no incoming connections) before self-terminating.
const DAEMON_IDLE_TIMEOUT_SECS: u64 = 30 * 60; // 30 minutes
/// Default max age (days) for closed session records during cleanup.
const CLEANUP_MAX_SESSION_AGE_DAYS: u64 = 14;
/// Default max age (days) for log files of closed/missing sessions during cleanup.
const CLEANUP_MAX_LOG_AGE_DAYS: u64 = 7;
/// Truncate individual log files larger than this (bytes).
const CLEANUP_MAX_LOG_SIZE: u64 = 500 * 1024 * 1024; // 500 MB
/// Keep the tail of truncated logs (bytes).
const CLEANUP_LOG_TAIL_KEEP: u64 = 10 * 1024 * 1024; // 10 MB

#[derive(Parser, Debug)]
#[command(name = "acpx", version = VERSION, about = "Persistent ACP session broker")]
struct Cli {
    #[arg(long, default_value_os_t = default_home_dir())]
    home: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Prompt(PromptArgs),
    Status(SessionRefArgs),
    #[command(subcommand)]
    Sessions(SessionsCommand),
    #[command(hide = true, name = "__serve-session")]
    ServeSession(ServeSessionArgs),
    #[command(hide = true, name = "__guard-session")]
    GuardSession(ServeSessionArgs),
}

#[derive(Args, Debug)]
struct PromptArgs {
    #[arg(short = 's', long = "session")]
    session: String,

    #[arg(long)]
    json: bool,

    /// Read prompt from a file (use "-" for stdin)
    #[arg(long)]
    file: Option<String>,

    #[arg(trailing_var_arg = true)]
    text: Vec<String>,
}

#[derive(Args, Debug)]
struct SessionRefArgs {
    #[arg(short = 's', long = "session")]
    session: String,
}

#[derive(Subcommand, Debug)]
enum SessionsCommand {
    Ensure(EnsureArgs),
    Last(SessionNameArgs),
    Close(SessionNameArgs),
    /// List all sessions (active and closed)
    List(ListArgs),
    /// Clean up dead daemons, stale sockets, old session records, and oversized logs
    Cleanup(CleanupArgs),
}

#[derive(Args, Debug)]
struct ListArgs {
    /// Show only active (non-closed) sessions
    #[arg(long)]
    active: bool,
}

#[derive(Args, Debug)]
struct CleanupArgs {
    /// Max age in days for closed session records (default: 14)
    #[arg(long)]
    max_session_age_days: Option<u64>,

    /// Max age in days for log files of closed sessions (default: 7)
    #[arg(long)]
    max_log_age_days: Option<u64>,

    /// Max log file size in MB before truncation (default: 500)
    #[arg(long)]
    max_log_size_mb: Option<u64>,

    /// Actually delete/truncate (default is dry-run)
    #[arg(long)]
    force: bool,
}

#[derive(Args, Debug)]
struct EnsureArgs {
    #[arg(long)]
    name: String,

    #[arg(long)]
    agent: String,

    #[arg(long, default_value_os_t = std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))]
    cwd: PathBuf,

    #[arg(long, default_value_t = 30)]
    startup_timeout: u64,

    #[arg(long)]
    model: Option<String>,

    #[arg(long)]
    mode: Option<String>,

    /// Suppress stdout output (useful when chaining with prompt --json)
    #[arg(long, short = 'q')]
    quiet: bool,
}

#[derive(Args, Debug)]
struct SessionNameArgs {
    name: String,
}

#[derive(Args, Debug)]
struct ServeSessionArgs {
    #[arg(long)]
    record: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HistoryEntry {
    role: String,
    text: String,
    timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionRecord {
    name: String,
    cwd: String,
    agent_command: String,
    socket_path: String,
    pid: Option<u32>,
    agent_pid: Option<u32>,
    acp_session_id: Option<String>,
    closed: bool,
    created_at: String,
    updated_at: String,
    last_assistant: Option<HistoryEntry>,
    history: Vec<HistoryEntry>,
    #[serde(default)]
    config_model: Option<String>,
    #[serde(default)]
    config_mode: Option<String>,
    /// Set by the guardian when the daemon exits without cleanly marking itself closed.
    #[serde(default)]
    death_reason: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OwnerRequest {
    Status,
    Prompt { text: String },
    Close,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OwnerEvent {
    Status { record: SessionRecord },
    Chunk { text: String },
    Thought { text: String },
    ToolCall { title: String, status: String, tool_call_id: String },
    Done { stop_reason: String, text: String },
    Closed,
    Error { message: String },
}

/// ACP-aligned output events matching the AcpRuntimeEvent TypeScript type.
/// Used for --json stdout output so the TS runtime can consume events directly.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AcpEvent {
    TextDelta {
        text: String,
        stream: &'static str,
        tag: &'static str,
    },
    Status {
        text: String,
    },
    Done {
        #[serde(rename = "stopReason")]
        stop_reason: String,
    },
    Error {
        message: String,
    },
}

impl From<&OwnerEvent> for Option<AcpEvent> {
    fn from(event: &OwnerEvent) -> Self {
        match event {
            OwnerEvent::Chunk { text } => Some(AcpEvent::TextDelta {
                text: text.clone(),
                stream: "output",
                tag: "agent_message_chunk",
            }),
            OwnerEvent::Thought { text } => Some(AcpEvent::TextDelta {
                text: text.clone(),
                stream: "thought",
                tag: "agent_thought_chunk",
            }),
            OwnerEvent::Done { stop_reason, .. } => Some(AcpEvent::Done {
                stop_reason: stop_reason.clone(),
            }),
            OwnerEvent::ToolCall { title, status, .. } => {
                // Only emit a brief status on start — skip updates/completions to
                // keep the calling agent's context lean.
                if status == "in_progress" {
                    Some(AcpEvent::Status {
                        text: format!("using {title}"),
                    })
                } else {
                    None
                }
            }
            OwnerEvent::Error { message } => Some(AcpEvent::Error {
                message: message.clone(),
            }),
            _ => None,
        }
    }
}

struct SessionPaths {
    root: PathBuf,
    sessions_dir: PathBuf,
    sockets_dir: PathBuf,
    logs_dir: PathBuf,
}

impl SessionPaths {
    fn new(home: &Path) -> Self {
        let root = home.to_path_buf();
        Self {
            sessions_dir: root.join("sessions"),
            sockets_dir: root.join("sockets"),
            logs_dir: root.join("logs"),
            root,
        }
    }

    fn ensure_dirs(&self) -> Result<()> {
        fs::create_dir_all(&self.sessions_dir)?;
        fs::create_dir_all(&self.sockets_dir)?;
        fs::create_dir_all(&self.logs_dir)?;
        Ok(())
    }

    fn record_path(&self, name: &str) -> PathBuf {
        self.sessions_dir.join(format!("{}.json", safe_name(name)))
    }

    fn socket_path(&self, name: &str) -> PathBuf {
        self.sockets_dir.join(format!("{}.sock", safe_name(name)))
    }

    fn log_path(&self, name: &str) -> PathBuf {
        self.logs_dir.join(format!("{}.log", safe_name(name)))
    }
}

struct AcpClient {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    messages: Receiver<Value>,
    next_id: u64,
    log_path: PathBuf,
    stderr_tail: Arc<Mutex<Vec<String>>>,
    allowed_root: PathBuf,
}

/// Max stderr lines to keep in memory for crash diagnostics.
const STDERR_TAIL_LINES: usize = 50;

impl AcpClient {
    fn start(agent_command: &str, cwd: &Path, log_path: PathBuf) -> Result<Self> {
        let (command, args) = split_command_line(agent_command)?;
        let mut cmd = Command::new(&command);
        cmd.args(args);
        // Only set cwd if accessible (cross-user agents handle their own cwd)
        if cwd.is_dir() {
            cmd.current_dir(cwd);
        }
        cmd.env_remove("CLAUDECODE");
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to spawn agent: {agent_command}"))?;

        let child_stdout = child.stdout.take().context("missing agent stdout")?;
        let child_stderr = child.stderr.take().context("missing agent stderr")?;
        let child_stdin = child.stdin.take().context("missing agent stdin")?;
        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let reader = BufReader::new(child_stdout);
            for line in reader.lines() {
                let Ok(line) = line else {
                    break;
                };
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
                    if tx.send(value).is_err() {
                        break;
                    }
                }
            }
        });

        // Capture stderr into a ring buffer and log to session log file
        let stderr_tail: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let stderr_tail_writer = Arc::clone(&stderr_tail);
        let stderr_log_path = log_path.clone();
        thread::spawn(move || {
            let reader = BufReader::new(child_stderr);
            let mut log_file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&stderr_log_path)
                .ok();
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Some(ref mut f) = log_file {
                    let _ = writeln!(f, "[agent:stderr] {trimmed}");
                }
                if let Ok(mut tail) = stderr_tail_writer.lock() {
                    tail.push(trimmed.to_owned());
                    if tail.len() > STDERR_TAIL_LINES {
                        tail.remove(0);
                    }
                }
            }
        });

        let allowed_root = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let mut client = Self {
            child,
            stdin: BufWriter::new(child_stdin),
            messages: rx,
            next_id: 1,
            log_path,
            stderr_tail,
            allowed_root,
        };
        client.initialize()?;
        Ok(client)
    }

    fn agent_pid(&self) -> u32 {
        self.child.id()
    }

    fn is_agent_dead(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    fn last_stderr(&self) -> String {
        self.stderr_tail
            .lock()
            .ok()
            .map(|lines| lines.join("\n"))
            .unwrap_or_default()
    }

}

struct SessionInfo {
    session_id: String,
    available_modes: Vec<String>,
    current_mode: Option<String>,
    current_model: Option<String>,
}

impl AcpClient {
    fn initialize(&mut self) -> Result<()> {
        let result = self.request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {},
                "clientInfo": {
                    "name": APP_NAME,
                    "version": VERSION
                }
            }),
            None::<fn(&Value)>,
        )?;
        let protocol_version = result
            .get("protocolVersion")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        if protocol_version != 1 {
            bail!("unsupported ACP protocol version: {protocol_version}");
        }
        Ok(())
    }

    fn new_session(&mut self, cwd: &Path) -> Result<SessionInfo> {
        let result = self.request(
            "session/new",
            json!({
                "cwd": cwd,
                "mcpServers": []
            }),
            None::<fn(&Value)>,
        )?;
        let session_id = result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .context("session/new did not return sessionId")?;

        // Extract available modes
        let available_modes: Vec<String> = result
            .pointer("/modes/availableModes")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("id").and_then(Value::as_str).map(ToOwned::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let current_mode = result
            .pointer("/modes/currentModeId")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);

        // Extract current model
        let current_model = result
            .pointer("/models/currentModelId")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);

        // Check if agent supports set_config_option (from initialize capabilities)
        // We detect this at set_config_option call time instead — keep it simple.

        Ok(SessionInfo {
            session_id,
            available_modes,
            current_mode,
            current_model,
        })
    }

    fn set_config_option(&mut self, session_id: &str, config_id: &str, value: &str) -> Result<()> {
        self.request(
            "session/set_config_option",
            json!({
                "sessionId": session_id,
                "configId": config_id,
                "value": value
            }),
            None::<fn(&Value)>,
        )?;
        Ok(())
    }

    fn set_mode(&mut self, session_id: &str, mode: &str) -> Result<()> {
        self.request(
            "session/set_mode",
            json!({
                "sessionId": session_id,
                "modeId": mode
            }),
            None::<fn(&Value)>,
        )?;
        Ok(())
    }

    fn prompt<F>(
        &mut self,
        session_id: &str,
        text: &str,
        mut on_event: F,
    ) -> Result<String>
    where
        F: FnMut(OwnerEvent),
    {
        let mut assembled = String::new();
        let result = self.request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [
                    {
                        "type": "text",
                        "text": text
                    }
                ]
            }),
            Some(|message: &Value| {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                let same_session = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(|value| value == session_id)
                    .unwrap_or(false);
                if !same_session {
                    return;
                }
                let update = params.get("update");
                let update_type = update
                    .and_then(|u| u.get("sessionUpdate"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                match update_type {
                    "tool_call" | "tool_call_update" => {
                        let content = update.and_then(|u| u.get("content"));
                        let title = content
                            .and_then(|c| c.get("title"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let status = content
                            .and_then(|c| c.get("status"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let tool_call_id = content
                            .and_then(|c| c.get("toolCallId"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        if !title.is_empty() {
                            on_event(OwnerEvent::ToolCall { title, status, tool_call_id });
                        }
                        return;
                    }
                    _ => {}
                }
                let text = update
                    .and_then(|u| u.get("content"))
                    .and_then(|c| c.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if text.is_empty() {
                    return;
                }
                match update_type {
                    "agent_thought_chunk" => {
                        on_event(OwnerEvent::Thought {
                            text: text.to_string(),
                        });
                    }
                    "agent_message_chunk" | "" => {
                        assembled.push_str(text);
                        on_event(OwnerEvent::Chunk {
                            text: text.to_string(),
                        });
                    }
                    _ => {} // available_commands_update etc — skip
                }
            }),
        )?;

        let stop_reason = result
            .get("stopReason")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();

        if stop_reason == "cancelled" && assembled.is_empty() {
            assembled.push_str("cancelled");
        }

        Ok(assembled)
    }

    fn request<F>(&mut self, method: &str, params: Value, mut on_notify: Option<F>) -> Result<Value>
    where
        F: FnMut(&Value),
    {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        self.log_json("outbound", &request);
        self.write_json(&request)?;

        loop {
            // Poll with timeout so we can detect dead agent processes.
            // Some agents (e.g. Gemini/node) spawn children that inherit stdout,
            // keeping the pipe open after the main process exits.
            let message = match self.messages.recv_timeout(Duration::from_secs(5)) {
                Ok(msg) => msg,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // Check if child is still alive
                    if let Some(status) = self.child.try_wait()? {
                        let stderr = self.last_stderr();
                        let detail = if stderr.is_empty() {
                            format!("agent process exited ({status}) while waiting for {method} response")
                        } else {
                            format!("agent process exited ({status}) while waiting for {method} response.\nAgent stderr:\n{stderr}")
                        };
                        self.log_error(&detail);
                        bail!("{detail}");
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let stderr = self.last_stderr();
                    let detail = if stderr.is_empty() {
                        "agent closed stdout before responding".to_owned()
                    } else {
                        format!("agent closed stdout before responding.\nAgent stderr:\n{stderr}")
                    };
                    self.log_error(&detail);
                    bail!("{detail}");
                }
            };
            self.log_json("inbound", &message);

            if let Some(method_name) = message.get("method").and_then(Value::as_str) {
                if method_name == "session/update" {
                    if let Some(callback) = on_notify.as_mut() {
                        callback(&message);
                    }
                    continue;
                }

                if let Some(request_id) = message.get("id").and_then(Value::as_u64) {
                    let params = message.get("params").cloned().unwrap_or(Value::Null);
                    let response = self.handle_server_request(method_name, params);
                    self.write_response(request_id, response)?;
                    continue;
                }
            }

            let Some(response_id) = message.get("id").and_then(Value::as_u64) else {
                continue;
            };
            if response_id != id {
                continue;
            }

            if let Some(error) = message.get("error") {
                let error_message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown ACP error");
                bail!("{method} failed: {error_message}");
            }

            return message
                .get("result")
                .cloned()
                .context("ACP response missing result");
        }
    }

    fn write_json(&mut self, value: &Value) -> Result<()> {
        serde_json::to_writer(&mut self.stdin, value)?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }

    fn write_response(&mut self, id: u64, response: Result<Value>) -> Result<()> {
        let message = match response {
            Ok(result) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result
            }),
            Err(error) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32601,
                    "message": format!("{error:#}")
                }
            }),
        };
        self.log_json("outbound", &message);
        self.write_json(&message)
    }

    /// Validate that a path is under the session's allowed root (cwd).
    /// Resolves symlinks and `..` to prevent path traversal.
    fn validate_path(&self, raw: &str) -> Result<PathBuf> {
        let requested = Path::new(raw);
        // Resolve to absolute: if relative, join with allowed_root
        let absolute = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            self.allowed_root.join(requested)
        };
        // Canonicalize to resolve symlinks and .. components.
        // For reads the file must exist; for writes the parent must exist.
        let canonical = if let Ok(p) = absolute.canonicalize() {
            p
        } else {
            // File may not exist yet (write). Canonicalize the parent.
            let parent = absolute
                .parent()
                .context("no parent directory")?
                .canonicalize()
                .with_context(|| format!("cannot resolve parent of: {raw}"))?;
            let name = absolute
                .file_name()
                .context("no file name")?;
            parent.join(name)
        };
        if !canonical.starts_with(&self.allowed_root) {
            bail!(
                "path {} is outside the allowed workspace ({})",
                canonical.display(),
                self.allowed_root.display()
            );
        }
        Ok(canonical)
    }

    fn handle_server_request(&mut self, method: &str, params: Value) -> Result<Value> {
        match method {
            "requestPermission" | "session/request_permission" => Ok(handle_permission_request(&params)),
            "readTextFile" => {
                let raw = params
                    .get("path")
                    .and_then(Value::as_str)
                    .context("readTextFile missing path")?;
                let path = self.validate_path(raw)?;
                let content = fs::read_to_string(&path)
                    .with_context(|| format!("failed to read file {}", path.display()))?;
                Ok(json!({ "content": content }))
            }
            "writeTextFile" => {
                let raw = params
                    .get("path")
                    .and_then(Value::as_str)
                    .context("writeTextFile missing path")?;
                let path = self.validate_path(raw)?;
                let content = params
                    .get("content")
                    .and_then(Value::as_str)
                    .context("writeTextFile missing content")?;
                fs::write(&path, content)
                    .with_context(|| format!("failed to write file {}", path.display()))?;
                Ok(json!({}))
            }
            "createTerminal" | "terminalOutput" | "waitForTerminalExit" | "killTerminal" | "releaseTerminal" => {
                bail!("{method} is not implemented")
            }
            _ => bail!("unsupported server request method: {method}"),
        }
    }

    fn log_json(&self, direction: &str, value: &Value) {
        let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
        else {
            return;
        };
        let _ = writeln!(file, "[acp:{direction}] {}", value);
    }

    fn log_error(&self, message: &str) {
        let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
        else {
            return;
        };
        let _ = writeln!(file, "[error] {message}");
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let paths = SessionPaths::new(&cli.home);
    paths.ensure_dirs()?;

    match cli.command {
        Commands::Prompt(args) => run_prompt(&paths, args),
        Commands::Status(args) => run_status(&paths, &args.session),
        Commands::Sessions(command) => match command {
            SessionsCommand::Ensure(args) => run_sessions_ensure(&paths, args),
            SessionsCommand::Last(args) => run_sessions_last(&paths, &args.name),
            SessionsCommand::Close(args) => run_sessions_close(&paths, &args.name),
            SessionsCommand::List(args) => run_sessions_list(&paths, args),
            SessionsCommand::Cleanup(args) => run_sessions_cleanup(&paths, args),
        },
        Commands::ServeSession(args) => run_session_daemon(&cli.home, &args.record),
        Commands::GuardSession(args) => run_session_guardian(&cli.home, &args.record),
    }
}

fn run_sessions_ensure(paths: &SessionPaths, args: EnsureArgs) -> Result<()> {
    let record_path = paths.record_path(&args.name);
    let old_record = load_record(&record_path).ok();

    if let Some(ref old) = old_record {
        if !old.closed {
            // Check if daemon PID is still alive before trying socket
            let pid_alive = old.pid.map(|p| process_alive(p)).unwrap_or(false);
            if pid_alive && socket_alive(Path::new(&old.socket_path)) {
                if !args.quiet {
                    print_record(old)?;
                }
                return Ok(());
            }
            // Daemon is dead — mark the old record closed before we overwrite it,
            // so the death_reason is preserved in case the caller wants to query it.
            if !pid_alive {
                let mut stale = old.clone();
                if stale.death_reason.is_none() {
                    stale.death_reason = Some("daemon found dead during session ensure".to_string());
                }
                stale.closed = true;
                stale.updated_at = iso_now();
                let _ = save_record(&record_path, &stale);
                let _ = fs::remove_file(&old.socket_path);
            }
        }
    }

    let socket_path = paths.socket_path(&args.name);
    let (history, last_assistant, created_at) = match old_record {
        Some(ref old) => (old.history.clone(), old.last_assistant.clone(), old.created_at.clone()),
        None => (Vec::new(), None, iso_now()),
    };
    let record = SessionRecord {
        name: args.name.clone(),
        cwd: args.cwd.canonicalize().unwrap_or(args.cwd).display().to_string(),
        agent_command: args.agent,
        socket_path: socket_path.display().to_string(),
        pid: None,
        agent_pid: None,
        acp_session_id: None,
        closed: false,
        created_at,
        updated_at: iso_now(),
        last_assistant,
        history,
        config_model: args.model,
        config_mode: args.mode,
        death_reason: None,
    };
    save_record(&record_path, &record)?;

    spawn_session_daemon(paths, &record)?;
    let timeout = Duration::from_secs(args.startup_timeout);
    let record = wait_for_ready_record(&record_path, &socket_path, timeout)?;
    if !args.quiet {
        print_record(&record)?;
    }
    Ok(())
}

fn run_prompt(paths: &SessionPaths, args: PromptArgs) -> Result<()> {
    let record_path = paths.record_path(&args.session);
    let record = load_record(&record_path)
        .with_context(|| format!("session '{}' does not exist; run sessions ensure", args.session))?;
    if record.closed {
        let base = format!("session '{}' is closed", args.session);
        let msg = match &record.death_reason {
            Some(reason) => format!("{base}: {reason}"),
            None => base,
        };
        bail!("{msg}");
    }

    let text = match args.file.as_deref() {
        Some("-") => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            buf
        }
        Some(path) => fs::read_to_string(path)
            .with_context(|| format!("failed to read prompt file: {path}"))?,
        None => {
            if args.text.is_empty() {
                bail!("no prompt text provided; use trailing args or --file");
            }
            args.text.join(" ")
        }
    };
    let request = OwnerRequest::Prompt { text };
    let mut stream = UnixStream::connect(&record.socket_path).with_context(|| {
        // Reload the record in case the guardian just wrote a death_reason
        let death = load_record(&record_path)
            .ok()
            .and_then(|r| r.death_reason)
            .unwrap_or_else(|| "daemon not reachable".to_string());
        format!("session '{}' is not running: {}", args.session, death)
    })?;
    write_line_json(&mut stream, &request)?;

    // In JSON mode, set a read timeout so we can emit heartbeat status events
    // when the agent is working silently (e.g. long tool runs).
    let heartbeat_secs: u64 = 15;
    if args.json {
        stream.set_read_timeout(Some(std::time::Duration::from_secs(heartbeat_secs)))?;
    }

    let mut reader = BufReader::new(stream);
    let json_mode = args.json;
    let mut last_event = std::time::Instant::now();
    loop {
        let event = match read_line_json::<OwnerEvent, _>(&mut reader) {
            Err(e) if e.downcast_ref::<std::io::Error>()
                .map_or(false, |io| io.kind() == std::io::ErrorKind::WouldBlock
                    || io.kind() == std::io::ErrorKind::TimedOut) =>
            {
                if json_mode && last_event.elapsed().as_secs() >= heartbeat_secs {
                    let hb = AcpEvent::Status { text: "working…".to_string() };
                    println!("{}", serde_json::to_string(&hb)?);
                    std::io::stdout().flush()?;
                    last_event = std::time::Instant::now();
                }
                continue;
            }
            Err(e) => return Err(e),
            Ok(None) => break,
            Ok(Some(event)) => {
                last_event = std::time::Instant::now();
                event
            }
        };
        if json_mode {
            if let Some(acp_event) = Option::<AcpEvent>::from(&event) {
                println!("{}", serde_json::to_string(&acp_event)?);
                std::io::stdout().flush()?;
            }
            match event {
                OwnerEvent::Done { .. } => return Ok(()),
                OwnerEvent::Error { message } => bail!("{message}"),
                _ => {}
            }
        } else {
            match event {
                OwnerEvent::Chunk { text } => {
                    print!("{text}");
                    std::io::stdout().flush()?;
                }
                OwnerEvent::Thought { text } => {
                    eprint!("\x1b[2m[thinking] {text}\x1b[0m");
                    std::io::stderr().flush()?;
                }
                OwnerEvent::ToolCall { title, status, .. } => {
                    eprint!("\x1b[2m[tool: {title}] {status}\x1b[0m\n");
                    std::io::stderr().flush()?;
                }
                OwnerEvent::Done { text, .. } => {
                    if !text.is_empty() && !text.ends_with('\n') {
                        println!();
                    }
                    return Ok(());
                }
                OwnerEvent::Error { message } => bail!("{message}"),
                other => bail!("unexpected owner event: {}", serde_json::to_string(&other)?),
            }
        }
    }

    bail!("owner closed connection without a final response")
}

fn run_status(paths: &SessionPaths, name: &str) -> Result<()> {
    let record_path = paths.record_path(name);
    let record = load_record(&record_path)?;
    if socket_alive(Path::new(&record.socket_path)) {
        let event = send_owner_request(&record.socket_path, &OwnerRequest::Status)?;
        print_owner_event(&event)?;
        return Ok(());
    }
    print_record(&record)?;
    Ok(())
}

fn run_sessions_last(paths: &SessionPaths, name: &str) -> Result<()> {
    let record_path = paths.record_path(name);
    let record = load_record(&record_path)?;
    if let Some(last) = record.last_assistant {
        println!("{}", last.text);
        return Ok(());
    }
    bail!("session '{name}' has no assistant reply yet")
}

fn run_sessions_close(paths: &SessionPaths, name: &str) -> Result<()> {
    let record_path = paths.record_path(name);
    let mut record = load_record(&record_path)?;
    if socket_alive(Path::new(&record.socket_path)) {
        let _ = send_owner_request(&record.socket_path, &OwnerRequest::Close);
    }
    record.closed = true;
    record.updated_at = iso_now();
    save_record(&record_path, &record)?;
    if Path::new(&record.socket_path).exists() {
        let _ = fs::remove_file(&record.socket_path);
    }
    print_record(&record)?;
    Ok(())
}

fn run_sessions_list(paths: &SessionPaths, args: ListArgs) -> Result<()> {
    let entries = fs::read_dir(&paths.sessions_dir)?;
    let mut sessions: Vec<Value> = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(mut record) = load_record(&path) else { continue };
        if args.active && record.closed {
            continue;
        }
        let pid_alive = record.pid.map(|p| process_alive(p)).unwrap_or(false);
        let sock_alive = socket_alive(Path::new(&record.socket_path));
        let status = if record.closed {
            "closed"
        } else if pid_alive && sock_alive {
            "running"
        } else {
            // Daemon is gone but record wasn't closed (guardian may not have run yet,
            // e.g. SIGKILL to the whole process group). Mark it closed inline.
            if !record.closed && record.pid.is_some() && !pid_alive && !sock_alive {
                let reason = record
                    .death_reason
                    .clone()
                    .unwrap_or_else(|| "daemon found dead during list".to_string());
                record.closed = true;
                record.death_reason = Some(reason);
                record.updated_at = iso_now();
                let _ = save_record(&path, &record);
            }
            "stale"
        };
        sessions.push(json!({
            "name": record.name,
            "status": status,
            "agent_command": record.agent_command,
            "created_at": record.created_at,
            "updated_at": record.updated_at,
            "pid": record.pid,
            "history_len": record.history.len(),
            "death_reason": record.death_reason,
        }));
    }
    println!("{}", serde_json::to_string_pretty(&sessions)?);
    Ok(())
}

fn run_sessions_cleanup(paths: &SessionPaths, args: CleanupArgs) -> Result<()> {
    let max_session_age = args.max_session_age_days.unwrap_or(CLEANUP_MAX_SESSION_AGE_DAYS);
    let max_log_age = args.max_log_age_days.unwrap_or(CLEANUP_MAX_LOG_AGE_DAYS);
    let max_log_size = args.max_log_size_mb.map(|mb| mb * 1024 * 1024).unwrap_or(CLEANUP_MAX_LOG_SIZE);
    let dry_run = !args.force;
    let now = SystemTime::now();

    if dry_run {
        eprintln!("dry-run mode (use --force to apply changes)");
    }

    let mut cleaned = 0u32;

    // 1. Reap dead daemons — mark stale sessions as closed, remove orphan sockets
    if let Ok(entries) = fs::read_dir(&paths.sessions_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(mut record) = load_record(&path) else { continue };
            if record.closed {
                continue;
            }
            let pid_alive = record.pid.map(|p| process_alive(p)).unwrap_or(false);
            if pid_alive {
                continue;
            }
            // Daemon is dead but session not marked closed
            eprintln!("reap: {} (pid={:?}, daemon dead)", record.name, record.pid);
            if !dry_run {
                record.closed = true;
                record.updated_at = iso_now();
                save_record(&path, &record)?;
                let sock = Path::new(&record.socket_path);
                if sock.exists() {
                    let _ = fs::remove_file(sock);
                }
            }
            cleaned += 1;
        }
    }

    // 2. Prune old closed session records
    if let Ok(entries) = fs::read_dir(&paths.sessions_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(record) = load_record(&path) else { continue };
            if !record.closed {
                continue;
            }
            let Ok(metadata) = fs::metadata(&path) else { continue };
            let Ok(modified) = metadata.modified() else { continue };
            let age = now.duration_since(modified).unwrap_or_default();
            let age_days = age.as_secs() / 86400;
            if age_days >= max_session_age {
                eprintln!("prune: session record {} ({}d old)", record.name, age_days);
                if !dry_run {
                    let _ = fs::remove_file(&path);
                }
                cleaned += 1;
            }
        }
    }

    // 3. Truncate oversized log files
    if let Ok(entries) = fs::read_dir(&paths.logs_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("log") {
                continue;
            }
            let Ok(metadata) = fs::metadata(&path) else { continue };
            let size = metadata.len();
            if size > max_log_size {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                eprintln!("truncate: {} ({}MB > {}MB)", name, size / (1024 * 1024), max_log_size / (1024 * 1024));
                if !dry_run {
                    truncate_log_tail(&path, CLEANUP_LOG_TAIL_KEEP)?;
                }
                cleaned += 1;
            }
        }
    }

    // 4. Delete old log files for closed/missing sessions
    if let Ok(entries) = fs::read_dir(&paths.logs_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("log") {
                continue;
            }
            let Ok(metadata) = fs::metadata(&path) else { continue };
            let Ok(modified) = metadata.modified() else { continue };
            let age = now.duration_since(modified).unwrap_or_default();
            let age_days = age.as_secs() / 86400;
            if age_days < max_log_age {
                continue;
            }
            // Check if there's a matching active session
            let stem = path.file_stem().unwrap_or_default().to_string_lossy();
            let record_path = paths.sessions_dir.join(format!("{stem}.json"));
            if let Ok(record) = load_record(&record_path) {
                if !record.closed {
                    continue; // session still active, keep the log
                }
            }
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            eprintln!("delete: log {} ({}d old)", name, age_days);
            if !dry_run {
                let _ = fs::remove_file(&path);
            }
            cleaned += 1;
        }
    }

    // 5. Remove orphan sockets (no matching session record, or session closed)
    if let Ok(entries) = fs::read_dir(&paths.sockets_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("sock") {
                continue;
            }
            let stem = path.file_stem().unwrap_or_default().to_string_lossy();
            let record_path = paths.sessions_dir.join(format!("{stem}.json"));
            let remove = if let Ok(record) = load_record(&record_path) {
                record.closed
            } else {
                true // no record at all
            };
            if remove {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                eprintln!("remove: orphan socket {}", name);
                if !dry_run {
                    let _ = fs::remove_file(&path);
                }
                cleaned += 1;
            }
        }
    }

    eprintln!("{} items {}", cleaned, if dry_run { "would be cleaned" } else { "cleaned" });
    Ok(())
}

/// Keep only the last `keep_bytes` of a log file.
fn truncate_log_tail(path: &Path, keep_bytes: u64) -> Result<()> {
    let content = fs::read(path)?;
    let start = if content.len() as u64 > keep_bytes {
        content.len() - keep_bytes as usize
    } else {
        0
    };
    fs::write(path, &content[start..])?;
    Ok(())
}

fn run_session_daemon(home: &Path, record_path: &Path) -> Result<()> {
    let paths = SessionPaths::new(home);
    paths.ensure_dirs()?;
    let mut record = load_record(record_path)?;
    let socket_path = PathBuf::from(&record.socket_path);
    if socket_path.exists() {
        let _ = fs::remove_file(&socket_path);
    }

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind socket {}", socket_path.display()))?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;

    let cwd = PathBuf::from(&record.cwd);
    let mut client = AcpClient::start(&record.agent_command, &cwd, paths.log_path(&record.name))?;
    let info = client.new_session(&cwd)?;
    let session_id = info.session_id;

    // Apply config after session creation (required by some agents like Codex).
    // Skip set_mode if the requested mode isn't in the agent's available modes list,
    // or if the agent is already in the requested mode.
    if let Some(ref mode) = record.config_mode {
        let already_set = info.current_mode.as_deref() == Some(mode.as_str());
        let mode_available = info.available_modes.is_empty()
            || info.available_modes.iter().any(|m| m == mode);
        if already_set {
            eprintln!("info: mode \"{mode}\" already active, skipping set_mode");
        } else if !mode_available {
            eprintln!(
                "info: mode \"{mode}\" not in agent's available modes {:?}, skipping set_mode (current: {:?})",
                info.available_modes,
                info.current_mode,
            );
        } else if let Err(e) = client.set_mode(&session_id, mode) {
            eprintln!("warning: set_mode failed (agent may not support it): {e:#}");
        }
    }
    // Skip set_config_option for model if it's already the current model.
    if let Some(ref model) = record.config_model {
        let already_set = info.current_model.as_deref() == Some(model.as_str());
        if already_set {
            eprintln!("info: model \"{model}\" already active, skipping set_config_option");
        } else {
            // Split composite model IDs like "gpt-5.4/high" into model + reasoning_effort
            if let Some((base, effort)) = model.split_once('/') {
                if let Err(e) = client.set_config_option(&session_id, "model", base) {
                    eprintln!("warning: set_config_option(model) failed: {e:#}");
                }
                if let Err(e) = client.set_config_option(&session_id, "reasoning_effort", effort) {
                    eprintln!("warning: set_config_option(reasoning_effort) failed: {e:#}");
                }
            } else {
                if let Err(e) = client.set_config_option(&session_id, "model", model) {
                    eprintln!("warning: set_config_option(model) failed: {e:#}");
                }
            }
        }
    }

    record.pid = Some(std::process::id());
    record.agent_pid = Some(client.agent_pid());
    record.acp_session_id = Some(session_id.clone());
    record.updated_at = iso_now();
    save_record(record_path, &record)?;

    // Use non-blocking accept with idle timeout so daemon self-terminates
    // when no requests arrive for DAEMON_IDLE_TIMEOUT_SECS.
    listener.set_nonblocking(true)?;
    let idle_timeout = Duration::from_secs(DAEMON_IDLE_TIMEOUT_SECS);
    let mut last_activity = std::time::Instant::now();

    // Register signal handler: SIGTERM and SIGHUP both trigger a clean shutdown
    // so the guardian (or readers of the record) know we exited intentionally.
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&shutdown_flag))?;
    signal_hook::flag::register(signal_hook::consts::SIGHUP, Arc::clone(&shutdown_flag))?;

    loop {
        let stream = match listener.accept() {
            Ok((stream, _)) => {
                last_activity = std::time::Instant::now();
                stream
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // Clean exit on SIGTERM / SIGHUP
                if shutdown_flag.load(Ordering::Relaxed) {
                    eprintln!("received shutdown signal, daemon exiting cleanly");
                    let mut record = load_record(record_path)?;
                    record.closed = true;
                    record.updated_at = iso_now();
                    save_record(record_path, &record)?;
                    break;
                }
                if last_activity.elapsed() >= idle_timeout {
                    eprintln!("idle timeout ({DAEMON_IDLE_TIMEOUT_SECS}s), daemon exiting");
                    let mut record = load_record(record_path)?;
                    record.closed = true;
                    record.updated_at = iso_now();
                    save_record(record_path, &record)?;
                    break;
                }
                // Also check if agent process died while idle
                if client.is_agent_dead() {
                    eprintln!("agent died while idle, daemon exiting");
                    let mut record = load_record(record_path)?;
                    record.closed = true;
                    record.updated_at = iso_now();
                    save_record(record_path, &record)?;
                    break;
                }
                thread::sleep(Duration::from_millis(500));
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        // Process request on the accepted stream (blocking per-connection)
        let mut stream = stream;
        let mut reader = BufReader::new(stream.try_clone()?);
        let Some(request) = read_line_json::<OwnerRequest, _>(&mut reader)? else {
            continue;
        };
        match request {
            OwnerRequest::Status => {
                let record = load_record(record_path)?;
                write_line_json(&mut stream, &OwnerEvent::Status { record })?;
            }
            OwnerRequest::Prompt { text } => {
                let response = handle_prompt(record_path, &session_id, &mut client, &text, &mut stream);
                if let Err(error) = response {
                    let msg = format!("{error:#}");
                    let _ = write_line_json(
                        &mut stream,
                        &OwnerEvent::Error {
                            message: msg.clone(),
                        },
                    );
                    // Record the error in history so it's visible in session records
                    let _ = append_history(record_path, "error", &msg);
                    // If the agent process is dead, exit the daemon so that
                    // `sessions ensure` will detect the stale socket and
                    // recreate everything from scratch.
                    if client.is_agent_dead() {
                        eprintln!("agent is dead, daemon exiting: {msg}");
                        let mut record = load_record(record_path)?;
                        record.closed = true;
                        record.death_reason = Some(msg);
                        record.updated_at = iso_now();
                        save_record(record_path, &record)?;
                        break;
                    }
                }
            }
            OwnerRequest::Close => {
                let mut record = load_record(record_path)?;
                record.closed = true;
                record.updated_at = iso_now();
                save_record(record_path, &record)?;
                let _ = write_line_json(&mut stream, &OwnerEvent::Closed);
                break;
            }
        }
    }

    let _ = fs::remove_file(socket_path);
    Ok(())
}

fn handle_prompt(
    record_path: &Path,
    session_id: &str,
    client: &mut AcpClient,
    text: &str,
    stream: &mut UnixStream,
) -> Result<()> {
    append_history(record_path, "user", text)?;
    // Track the first write failure so we can bail after prompt() returns.
    // Cell is used because the closure already borrows `stream` mutably;
    // Cell<bool> avoids a second &mut borrow.
    let write_failed = Cell::new(false);
    let response = client.prompt(session_id, text, |event| {
        if !write_failed.get() {
            if write_line_json(stream, &event).is_err() {
                write_failed.set(true);
            }
        }
    })?;
    if write_failed.get() {
        bail!("failed to write event to client (broken pipe)");
    }
    let assistant = if response.is_empty() {
        "".to_string()
    } else {
        response.clone()
    };
    if !assistant.is_empty() {
        append_history(record_path, "assistant", &assistant)?;
    }
    write_line_json(
        stream,
        &OwnerEvent::Done {
            stop_reason: "end_turn".to_string(),
            text: assistant,
        },
    )?;
    Ok(())
}

fn append_history(record_path: &Path, role: &str, text: &str) -> Result<()> {
    let mut record = load_record(record_path)?;
    let entry = HistoryEntry {
        role: role.to_string(),
        text: text.to_string(),
        timestamp: iso_now(),
    };
    if role == "assistant" {
        record.last_assistant = Some(entry.clone());
    }
    record.history.push(entry);
    record.updated_at = iso_now();
    save_record(record_path, &record)?;
    Ok(())
}

/// Guardian process: spawns the real daemon as a child and waits for it.
/// If the daemon exits without marking the session closed (crash, SIGKILL, OOM),
/// the guardian writes the death reason into the record so callers know what happened.
fn run_session_guardian(home: &Path, record_path: &Path) -> Result<()> {
    let exe = std::env::current_exe()?;
    let mut child = Command::new(&exe)
        .args([
            "--home",
            &home.to_string_lossy(),
            "__serve-session",
            "--record",
            &record_path.to_string_lossy(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("guardian: failed to spawn daemon")?;

    let status = child.wait().context("guardian: waitpid failed")?;

    // If the daemon exited without marking the session closed it died unexpectedly.
    if let Ok(mut record) = load_record(record_path) {
        if !record.closed {
            let reason = match status.code() {
                Some(code) => format!("daemon exited unexpectedly (exit code {code})"),
                None => "daemon killed by signal".to_string(),
            };
            eprintln!("guardian: {reason} — marking session closed");
            record.closed = true;
            record.death_reason = Some(reason);
            record.updated_at = iso_now();
            let _ = save_record(record_path, &record);
            // Remove the stale socket so future ensure/prompt don't try to connect
            let sock = Path::new(&record.socket_path);
            if sock.exists() {
                let _ = fs::remove_file(sock);
            }
        }
    }
    Ok(())
}

fn spawn_session_daemon(paths: &SessionPaths, record: &SessionRecord) -> Result<()> {
    let exe = std::env::current_exe()?;
    let record_path = paths.record_path(&record.name);
    let log_path = paths.log_path(&record.name);
    let command = format!(
        "nohup {} --home {} __guard-session --record {} >> {} 2>&1 < /dev/null &",
        shell_escape(exe.as_os_str().to_string_lossy().as_ref()),
        shell_escape(paths.root.as_os_str().to_string_lossy().as_ref()),
        shell_escape(record_path.as_os_str().to_string_lossy().as_ref()),
        shell_escape(log_path.as_os_str().to_string_lossy().as_ref()),
    );

    Command::new("sh")
        .arg("-lc")
        .arg(command)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to spawn session daemon")?;
    Ok(())
}

fn wait_for_ready_record(record_path: &Path, socket_path: &Path, timeout: Duration) -> Result<SessionRecord> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if socket_alive(socket_path) {
            let record = load_record(record_path)?;
            if record.pid.is_some() && record.acp_session_id.is_some() {
                return Ok(record);
            }
        }
        thread::sleep(Duration::from_millis(SOCKET_WAIT_POLL_MS));
    }
    bail!("session daemon did not become ready before timeout")
}

fn socket_alive(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    UnixStream::connect(path).is_ok()
}

fn process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn send_owner_request(socket_path: &str, request: &OwnerRequest) -> Result<OwnerEvent> {
    let mut stream = UnixStream::connect(socket_path)?;
    write_line_json(&mut stream, request)?;
    let mut reader = BufReader::new(stream);
    read_line_json::<OwnerEvent, _>(&mut reader)?.context("owner returned no response")
}

fn print_owner_event(event: &OwnerEvent) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(event)?);
    Ok(())
}

fn print_record(record: &SessionRecord) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(record)?);
    Ok(())
}

fn write_line_json<T: Serialize, W: Write>(writer: &mut W, value: &T) -> Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn read_line_json<T: for<'de> Deserialize<'de>, R: BufRead>(reader: &mut R) -> Result<Option<T>> {
    let mut line = String::new();
    let bytes = reader.read_line(&mut line)?;
    if bytes == 0 {
        return Ok(None);
    }
    let value = serde_json::from_str::<T>(line.trim())?;
    Ok(Some(value))
}

fn load_record(path: &Path) -> Result<SessionRecord> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read session record {}", path.display()))?;
    Ok(serde_json::from_str(&contents)?)
}

fn save_record(path: &Path, record: &SessionRecord) -> Result<()> {
    let mut file = File::create(path)
        .with_context(|| format!("failed to write session record {}", path.display()))?;
    serde_json::to_writer_pretty(&mut file, record)?;
    file.write_all(b"\n")?;
    Ok(())
}

fn split_command_line(input: &str) -> Result<(String, Vec<String>)> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escaping = false;

    for ch in input.chars() {
        if escaping {
            current.push(ch);
            escaping = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaping = true;
            continue;
        }
        if let Some(active_quote) = quote {
            if ch == active_quote {
                quote = None;
            } else {
                current.push(ch);
            }
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }
        if ch.is_whitespace() {
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push(ch);
    }

    if escaping {
        current.push('\\');
    }
    if quote.is_some() {
        bail!("unterminated quote in agent command");
    }
    if !current.is_empty() {
        parts.push(current);
    }
    if parts.is_empty() {
        bail!("agent command is empty");
    }
    let command = parts.remove(0);
    Ok((command, parts))
}

fn safe_name(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn handle_permission_request(params: &Value) -> Value {
    let options = params
        .get("options")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let pick = |kinds: &[&str]| -> Option<String> {
        options.iter().find_map(|option| {
            let kind = option.get("kind").and_then(Value::as_str)?;
            if kinds.contains(&kind) {
                option.get("optionId").and_then(Value::as_str).map(ToOwned::to_owned)
            } else {
                None
            }
        })
    };

    if let Some(option_id) = pick(&["allow_once", "allow_always"]) {
        return json!({
            "outcome": {
                "outcome": "selected",
                "optionId": option_id
            }
        });
    }

    if let Some(option_id) = pick(&["reject_once", "reject_always"]) {
        return json!({
            "outcome": {
                "outcome": "selected",
                "optionId": option_id
            }
        });
    }

    json!({
        "outcome": {
            "outcome": "cancelled"
        }
    })
}

fn shell_escape(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    if !value.chars().any(|ch| ch.is_whitespace() || "'\"\\$`!&;|<>(){}".contains(ch)) {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn iso_now() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{now}")
}

fn default_home_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".acpx-rs")
}
