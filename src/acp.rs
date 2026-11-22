//! Synchronous Agent Client Protocol (ACP) v1 client over stdio.
//!
//! Spawns a local ACP-compatible CLI agent (`claude-acp`, `codex-acp`, `cursor-agent acp`,
//! `opencode acp`, `gemini --acp`, etc.) and speaks newline-delimited JSON-RPC 2.0 on its
//! `stdin`/`stdout`.
//!
//! Security and isolation:
//! - Children start with `env_clear()` plus [`ACP_INHERITED_ENV`], never inheriting API
//!   keys or database URLs stored in the parent's environment; agents rely on their own
//!   local authentication.
//! - On Unix, `PATH` appends standard Homebrew and user local binary directories when not
//!   already present, so GUI `.app` launches still locate user-installed CLI tools without
//!   shadowing explicit `PATH` overrides.
//! - File and terminal client capabilities are advertised as `false`; any unexpected
//!   agent-to-client filesystem or terminal request receives `-32601`.
//! - Permission requests (`session/request_permission`) honor [`AcpConfig::read_only`]:
//!   when true, only `read`, `search`, and `think` tool kinds are approved while `edit`,
//!   `delete`, `execute`, and other operations are denied; when false, requested
//!   permissions are approved automatically.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::exec::{CANCELLED, MAX_OUTPUT_BYTES};
use crate::node_data::AcpConfig;

const POLL: Duration = Duration::from_millis(25);
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);
const MAX_STDERR_BYTES: usize = 1024 * 1024;
const JSONRPC_METHOD_NOT_FOUND: i64 = -32601;

