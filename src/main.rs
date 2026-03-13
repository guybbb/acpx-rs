use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const APP_NAME: &str = "acpx-rs";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const SOCKET_WAIT_POLL_MS: u64 = 50;

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
}

#[derive(Args, Debug)]
struct PromptArgs {
    #[arg(short = 's', long = "session")]
    session: String,

    #[arg(trailing_var_arg = true, required = true)]
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
    Done { stop_reason: String, text: String },
    Closed,
    Error { message: String },
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
}

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
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to spawn agent: {agent_command}"))?;

        let child_stdout = child.stdout.take().context("missing agent stdout")?;
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

        let mut client = Self {
            child,
            stdin: BufWriter::new(child_stdin),
            messages: rx,
            next_id: 1,
            log_path,
        };
        client.initialize()?;
        Ok(client)
    }

    fn agent_pid(&self) -> u32 {
        self.child.id()
    }

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

    fn new_session(&mut self, cwd: &Path) -> Result<String> {
        let result = self.request(
            "session/new",
            json!({
                "cwd": cwd,
                "mcpServers": []
            }),
            None::<fn(&Value)>,
        )?;
        result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .context("session/new did not return sessionId")
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
            let message = self
                .messages
                .recv()
                .context("agent closed stdout before responding")?;
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

    fn handle_server_request(&mut self, method: &str, params: Value) -> Result<Value> {
        match method {
            "requestPermission" | "session/request_permission" => Ok(handle_permission_request(&params)),
            "readTextFile" => {
                let path = params
                    .get("path")
                    .and_then(Value::as_str)
                    .context("readTextFile missing path")?;
                let content = fs::read_to_string(path)
                    .with_context(|| format!("failed to read file {}", path))?;
                Ok(json!({ "content": content }))
            }
            "writeTextFile" => {
                let path = params
                    .get("path")
                    .and_then(Value::as_str)
                    .context("writeTextFile missing path")?;
                let content = params
                    .get("content")
                    .and_then(Value::as_str)
                    .context("writeTextFile missing content")?;
                fs::write(path, content)
                    .with_context(|| format!("failed to write file {}", path))?;
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
        },
        Commands::ServeSession(args) => run_session_daemon(&cli.home, &args.record),
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
                print_record(old)?;
                return Ok(());
            }
            // Clean up stale socket if PID is dead
            if !pid_alive {
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
    };
    save_record(&record_path, &record)?;

    spawn_session_daemon(paths, &record)?;
    let timeout = Duration::from_secs(args.startup_timeout);
    let record = wait_for_ready_record(&record_path, &socket_path, timeout)?;
    print_record(&record)?;
    Ok(())
}

fn run_prompt(paths: &SessionPaths, args: PromptArgs) -> Result<()> {
    let record_path = paths.record_path(&args.session);
    let record = load_record(&record_path)
        .with_context(|| format!("session '{}' does not exist; run sessions ensure", args.session))?;
    if record.closed {
        bail!("session '{}' is closed", args.session);
    }

    let text = args.text.join(" ");
    let request = OwnerRequest::Prompt { text };
    let mut stream = UnixStream::connect(&record.socket_path)
        .with_context(|| format!("failed to connect to {}", record.socket_path))?;
    write_line_json(&mut stream, &request)?;

    let mut reader = BufReader::new(stream);
    loop {
        let Some(event) = read_line_json::<OwnerEvent, _>(&mut reader)? else {
            break;
        };
        match event {
            OwnerEvent::Chunk { text } => {
                print!("{text}");
                std::io::stdout().flush()?;
            }
            OwnerEvent::Thought { text } => {
                eprint!("\x1b[2m[thinking] {text}\x1b[0m");
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
    let session_id = client.new_session(&cwd)?;

    // Apply config after session creation (required by some agents like Codex)
    if let Some(ref mode) = record.config_mode {
        client.set_mode(&session_id, mode)?;
    }
    if let Some(ref model) = record.config_model {
        // Split composite model IDs like "gpt-5.4/high" into model + reasoning_effort
        if let Some((base, effort)) = model.split_once('/') {
            client.set_config_option(&session_id, "model", base)?;
            client.set_config_option(&session_id, "reasoning_effort", effort)?;
        } else {
            client.set_config_option(&session_id, "model", model)?;
        }
    }

    record.pid = Some(std::process::id());
    record.agent_pid = Some(client.agent_pid());
    record.acp_session_id = Some(session_id.clone());
    record.updated_at = iso_now();
    save_record(record_path, &record)?;

    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(stream) => stream,
            Err(error) => return Err(error.into()),
        };
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
                    let _ = write_line_json(
                        &mut stream,
                        &OwnerEvent::Error {
                            message: format!("{error:#}"),
                        },
                    );
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
    let response = client.prompt(session_id, text, |event| {
        let _ = write_line_json(stream, &event);
    })?;
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

fn spawn_session_daemon(paths: &SessionPaths, record: &SessionRecord) -> Result<()> {
    let exe = std::env::current_exe()?;
    let record_path = paths.record_path(&record.name);
    let log_path = paths.log_path(&record.name);
    let command = format!(
        "nohup {} --home {} __serve-session --record {} >> {} 2>&1 < /dev/null &",
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