/// Environment variables ACP child processes inherit. API keys and database URLs stay out.
pub const ACP_INHERITED_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "TERM",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
    "SSH_AUTH_SOCK",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "SSL_CERT_FILE",
    "NODE_EXTRA_CA_CERTS",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnResult {
    pub text: String,
    pub stop_reason: Option<String>,
    pub agent: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeResult {
    pub agent: Option<String>,
    pub models: Vec<String>,
    pub modes: Vec<String>,
}

enum Recv {
    Line(String),
    Eof,
    Timeout,
    Overflow,
    Err(String),
}

trait Transport {
    fn send(&mut self, value: &Value) -> Result<(), String>;
    fn recv(&mut self, timeout: Duration) -> Recv;
    fn stderr_excerpt(&mut self) -> String;
    fn finish(self: Box<Self>);
}

struct ProcessTransport {
    program: String,
    child: Child,
    stdin: Option<ChildStdin>,
    rx: mpsc::Receiver<Recv>,
    stderr_buf: Arc<Mutex<Vec<u8>>>,
    stdout_thread: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
    finished: bool,
}

impl ProcessTransport {
    fn spawn(command: &str, args: &[String], cwd: &Path) -> Result<Self, String> {
        let program = command.trim();
        let mut cmd = Command::new(program);
        cmd.args(args).current_dir(cwd).env_clear();
        #[cfg(unix)]
        cmd.process_group(0);
        for name in ACP_INHERITED_ENV {
            if *name == "PATH" {
                if let Some(path) = augmented_path() {
                    cmd.env("PATH", path);
                }
            } else if let Some(value) = std::env::var_os(name) {
                cmd.env(name, value);
            }
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => format!("`{program}` is not installed or not on PATH"),
            _ => format!("could not start `{program}`: {e}"),
        })?;

        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let (tx, rx) = mpsc::channel();
        let stdout_thread = thread::spawn(move || {
            if let Some(pipe) = stdout {
                read_stdout_lines(pipe, tx);
            } else {
                let _ = tx.send(Recv::Eof);
            }
        });

        let stderr_buf = Arc::new(Mutex::new(Vec::new()));
        let stderr_target = stderr_buf.clone();
        let stderr_thread = thread::spawn(move || {
            if let Some(mut pipe) = stderr {
                read_stderr_capped(&mut pipe, &stderr_target);
            }
        });

        Ok(Self {
            program: program.to_string(),
            child,
            stdin,
            rx,
            stderr_buf,
            stdout_thread: Some(stdout_thread),
            stderr_thread: Some(stderr_thread),
            finished: false,
        })
    }

    fn io_finished(&self) -> bool {
        self.stdout_thread
            .as_ref()
            .is_none_or(JoinHandle::is_finished)
            && self
                .stderr_thread
                .as_ref()
                .is_none_or(JoinHandle::is_finished)
    }

    fn terminate(&mut self) {
        self.stdin.take();
        let start = Instant::now();
        let mut exited = false;
        while start.elapsed() < SHUTDOWN_GRACE {
            if !exited {
                match self.child.try_wait() {
                    Ok(Some(_)) => exited = true,
                    Ok(None) => {}
                    Err(_) => break,
                }
            }
            if exited && self.io_finished() {
                break;
            }
            thread::sleep(POLL);
        }
        if !exited || !self.io_finished() {
            kill_group(self.child.id());
            if !exited {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
        if let Some(handle) = self.stdout_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr_thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ProcessTransport {
    fn drop(&mut self) {
        if !self.finished {
            self.terminate();
            self.finished = true;
        }
    }
}

impl Transport for ProcessTransport {
    fn send(&mut self, value: &Value) -> Result<(), String> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(format!("`{}` closed stdin", self.program));
        };
        let line = serde_json::to_string(value).map_err(|e| e.to_string())?;
        writeln!(stdin, "{line}")
            .and_then(|()| stdin.flush())
            .map_err(|e| format!("failed to write to `{}`: {e}", self.program))
    }

    fn recv(&mut self, timeout: Duration) -> Recv {
        match self.rx.recv_timeout(timeout) {
            Ok(event) => event,
            Err(mpsc::RecvTimeoutError::Timeout) => Recv::Timeout,
            Err(mpsc::RecvTimeoutError::Disconnected) => Recv::Eof,
        }
    }

    fn stderr_excerpt(&mut self) -> String {
        let guard = self
            .stderr_buf
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        excerpt_bytes(&guard)
    }

    fn finish(mut self: Box<Self>) {
        if !self.finished {
            self.terminate();
            self.finished = true;
        }
    }
}

fn augmented_path() -> Option<std::ffi::OsString> {
    let existing = std::env::var_os("PATH");
    #[cfg(unix)]
    {
        let mut entries: Vec<PathBuf> = existing
            .as_deref()
            .map(|value| std::env::split_paths(value).collect())
            .unwrap_or_default();
        let mut seen: HashSet<PathBuf> = entries.iter().cloned().collect();
        let mut extra = vec![
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
        ];
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            extra.push(home.join(".local/bin"));
            extra.push(home.join(".cargo/bin"));
        }
        for dir in extra {
            if !seen.contains(&dir) && dir.is_dir() {
                seen.insert(dir.clone());
                entries.push(dir);
            }
        }
        if entries.is_empty() {
            existing
        } else {
            std::env::join_paths(entries).ok().or(existing)
        }
    }
    #[cfg(not(unix))]
    {
        existing
    }
}

fn read_stdout_lines(pipe: impl Read, tx: mpsc::Sender<Recv>) {
    let mut reader = BufReader::new(pipe);
    let mut total_bytes = 0usize;
    let mut line = Vec::new();
    loop {
        let available = match reader.fill_buf() {
            Ok(buf) => buf,
            Err(e) => {
                let _ = tx.send(Recv::Err(format!("reading ACP stdout failed: {e}")));
                return;
            }
        };
        if available.is_empty() {
            if !line.is_empty() {
                send_line(&line, &tx);
            }
            let _ = tx.send(Recv::Eof);
            return;
        }
        let mut consumed = 0usize;
        for &byte in available {
            consumed += 1;
            total_bytes = total_bytes.saturating_add(1);
            if total_bytes > MAX_OUTPUT_BYTES {
                let _ = tx.send(Recv::Overflow);
                return;
            }
            if byte == b'\n' {
                if !send_line(&line, &tx) {
                    return;
                }
                line.clear();
            } else {
                line.push(byte);
            }
        }
        reader.consume(consumed);
    }
}

fn send_line(raw: &[u8], tx: &mpsc::Sender<Recv>) -> bool {
    let trimmed = raw.strip_suffix(b"\r").unwrap_or(raw);
    let event = match std::str::from_utf8(trimmed) {
        Ok(text) => Recv::Line(text.to_string()),
        Err(_) => Recv::Err("ACP agent stdout contains invalid UTF-8".into()),
    };
    tx.send(event).is_ok()
}

fn read_stderr_capped(reader: &mut impl Read, target: &Mutex<Vec<u8>>) {
    let mut buf = [0u8; 8 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut guard = target
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let room = MAX_STDERR_BYTES.saturating_sub(guard.len());
                if room == 0 {
                    break;
                }
                guard.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
}

fn kill_group(pid: u32) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .arg("-9")
            .arg(format!("-{pid}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(unix))]
    let _ = pid;
}

fn excerpt_bytes(bytes: &[u8]) -> String {
    excerpt_str(&String::from_utf8_lossy(bytes))
}

fn excerpt_str(text: &str) -> String {
    const MAX: usize = 400;
    let text = text.trim();
    if text.chars().count() <= MAX {
        text.to_string()
    } else {
        format!("{}...", text.chars().take(MAX).collect::<String>())
    }
}

/// Start an ACP agent, run one prompt turn, stream text and log events, and return the
/// collected text reply.
pub fn run(
    config: &AcpConfig,
    cwd: &Path,
    prompt_text: &str,
    cancel: &AtomicBool,
    on_text: &mut dyn FnMut(&str) -> bool,
    on_log: &mut dyn FnMut(&str),
) -> Result<TurnResult, String> {
    let transport = Box::new(ProcessTransport::spawn(&config.command, &config.args, cwd)?);
    let timeout = Duration::from_secs(u64::from(config.timeout_secs));
    Session::new(transport, config, timeout, cancel, on_text, on_log).run_turn(
        config,
        cwd,
        prompt_text,
    )
}

/// Start an ACP agent, run `initialize` + `session/new`, discover advertised models and
/// modes, and shut down cleanly without sending a prompt.
pub fn probe(
    config: &AcpConfig,
    cwd: &Path,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<ProbeResult, String> {
    let transport = Box::new(ProcessTransport::spawn(&config.command, &config.args, cwd)?);
    let mut noop_text = |_: &str| true;
    let mut noop_log = |_: &str| {};
    Session::new(
        transport,
        config,
        timeout,
        cancel,
        &mut noop_text,
        &mut noop_log,
    )
    .probe_session(cwd)
}

#[derive(Default)]
struct SessionMeta {
    agent: Option<String>,
    models: Vec<String>,
    modes: Vec<String>,
    model_config_id: Option<String>,
    mode_config_id: Option<String>,
}

struct Session<'a> {
    transport: Box<dyn Transport>,
    program: String,
    timeout_secs: u64,
    deadline: Instant,
    read_only: bool,
    cancel: &'a AtomicBool,
    next_id: u64,
    session_id: Option<String>,
    collected: String,
    on_text: &'a mut dyn FnMut(&str) -> bool,
    on_log: &'a mut dyn FnMut(&str),
}

impl<'a> Session<'a> {
    fn new(
        transport: Box<dyn Transport>,
        config: &AcpConfig,
        timeout: Duration,
        cancel: &'a AtomicBool,
        on_text: &'a mut dyn FnMut(&str) -> bool,
        on_log: &'a mut dyn FnMut(&str),
    ) -> Self {
        Self {
            transport,
            program: config.command.trim().to_string(),
            timeout_secs: timeout.as_secs(),
            deadline: Instant::now() + timeout,
            read_only: config.read_only,
            cancel,
            next_id: 1,
            session_id: None,
            collected: String::new(),
            on_text,
            on_log,
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if let Err(err) = self.transport.send(&payload) {
            return Err(self.with_stderr(err));
        }
        self.wait_for_response(id, method)
    }

    fn send_cancel(&mut self) {
        if let Some(sid) = self.session_id.as_deref() {
            let _ = self.transport.send(&json!({
                "jsonrpc": "2.0",
                "method": "session/cancel",
                "params": { "sessionId": sid },
            }));
        }
    }

    fn with_stderr(&mut self, message: String) -> String {
        let stderr = self.transport.stderr_excerpt();
        if stderr.is_empty() {
            message
        } else {
            format!("{message} (stderr: {stderr})")
        }
    }

    fn wait_for_response(&mut self, expected_id: u64, method: &str) -> Result<Value, String> {
        loop {
            if self.cancel.load(Ordering::Relaxed) {
                self.send_cancel();
                return Err(CANCELLED.into());
            }
            let now = Instant::now();
            if now >= self.deadline {
                return Err(format!(
                    "`{}` timed out after {}s and was killed",
                    self.program, self.timeout_secs
                ));
            }
            let wait = self.deadline.saturating_duration_since(now).min(POLL);
            match self.transport.recv(wait) {
                Recv::Timeout => continue,
                Recv::Eof => {
                    return Err(self.with_stderr(format!(
                        "`{}` exited before answering `{method}`",
                        self.program
                    )));
                }
                Recv::Overflow => {
                    return Err(format!(
                        "`{}` produced more than {} bytes of output",
                        self.program, MAX_OUTPUT_BYTES
                    ));
                }
                Recv::Err(e) => return Err(self.with_stderr(e)),
                Recv::Line(line) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let msg: Value = serde_json::from_str(trimmed).map_err(|e| {
                        self.with_stderr(format!(
                            "`{}` returned malformed ACP JSON ({e}): {}",
                            self.program,
                            excerpt_str(trimmed)
                        ))
                    })?;
                    if let Some(resp_id) = msg.get("id").and_then(Value::as_u64)
                        && msg.get("method").is_none()
                        && resp_id == expected_id
                    {
                        if let Some(error) = msg.get("error").filter(|v| !v.is_null()) {
                            return Err(format_rpc_error(&self.program, method, error));
                        }
                        return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
                    }
                    if let Some(incoming_method) = msg.get("method").and_then(Value::as_str) {
                        let params = msg.get("params").cloned().unwrap_or(Value::Null);
                        let req_id = msg.get("id").filter(|v| !v.is_null()).cloned();
                        if let Some(req_id) = req_id {
                            self.handle_incoming_request(req_id, incoming_method, &params)?;
                        } else {
                            self.handle_notification(incoming_method, &params)?;
                        }
                    }
                }
            }
        }
    }

    fn handle_notification(&mut self, method: &str, params: &Value) -> Result<(), String> {
        if method != "session/update" {
            return Ok(());
        }
        let update = &params["update"];
        match update["sessionUpdate"].as_str() {
            Some("agent_message_chunk") => {
                let text = extract_chunk_text(&update["content"]);
                if !text.is_empty() {
                    if self.collected.len().saturating_add(text.len()) > MAX_OUTPUT_BYTES {
                        return Err(format!(
                            "`{}` produced more than {} bytes of text",
                            self.program, MAX_OUTPUT_BYTES
                        ));
                    }
                    self.collected.push_str(&text);
                    if !(self.on_text)(&text) {
                        self.send_cancel();
                        return Err(CANCELLED.into());
                    }
                }
            }
            Some("tool_call") => {
                let kind = update["kind"].as_str().unwrap_or("other");
                let title = update["title"].as_str().unwrap_or("").trim();
                if title.is_empty() {
                    (self.on_log)(&format!("tool [{kind}]"));
                } else {
                    (self.on_log)(&format!("tool [{kind}]: {title}"));
                }
            }
            Some("tool_call_update") => {
                if let Some(status @ ("completed" | "failed")) = update["status"].as_str() {
                    let kind = update["kind"].as_str();
                    let title = update["title"]
                        .as_str()
                        .map(str::trim)
                        .filter(|t| !t.is_empty())
                        .or_else(|| update["toolCallId"].as_str())
                        .unwrap_or("tool");
                    match kind {
                        Some(kind) => (self.on_log)(&format!("tool [{kind}] {status}: {title}")),
                        None => (self.on_log)(&format!("tool {status}: {title}")),
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_incoming_request(
        &mut self,
        req_id: Value,
        method: &str,
        params: &Value,
    ) -> Result<(), String> {
        if method == "session/request_permission" {
            let kind = params["toolCall"]["kind"].as_str().unwrap_or("other");
            let title = params["toolCall"]["title"].as_str().unwrap_or("").trim();
            let allow = !self.read_only || matches!(kind, "read" | "search" | "think");
            let options = params["options"].as_array();
            let chosen = pick_permission_option(options, allow);
            if allow {
                if title.is_empty() {
                    (self.on_log)(&format!("approved tool [{kind}]"));
                } else {
                    (self.on_log)(&format!("approved tool [{kind}]: {title}"));
                }
            } else if title.is_empty() {
                (self.on_log)(&format!("denied tool [{kind}] in read-only mode"));
            } else {
                (self.on_log)(&format!("denied tool [{kind}] in read-only mode: {title}"));
            }
            let outcome = match chosen {
                Some(option_id) => json!({ "outcome": "selected", "optionId": option_id }),
                None => json!({ "outcome": "cancelled" }),
            };
            self.transport.send(&json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": { "outcome": outcome },
            }))
        } else {
            self.transport.send(&json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "error": {
                    "code": JSONRPC_METHOD_NOT_FOUND,
                    "message": "client capability not supported",
                },
            }))
        }
    }

    fn start_session(&mut self, cwd: &Path) -> Result<(String, SessionMeta), String> {
        let init = self.request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": { "readTextFile": false, "writeTextFile": false },
                    "terminal": false,
                },
                "clientInfo": {
                    "name": "uapz",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
        )?;
        let agent = init["agentInfo"]["name"]
            .as_str()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(|name| {
                match init["agentInfo"]["version"]
                    .as_str()
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                {
                    Some(version) => format!("{name} {version}"),
                    None => name.to_string(),
                }
            });

        let created = self.request(
            "session/new",
            json!({
                "cwd": cwd.to_string_lossy(),
                "mcpServers": [],
            }),
        )?;
        let session_id = created["sessionId"]
            .as_str()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| format!("`{}` returned no sessionId from session/new", self.program))?
            .to_string();
        self.session_id = Some(session_id.clone());

        let mut meta = parse_session_meta(&created);
        meta.agent = agent;
        Ok((session_id, meta))
    }

    fn finish(self) {
        self.transport.finish();
    }

    fn run_turn(
        mut self,
        config: &AcpConfig,
        cwd: &Path,
        prompt_text: &str,
    ) -> Result<TurnResult, String> {
        let outcome = (|| -> Result<TurnResult, String> {
            let (session_id, meta) = self.start_session(cwd)?;
            if let Some(agent) = &meta.agent {
                (self.on_log)(&format!("connected to {agent}"));
            }
            let mode = config.mode.trim();
            if !mode.is_empty() {
                if let Some(config_id) = &meta.mode_config_id {
                    self.request(
                        "session/set_config_option",
                        json!({
                            "sessionId": session_id,
                            "configId": config_id,
                            "value": mode,
                        }),
                    )?;
                } else {
                    self.request(
                        "session/set_mode",
                        json!({
                            "sessionId": session_id,
                            "modeId": mode,
                        }),
                    )?;
                }
            }
            let model = config.model.trim();
            if !model.is_empty() {
                if let Some(config_id) = &meta.model_config_id {
                    self.request(
                        "session/set_config_option",
                        json!({
                            "sessionId": session_id,
                            "configId": config_id,
                            "value": model,
                        }),
                    )?;
                } else {
                    self.request(
                        "session/set_model",
                        json!({
                            "sessionId": session_id,
                            "modelId": model,
                        }),
                    )?;
                }
            }
            let prompted = self.request(
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": [{ "type": "text", "text": prompt_text }],
                }),
            )?;
            let stop_reason = prompted["stopReason"].as_str().map(str::to_string);
            match stop_reason.as_deref() {
                Some("cancelled") => return Err(CANCELLED.into()),
                Some("refusal") => return Err("ACP agent refused the request".into()),
                Some("max_tokens") => {
                    (self.on_log)("ACP agent stopped because the token limit was reached");
                }
                _ => {}
            }
            let text = std::mem::take(&mut self.collected);
            Ok(TurnResult {
                text,
                stop_reason,
                agent: meta.agent,
            })
        })();
        self.finish();
        outcome
    }

    fn probe_session(mut self, cwd: &Path) -> Result<ProbeResult, String> {
        let outcome = self.start_session(cwd).map(|(_, meta)| ProbeResult {
            agent: meta.agent,
            models: meta.models,
            modes: meta.modes,
        });
        self.finish();
        outcome
    }
}

fn format_rpc_error(program: &str, method: &str, error: &Value) -> String {
    let msg = error["message"]
        .as_str()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or("unknown JSON-RPC error");
    let detail = error
        .get("data")
        .and_then(|d| d.as_str())
        .map(str::trim)
        .filter(|d| !d.is_empty());
    match detail {
        Some(detail) => format!("`{program}` rejected `{method}`: {msg}: {detail}"),
        None => format!("`{program}` rejected `{method}`: {msg}"),
    }
}

fn extract_chunk_text(content: &Value) -> String {
    if let Some(items) = content.as_array() {
        return items
            .iter()
            .filter_map(|item| {
                (item["type"].as_str() == Some("text"))
                    .then(|| item["text"].as_str())
                    .flatten()
            })
            .collect::<Vec<_>>()
            .join("");
    }
    if content["type"].as_str() == Some("text")
        && let Some(text) = content["text"].as_str()
    {
        return text.to_string();
    }
    String::new()
}

fn pick_permission_option(options: Option<&Vec<Value>>, allow: bool) -> Option<String> {
    let options = options?;
    let preferred = if allow {
        ["allow_once", "allow_always"]
    } else {
        ["reject_once", "reject_always"]
    };
    for kind in preferred {
        if let Some(id) = options.iter().find_map(|opt| {
            (opt["kind"].as_str() == Some(kind))
                .then(|| opt["optionId"].as_str())
                .flatten()
                .filter(|id| !id.is_empty())
        }) {
            return Some(id.to_string());
        }
    }
    None
}

fn parse_session_meta(created: &Value) -> SessionMeta {
    let mut meta = SessionMeta::default();
    if let Some(options) = created["configOptions"].as_array() {
        for option in options {
            let id = option["configId"]
                .as_str()
                .or_else(|| option["id"].as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let category = option["category"].as_str().map(str::trim);
            let is_model = category == Some("model") || id == Some("model");
            let is_mode = category == Some("mode") || id == Some("mode");
            if is_model {
                if meta.model_config_id.is_none() {
                    meta.model_config_id = id.map(str::to_string);
                }
                collect_option_values(&option["options"], &mut meta.models);
            } else if is_mode {
                if meta.mode_config_id.is_none() {
                    meta.mode_config_id = id.map(str::to_string);
                }
                collect_option_values(&option["options"], &mut meta.modes);
            }
        }
    }
    if meta.models.is_empty()
        && let Some(models) = created["models"]["availableModels"].as_array()
    {
        for item in models {
            if let Some(id) = item["modelId"].as_str().map(str::trim) {
                push_unique(&mut meta.models, id);
            }
        }
    }
    if meta.modes.is_empty()
        && let Some(modes) = created["modes"]["availableModes"].as_array()
    {
        for item in modes {
            if let Some(id) = item["id"].as_str().map(str::trim) {
                push_unique(&mut meta.modes, id);
            }
        }
    }
    meta
}

fn collect_option_values(value: &Value, out: &mut Vec<String>) {
    let Some(items) = value.as_array() else {
        return;
    };
    for item in items {
        if let Some(val) = item["value"].as_str().map(str::trim) {
            push_unique(out, val);
        } else if item.get("options").is_some() {
            collect_option_values(&item["options"], out);
        }
    }
}

fn push_unique(out: &mut Vec<String>, item: &str) {
    if !item.is_empty() && !out.iter().any(|existing| existing == item) {
        out.push(item.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    type SendHandler = Box<dyn FnMut(&Value, &mut VecDeque<Recv>)>;

    struct FakeTransport {
        sent: Arc<Mutex<Vec<Value>>>,
        incoming: VecDeque<Recv>,
        on_send: Option<SendHandler>,
        stderr: String,
    }

    fn run_fake(
        fake: FakeTransport,
        config: &AcpConfig,
        prompt_text: &str,
        cancel: &AtomicBool,
        on_text: &mut dyn FnMut(&str) -> bool,
        on_log: &mut dyn FnMut(&str),
    ) -> Result<TurnResult, String> {
        Session::new(
            Box::new(fake),
            config,
            Duration::from_secs(5),
            cancel,
            on_text,
            on_log,
        )
        .run_turn(config, Path::new("/tmp"), prompt_text)
    }

    fn probe_fake(fake: FakeTransport, config: &AcpConfig) -> Result<ProbeResult, String> {
        let cancel = AtomicBool::new(false);
        let mut noop_text = |_: &str| true;
        let mut noop_log = |_: &str| {};
        Session::new(
            Box::new(fake),
            config,
            Duration::from_secs(5),
            &cancel,
            &mut noop_text,
            &mut noop_log,
        )
        .probe_session(Path::new("/tmp"))
    }

    impl FakeTransport {
        fn new(sent: Arc<Mutex<Vec<Value>>>) -> Self {
            Self {
                sent,
                incoming: VecDeque::new(),
                on_send: None,
                stderr: String::new(),
            }
        }
    }

    impl Transport for FakeTransport {
        fn send(&mut self, value: &Value) -> Result<(), String> {
            self.sent.lock().unwrap().push(value.clone());
            if let Some(handler) = self.on_send.as_mut() {
                handler(value, &mut self.incoming);
            }
            Ok(())
        }

        fn recv(&mut self, _timeout: Duration) -> Recv {
            self.incoming.pop_front().unwrap_or(Recv::Timeout)
        }

        fn stderr_excerpt(&mut self) -> String {
            self.stderr.clone()
        }

        fn finish(self: Box<Self>) {}
    }

    fn base_config() -> AcpConfig {
        AcpConfig {
            command: "mock-acp".into(),
            ..AcpConfig::default()
        }
    }

    #[test]
    fn happy_path_streams_message_chunks_ignores_thoughts_and_logs_tools() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut fake = FakeTransport::new(sent.clone());
        fake.on_send = Some(Box::new(|msg, queue| {
            let Some(method) = msg["method"].as_str() else {
                return;
            };
            let id = msg["id"].clone();
            match method {
                "initialize" => queue.push_back(Recv::Line(
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": 1,
                            "agentInfo": { "name": "MockAgent", "version": "1.2.0" }
                        }
                    })
                    .to_string(),
                )),
                "session/new" => queue.push_back(Recv::Line(
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "sessionId": "sess-1" }
                    })
                    .to_string(),
                )),
                "session/prompt" => {
                    queue.push_back(Recv::Line(
                        json!({
                            "jsonrpc": "2.0",
                            "method": "session/update",
                            "params": {
                                "sessionId": "sess-1",
                                "update": {
                                    "sessionUpdate": "agent_thought_chunk",
                                    "content": { "type": "text", "text": "internal thought" }
                                }
                            }
                        })
                        .to_string(),
                    ));
                    queue.push_back(Recv::Line(
                        json!({
                            "jsonrpc": "2.0",
                            "method": "session/update",
                            "params": {
                                "sessionId": "sess-1",
                                "update": {
                                    "sessionUpdate": "tool_call",
                                    "toolCallId": "tc1",
                                    "kind": "read",
                                    "title": "Read src/lib.rs"
                                }
                            }
                        })
                        .to_string(),
                    ));
                    queue.push_back(Recv::Line(
                        json!({
                            "jsonrpc": "2.0",
                            "method": "session/update",
                            "params": {
                                "sessionId": "sess-1",
                                "update": {
                                    "sessionUpdate": "agent_message_chunk",
                                    "content": { "type": "text", "text": "Hello " }
                                }
                            }
                        })
                        .to_string(),
                    ));
                    queue.push_back(Recv::Line(
                        json!({
                            "jsonrpc": "2.0",
                            "method": "session/update",
                            "params": {
                                "sessionId": "sess-1",
                                "update": {
                                    "sessionUpdate": "agent_message_chunk",
                                    "content": [{ "type": "text", "text": "world!" }]
                                }
                            }
                        })
                        .to_string(),
                    ));
                    queue.push_back(Recv::Line(
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": { "stopReason": "end_turn" }
                        })
                        .to_string(),
                    ));
                }
                _ => {}
            }
        }));

        let mut chunks = Vec::new();
        let mut logs = Vec::new();
        let cancel = AtomicBool::new(false);
        let result = run_fake(
            fake,
            &base_config(),
            "say hello",
            &cancel,
            &mut |chunk| {
                chunks.push(chunk.to_string());
                true
            },
            &mut |line| logs.push(line.to_string()),
        )
        .unwrap();

        assert_eq!(result.text, "Hello world!");
        assert_eq!(result.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(result.agent.as_deref(), Some("MockAgent 1.2.0"));
        assert_eq!(chunks, ["Hello ", "world!"]);
        assert_eq!(
            logs,
            [
                "connected to MockAgent 1.2.0",
                "tool [read]: Read src/lib.rs"
            ]
        );

        let sent = sent.lock().unwrap();
        assert_eq!(sent[0]["method"], "initialize");
        assert_eq!(
            sent[0]["params"]["clientCapabilities"],
            json!({"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false})
        );
        assert_eq!(sent[1]["method"], "session/new");
        assert_eq!(sent[2]["method"], "session/prompt");
        assert_eq!(
            sent[2]["params"]["prompt"],
            json!([{"type": "text", "text": "say hello"}])
        );
    }

    #[test]
    fn config_options_and_legacy_model_mode_selection_work() {
        // Case 1: configOptions uses session/set_config_option.
        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut fake = FakeTransport::new(sent.clone());
        fake.on_send = Some(Box::new(|msg, queue| {
            let id = msg["id"].clone();
            match msg["method"].as_str() {
                Some("initialize") => queue.push_back(Recv::Line(
                    json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":1}}).to_string(),
                )),
                Some("session/new") => queue.push_back(Recv::Line(
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "sessionId": "s1",
                            "configOptions": [
                                {
                                    "id": "mode",
                                    "category": "mode",
                                    "options": [
                                        { "value": "ask", "name": "Ask" },
                                        { "group": "Write", "options": [{ "value": "code", "name": "Code" }] }
                                    ]
                                },
                                {
                                    "configId": "model_id",
                                    "category": "model",
                                    "options": [{ "value": "sonnet", "name": "Sonnet" }]
                                }
                            ]
                        }
                    })
                    .to_string(),
                )),
                Some(_) => queue.push_back(Recv::Line(
                    json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}}).to_string(),
                )),
                None => {}
            }
        }));
        let cfg = AcpConfig {
            model: "sonnet".into(),
            mode: "code".into(),
            ..base_config()
        };
        let cancel = AtomicBool::new(false);
        run_fake(fake, &cfg, "hi", &cancel, &mut |_| true, &mut |_| {}).unwrap();
        {
            let sent = sent.lock().unwrap();
            assert_eq!(sent[2]["method"], "session/set_config_option");
            assert_eq!(
                sent[2]["params"],
                json!({"sessionId": "s1", "configId": "mode", "value": "code"})
            );
            assert_eq!(sent[3]["method"], "session/set_config_option");
            assert_eq!(
                sent[3]["params"],
                json!({"sessionId": "s1", "configId": "model_id", "value": "sonnet"})
            );
            assert_eq!(sent[4]["method"], "session/prompt");
        }

        // Case 2: legacy session/new shape uses session/set_mode and session/set_model.
        let sent_legacy = Arc::new(Mutex::new(Vec::new()));
        let mut fake_legacy = FakeTransport::new(sent_legacy.clone());
        fake_legacy.on_send = Some(Box::new(|msg, queue| {
            let id = msg["id"].clone();
            match msg["method"].as_str() {
                Some("initialize") => queue.push_back(Recv::Line(
                    json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":1}}).to_string(),
                )),
                Some("session/new") => queue.push_back(Recv::Line(
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "sessionId": "s2",
                            "modes": { "availableModes": [{ "id": "architect" }] },
                            "models": { "availableModels": [{ "modelId": "o3" }] }
                        }
                    })
                    .to_string(),
                )),
                Some(_) => queue.push_back(Recv::Line(
                    json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}}).to_string(),
                )),
                None => {}
            }
        }));
        run_fake(fake_legacy, &cfg, "hi", &cancel, &mut |_| true, &mut |_| {}).unwrap();
        let sent_legacy = sent_legacy.lock().unwrap();
        assert_eq!(sent_legacy[2]["method"], "session/set_mode");
        assert_eq!(
            sent_legacy[2]["params"],
            json!({"sessionId": "s2", "modeId": "code"})
        );
        assert_eq!(sent_legacy[3]["method"], "session/set_model");
        assert_eq!(
            sent_legacy[3]["params"],
            json!({"sessionId": "s2", "modelId": "sonnet"})
        );
    }

    #[test]
    fn probe_parses_grouped_config_options_and_legacy_lists() {
        let mut fake = FakeTransport::new(Arc::new(Mutex::new(Vec::new())));
        fake.on_send = Some(Box::new(|msg, queue| {
            let id = msg["id"].clone();
            match msg["method"].as_str() {
                Some("initialize") => queue.push_back(Recv::Line(
                    json!({"jsonrpc":"2.0","id":id,"result":{"agentInfo":{"name":"Cursor"}}}).to_string(),
                )),
                Some("session/new") => queue.push_back(Recv::Line(
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "sessionId": "s1",
                            "configOptions": [
                                {
                                    "id": "model",
                                    "category": "model",
                                    "options": [
                                        { "value": "m1" },
                                        { "group": "Fast", "options": [{ "value": "m2" }, { "value": "m1" }] }
                                    ]
                                },
                                {
                                    "id": "mode",
                                    "category": "mode",
                                    "options": [{ "value": "ask" }, { "value": "agent" }]
                                }
                            ]
                        }
                    })
                    .to_string(),
                )),
                _ => {}
            }
        }));
        let probed = probe_fake(fake, &base_config()).unwrap();
        assert_eq!(probed.agent.as_deref(), Some("Cursor"));
        assert_eq!(probed.models, ["m1", "m2"]);
        assert_eq!(probed.modes, ["ask", "agent"]);
    }

    #[test]
    fn permissions_honor_read_only_toggle_and_unsupported_requests_fail_cleanly() {
        for read_only in [true, false] {
            let sent = Arc::new(Mutex::new(Vec::new()));
            let mut fake = FakeTransport::new(sent.clone());
            fake.on_send = Some(Box::new(|msg, queue| {
                let id = msg["id"].clone();
                match msg["method"].as_str() {
                    Some("initialize") => queue.push_back(Recv::Line(
                        json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":1}}).to_string(),
                    )),
                    Some("session/new") => queue.push_back(Recv::Line(
                        json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":"s1"}}).to_string(),
                    )),
                    Some("session/prompt") => {
                        let perm = |req_id: u64, kind: &str, title: &str| {
                            Recv::Line(
                                json!({
                                    "jsonrpc": "2.0",
                                    "id": req_id,
                                    "method": "session/request_permission",
                                    "params": {
                                        "sessionId": "s1",
                                        "toolCall": { "toolCallId": format!("tc{req_id}"), "kind": kind, "title": title },
                                        "options": [
                                            { "optionId": "allow", "kind": "allow_once", "name": "Allow" },
                                            { "optionId": "deny", "kind": "reject_once", "name": "Reject" }
                                        ]
                                    }
                                })
                                .to_string(),
                            )
                        };
                        queue.push_back(perm(10, "read", "Read Cargo.toml"));
                        queue.push_back(perm(11, "edit", "Write src/main.rs"));
                        queue.push_back(perm(12, "execute", "Run cargo test"));
                        queue.push_back(Recv::Line(
                            json!({
                                "jsonrpc": "2.0",
                                "id": 13,
                                "method": "fs/write_text_file",
                                "params": { "path": "x", "content": "y" }
                            })
                            .to_string(),
                        ));
                        queue.push_back(Recv::Line(
                            json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}})
                                .to_string(),
                        ));
                    }
                    _ => {}
                }
            }));

            let cfg = AcpConfig {
                read_only,
                ..base_config()
            };
            let mut logs = Vec::new();
            run_fake(
                fake,
                &cfg,
                "go",
                &AtomicBool::new(false),
                &mut |_| true,
                &mut |line| logs.push(line.to_string()),
            )
            .unwrap();

            let sent = sent.lock().unwrap();
            let reply_for = |req_id: u64| {
                sent.iter()
                    .find(|m| m.get("id") == Some(&json!(req_id)) && m.get("method").is_none())
                    .unwrap()
                    .clone()
            };
            assert_eq!(reply_for(10)["result"]["outcome"]["optionId"], "allow");
            let expected_write = if read_only { "deny" } else { "allow" };
            assert_eq!(
                reply_for(11)["result"]["outcome"]["optionId"],
                expected_write
            );
            assert_eq!(
                reply_for(12)["result"]["outcome"]["optionId"],
                expected_write
            );
            assert_eq!(reply_for(13)["error"]["code"], JSONRPC_METHOD_NOT_FOUND);
            if read_only {
                assert!(
                    logs.iter()
                        .any(|l| l.contains("denied tool [edit] in read-only mode"))
                );
            } else {
                assert!(logs.iter().any(|l| l.contains("approved tool [edit]")));
            }
        }
    }

    #[test]
    fn cancel_sends_session_cancel_and_eof_reports_stderr() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut fake = FakeTransport::new(sent.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_trigger = cancel.clone();
        fake.on_send = Some(Box::new(move |msg, queue| {
            let id = msg["id"].clone();
            match msg["method"].as_str() {
                Some("initialize") => queue.push_back(Recv::Line(
                    json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":1}}).to_string(),
                )),
                Some("session/new") => queue.push_back(Recv::Line(
                    json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":"s-cancel"}}).to_string(),
                )),
                Some("session/prompt") => {
                    cancel_trigger.store(true, Ordering::Relaxed);
                }
                _ => {}
            }
        }));
        let err = run_fake(
            fake,
            &base_config(),
            "go",
            &cancel,
            &mut |_| true,
            &mut |_| {},
        )
        .unwrap_err();
        assert_eq!(err, CANCELLED);
        assert!(sent.lock().unwrap().iter().any(|m| {
            m["method"] == "session/cancel" && m["params"]["sessionId"] == "s-cancel"
        }));

        let mut eof_fake = FakeTransport::new(Arc::new(Mutex::new(Vec::new())));
        eof_fake.stderr = "auth login required".into();
        eof_fake.incoming.push_back(Recv::Eof);
        let err = run_fake(
            eof_fake,
            &base_config(),
            "go",
            &AtomicBool::new(false),
            &mut |_| true,
            &mut |_| {},
        )
        .unwrap_err();
        assert!(
            err.contains("exited before answering `initialize`"),
            "{err}"
        );
        assert!(err.contains("auth login required"), "{err}");
    }

    #[test]
    fn refusal_and_malformed_json_are_errors() {
        let mut refusal_fake = FakeTransport::new(Arc::new(Mutex::new(Vec::new())));
        refusal_fake.on_send = Some(Box::new(|msg, queue| {
            let id = msg["id"].clone();
            match msg["method"].as_str() {
                Some("initialize") => queue.push_back(Recv::Line(
                    json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":1}}).to_string(),
                )),
                Some("session/new") => queue.push_back(Recv::Line(
                    json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":"s1"}}).to_string(),
                )),
                Some("session/prompt") => queue.push_back(Recv::Line(
                    json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"refusal"}}).to_string(),
                )),
                _ => {}
            }
        }));
        let err = run_fake(
            refusal_fake,
            &base_config(),
            "go",
            &AtomicBool::new(false),
            &mut |_| true,
            &mut |_| {},
        )
        .unwrap_err();
        assert!(err.contains("refused"), "{err}");

        let mut bad_json_fake = FakeTransport::new(Arc::new(Mutex::new(Vec::new())));
        bad_json_fake.stderr = "unexpected panic".into();
        bad_json_fake
            .incoming
            .push_back(Recv::Line("not json".into()));
        let err = run_fake(
            bad_json_fake,
            &base_config(),
            "go",
            &AtomicBool::new(false),
            &mut |_| true,
            &mut |_| {},
        )
        .unwrap_err();
        assert!(err.contains("malformed ACP JSON"), "{err}");
        assert!(err.contains("unexpected panic"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn finish_kills_lingering_grandchildren_holding_pipes_open() {
        let args = vec!["-c".to_string(), "sleep 30 & exit 0".to_string()];
        let transport = ProcessTransport::spawn("sh", &args, Path::new("/tmp")).unwrap();
        let started = Instant::now();
        Box::new(transport).finish();
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "finish() waited too long for grandchild holding pipes open: {:?}",
            started.elapsed()
        );
    }
}
