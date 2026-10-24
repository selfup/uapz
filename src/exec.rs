//! Running nodes.
//!
//! [`run`] executes one node of a [`GraphDoc`] after running everything wired into it and
//! returns a [`Report`]: the output plus a log of what happened. The contract with the rest
//! of the app:
//!
//! - every config is validated again here, whatever the inspector showed;
//! - every file path goes through [`validate::resolve_inside`] right before it is opened,
//!   so symlinks can't lead out of `data/` or `scripts/`;
//! - scripts and database command-line tools run with a minimal environment
//!   ([`INHERITED_ENV`]) plus exactly the secret they need, with stdin for their input and
//!   a timeout after which they are killed;
//! - secrets (API keys, connection URLs) are read from the environment at the last moment
//!   and handed only to the request or child process that needs them, never to argv, logs
//!   or disk;
//! - agent results are written with [`library::write_agent_output`].
//!
//! Text flows along the wires: a node's upstream outputs are joined and become a script's
//! stdin or an agent's user message. Database nodes take data from their input file, not
//! from the wire.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::doc::{GraphDoc, NodeDoc};
use crate::library;
use crate::node_data::{
    AgentConfig, Auth, DataConfig, DatabaseConfig, DbEngine, NodeData, OutputType, Runtime,
    ScriptConfig,
};
use crate::providers::{self, Http};
use crate::validate;

pub const HTTP_TIMEOUT: Duration = Duration::from_secs(300);
pub const DB_TIMEOUT: Duration = Duration::from_secs(300);
/// Largest data file a node will read.
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Largest stdout a child process may produce.
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
/// Environment variables child processes inherit. Nothing else leaks in.
pub const INHERITED_ENV: &[&str] = &["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "TERM"];
const POLL: Duration = Duration::from_millis(25);

/// Where the executor looks for files and writes outputs.
#[derive(Clone, Debug)]
pub struct Dirs {
    pub data: PathBuf,
    pub scripts: PathBuf,
    pub agents: PathBuf,
}

impl Dirs {
    /// The app's directories relative to the working directory.
    pub fn from_cwd() -> Self {
        Self::under(Path::new("."))
    }

    pub fn under(root: &Path) -> Self {
        Self {
            data: root.join(library::DATA_DIR),
            scripts: root.join(library::SCRIPTS_DIR),
            agents: root.join(library::AGENTS_DIR),
        }
    }
}

/// What a node produced.
#[derive(Clone, Debug, PartialEq)]
pub struct Output {
    pub bytes: Vec<u8>,
    /// Whether `bytes` is UTF-8 text that downstream nodes can take.
    pub text: bool,
    /// Short description for logs and the inspector, e.g. `CSV · 12.3 KB`.
    pub label: String,
    /// Where the output was written, for agents.
    pub saved: Option<PathBuf>,
}

impl Output {
    fn new(bytes: Vec<u8>, textual: bool, what: &str) -> Self {
        let text = textual && std::str::from_utf8(&bytes).is_ok();
        let label = format!("{what} · {}", size(bytes.len()));
        Self {
            bytes,
            text,
            label,
            saved: None,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        if self.text {
            std::str::from_utf8(&self.bytes).ok()
        } else {
            None
        }
    }
}

#[derive(Debug)]
pub struct Report {
    pub log: Vec<String>,
    pub result: Result<Output, String>,
    pub elapsed: Duration,
}

/// Looks up an environment variable. The real one is [`process_env`]; tests inject keys.
pub type Env<'a> = &'a (dyn Fn(&str) -> Option<String> + Sync);

pub fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Run node `id` (a document id such as `n3`) and everything wired into it.
pub fn run(doc: &GraphDoc, id: &str, dirs: Dirs, http: &dyn Http, env: Env) -> Report {
    let start = Instant::now();
    let mut runner = Runner {
        doc,
        dirs,
        http,
        env,
        log: Vec::new(),
        done: HashMap::new(),
        active: HashSet::new(),
    };
    let result = runner.run_node(id);
    Report {
        log: runner.log,
        result,
        elapsed: start.elapsed(),
    }
}

struct Runner<'a> {
    doc: &'a GraphDoc,
    dirs: Dirs,
    http: &'a dyn Http,
    env: Env<'a>,
    log: Vec<String>,
    /// Outputs of nodes already run, so a node feeding two others runs once.
    done: HashMap<String, Output>,
    /// Nodes on the current path, for cycle detection.
    active: HashSet<String>,
}

impl Runner<'_> {
    fn log(&mut self, line: impl Into<String>) {
        self.log.push(line.into());
    }

    fn node(&self, id: &str) -> Result<&NodeDoc, String> {
        self.doc
            .nodes
            .iter()
            .find(|n| n.id == id)
            .ok_or_else(|| format!("{id} is not in the graph"))
    }

    /// Ids of the nodes wired into `id`, in a stable order.
    fn upstream(&self, id: &str) -> Vec<String> {
        let mut from: Vec<String> = self
            .doc
            .edges
            .iter()
            .filter(|e| e.to == id)
            .map(|e| e.from.clone())
            .collect();
        from.sort();
        from.dedup();
        from
    }

    fn secret(&self, name: &str) -> Result<String, String> {
        (self.env)(name).ok_or_else(|| format!("environment variable {name} is not set"))
    }

    fn run_node(&mut self, id: &str) -> Result<Output, String> {
        if let Some(done) = self.done.get(id) {
            return Ok(done.clone());
        }
        if !self.active.insert(id.to_string()) {
            return Err(format!("{id} is wired in a cycle"));
        }
        let result = self.run_fresh(id);
        self.active.remove(id);
        if let Ok(output) = &result {
            self.done.insert(id.to_string(), output.clone());
        }
        result
    }

    fn run_fresh(&mut self, id: &str) -> Result<Output, String> {
        let node = self.node(id)?.clone();
        let Some(data) = &node.data else {
            return Err(format!("{id} is blank; give it content first"));
        };
        let issues = data.validate();
        if let Some(first) = issues.first() {
            let more = match issues.len() {
                1 => String::new(),
                n => format!(" (and {} more)", n - 1),
            };
            return Err(format!(
                "{id} is not valid: {}: {}{more}",
                first.field, first.message
            ));
        }

        let upstream = self.upstream(id);
        let mut texts = Vec::with_capacity(upstream.len());
        for from in &upstream {
            let output = self.run_node(from)?;
            match output.as_text() {
                Some(text) => texts.push(text.to_string()),
                None => {
                    return Err(format!(
                        "{from} produced binary output ({}), which {id} can't take as text",
                        output.label
                    ));
                }
            }
        }
        let input = texts.join("\n\n");
        self.log(format!(
            "{id}: {} ({}) with {} input{}",
            data.node_type().label().to_lowercase(),
            data.summary(),
            upstream.len(),
            if upstream.len() == 1 { "" } else { "s" }
        ));

        let name = node.kind.clone().unwrap_or_else(|| id.to_string());
        match data {
            NodeData::Data(config) => self.run_data(config),
            NodeData::Script(config) => self.run_script(config, &input),
            NodeData::Database(config) => {
                if !input.is_empty() {
                    self.log(format!(
                        "{id}: database nodes ignore upstream text; use the input file"
                    ));
                }
                self.run_database(config)
            }
            NodeData::Agent(config) => self.run_agent(config, &name, &input),
        }
    }

    // ----- data -----

    fn run_data(&mut self, config: &DataConfig) -> Result<Output, String> {
        let path = validate::resolve_inside(&self.dirs.data, &config.path)?;
        let bytes = read_bounded(&path, MAX_FILE_BYTES)?;
        Ok(Output::new(
            bytes,
            config.format.is_text(),
            config.format.label(),
        ))
    }

    // ----- scripts -----

    fn run_script(&mut self, config: &ScriptConfig, input: &str) -> Result<Output, String> {
        let path = validate::resolve_inside(&self.dirs.scripts, &config.path)?;
        if !validate::has_extension(&config.path, config.runtime.extensions()) {
            return Err(format!(
                "{} is not a {} script",
                config.path,
                config.runtime.label()
            ));
        }
        let cwd = self
            .dirs
            .scripts
            .canonicalize()
            .map_err(|e| format!("{}: {e}", self.dirs.scripts.display()))?;
        let (program, mut args) = script_command(config.runtime, &path);
        args.extend(config.args.iter().cloned());
        self.log(format!("$ {program} {}", args.join(" ")));
        let timeout = Duration::from_secs(u64::from(config.timeout_secs));
        let run = run_process(&program, &args, &cwd, &[], input.as_bytes(), timeout)?;
        if run.timed_out {
            return Err(format!(
                "{} timed out after {}s and was killed",
                config.path, config.timeout_secs
            ));
        }
        if !run.stderr.is_empty() {
            self.log(format!("stderr: {}", excerpt(&run.stderr)));
        }
        if !run.status.success() {
            return Err(format!(
                "{} exited with {}: {}",
                config.path,
                describe_status(run.status),
                excerpt(&run.stderr)
            ));
        }
        Ok(Output::new(run.stdout, true, "stdout"))
    }

    // ----- databases -----

    fn run_database(&mut self, config: &DatabaseConfig) -> Result<Output, String> {
        let input = match config.input.trim() {
            "" => None,
            path => Some(validate::resolve_inside(&self.dirs.data, path)?),
        };
        let plan = match config.engine {
            DbEngine::Sqlite => {
                let db = validate::resolve_inside(&self.dirs.data, &config.path)?;
                sqlite_plan(config, &db, input.as_deref())?
            }
            DbEngine::DuckDb => {
                let db = validate::resolve_inside(&self.dirs.data, &config.path)?;
                duckdb_plan(config, &db, input.as_deref())?
            }
            DbEngine::Postgres => postgres_plan(config, &self.secret(&config.url_env)?)?,
            DbEngine::Mysql => mysql_plan(config, &self.secret(&config.url_env)?)?,
        };
        // Plans keep secrets in `env`, so argv is safe to log.
        self.log(format!("$ {} {}", plan.program, plan.args.join(" ")));
        let run = run_process(
            &plan.program,
            &plan.args,
            Path::new("."),
            &plan.env,
            plan.stdin.as_bytes(),
            DB_TIMEOUT,
        )?;
        if run.timed_out {
            return Err(format!(
                "{} timed out after {}s and was killed",
                plan.program,
                DB_TIMEOUT.as_secs()
            ));
        }
        if !run.status.success() {
            return Err(format!(
                "{} exited with {}: {}",
                plan.program,
                describe_status(run.status),
                excerpt(&run.stderr)
            ));
        }
        let mut stdout = run.stdout;
        if stdout.is_empty() && plan.json {
            stdout = b"[]".to_vec();
        }
        Ok(Output::new(stdout, true, config.engine.label()))
    }

    // ----- agents -----

    fn run_agent(
        &mut self,
        config: &AgentConfig,
        name: &str,
        input: &str,
    ) -> Result<Output, String> {
        let key = match &config.auth {
            Auth::Env { var } => Some(self.secret(var)?),
            Auth::None => None,
        };
        let estimate = (config.prompt.len() + input.len()) / 4;
        if estimate as u64 > u64::from(config.context_size) {
            return Err(format!(
                "prompt and input are about {estimate} tokens, over the context size of {}",
                config.context_size
            ));
        }
        self.log(format!(
            "→ {} {} at {}",
            config.provider.label(),
            config.model.trim(),
            config.endpoint_url()
        ));
        let bytes = providers::generate(self.http, config, key.as_deref(), input)?;
        let saved = library::write_agent_output(&self.dirs.agents, name, config.output, &bytes)?;
        self.log(format!("saved {}", saved.display()));
        let mut output = Output::new(
            bytes,
            config.output != OutputType::Image,
            config.output.label(),
        );
        output.saved = Some(saved);
        Ok(output)
    }
}

// ----- files -----

fn read_bounded(path: &Path, max: u64) -> Result<Vec<u8>, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if meta.len() > max {
        return Err(format!(
            "{} is {} , over the {} limit",
            path.display(),
            size(meta.len() as usize),
            size(max as usize)
        ));
    }
    std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Human-readable byte count.
pub fn size(bytes: usize) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// The start of a child's stderr or stdout for messages: lossy UTF-8, trimmed, bounded.
fn excerpt(bytes: &[u8]) -> String {
    const MAX: usize = 400;
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    if text.chars().count() <= MAX {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(MAX).collect::<String>())
    }
}

// ----- child processes -----

struct Finished {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    timed_out: bool,
}

/// Run a program with a clean environment ([`INHERITED_ENV`] plus `env`), feeding `stdin`
/// and collecting both output streams, killing it when `timeout` passes.
fn run_process(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &[(String, String)],
    stdin: &[u8],
    timeout: Duration,
) -> Result<Finished, String> {
    let mut command = Command::new(program);
    command.args(args).current_dir(cwd).env_clear();
    // Its own process group, so a timeout can take grandchildren (a script's `sleep`, a
    // shell's pipeline) down with it instead of leaving them holding the pipes open.
    #[cfg(unix)]
    command.process_group(0);
    for name in INHERITED_ENV {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    for (name, value) in env {
        command.env(name, value);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("`{program}` is not installed or not on PATH"),
        _ => format!("could not start `{program}`: {e}"),
    })?;

    // Feed stdin and drain both outputs on their own threads so a chatty child can't
    // deadlock against a full pipe.
    let input = stdin.to_vec();
    let mut stdin_pipe = child.stdin.take();
    let writer = thread::spawn(move || {
        if let Some(pipe) = stdin_pipe.as_mut() {
            let _ = pipe.write_all(&input);
        }
        drop(stdin_pipe);
    });
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout = thread::spawn(move || stdout_pipe.map(read_capped).unwrap_or_default());
    let stderr = thread::spawn(move || stderr_pipe.map(read_capped).unwrap_or_default());

    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|e| format!("waiting for `{program}`: {e}"))?
        {
            break status;
        }
        if start.elapsed() >= timeout {
            timed_out = true;
            kill_group(child.id());
            let _ = child.kill();
            break child
                .wait()
                .map_err(|e| format!("waiting for `{program}`: {e}"))?;
        }
        thread::sleep(POLL);
    };
    let _ = writer.join();
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    if stdout.len() > MAX_OUTPUT_BYTES {
        return Err(format!(
            "`{program}` produced more than {} of output",
            size(MAX_OUTPUT_BYTES)
        ));
    }
    Ok(Finished {
        status,
        stdout,
        stderr,
        timed_out,
    })
}

/// Kill every process in the group `pid` leads. On other platforms only the child itself
/// is killed, by the caller.
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

/// Read a stream to the end, keeping at most one byte over the cap so the caller can tell.
fn read_capped(mut reader: impl Read) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if out.len() <= MAX_OUTPUT_BYTES {
                    out.extend_from_slice(&buf[..n]);
                }
            }
        }
    }
    out
}

fn describe_status(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("code {code}"),
        None => "a signal".to_string(),
    }
}

/// The interpreter and leading arguments for a script file.
fn script_command(runtime: Runtime, path: &Path) -> (String, Vec<String>) {
    let file = path.display().to_string();
    match runtime {
        Runtime::Python => ("uv".into(), vec!["run".into(), "--quiet".into(), file]),
        Runtime::Shell => {
            let shell = match validate::extension(&file).as_deref() {
                Some("bash") => "bash",
                Some("zsh") => "zsh",
                _ => "sh",
            };
            (shell.into(), vec![file])
        }
        Runtime::Node => ("node".into(), vec![file]),
    }
}

// ----- database plans -----

/// A database command-line invocation. Secrets live in `env`; `args` and `stdin` are safe
/// to show.
#[derive(Clone, Debug, PartialEq)]
struct Plan {
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    stdin: String,
    /// Whether empty stdout means an empty JSON array.
    json: bool,
}

fn query_statement(config: &DatabaseConfig) -> String {
    format!("{};\n", config.query.trim().trim_end_matches(';').trim())
}

/// A filesystem path for a dot-command or SQL literal: no quotes or newlines allowed.
fn plain_path(path: &Path) -> Result<String, String> {
    let text = path
        .to_str()
        .ok_or_else(|| format!("{} is not valid UTF-8", path.display()))?;
    if text.contains(['\'', '"', '\n', '\r']) {
        return Err(format!(
            "{} contains a quote or newline, which the database shell can't take",
            path.display()
        ));
    }
    Ok(text.to_string())
}

/// `sqlite3 -json [-readonly] <db>` with an optional CSV import into the `temp` schema,
/// which stays writable on a read-only connection and vanishes with the process.
fn sqlite_plan(config: &DatabaseConfig, db: &Path, input: Option<&Path>) -> Result<Plan, String> {
    let mut args: Vec<String> = ["-batch", "-bail", "-json"]
        .into_iter()
        .map(String::from)
        .collect();
    if config.read_only {
        args.push("-readonly".into());
    }
    args.push(plain_path(db)?);
    let mut stdin = String::new();
    if let Some(file) = input {
        validate::identifier(&config.input_table)?;
        stdin.push_str(&format!(
            ".import --csv --schema temp \"{}\" {}\n",
            plain_path(file)?,
            config.input_table.trim()
        ));
    }
    stdin.push_str(&query_statement(config));
    Ok(Plan {
        program: "sqlite3".into(),
        args,
        env: Vec::new(),
        stdin,
        json: true,
    })
}

/// `duckdb -json [-readonly] <db>` with the input loaded into a temporary table through
/// the reader for its format.
fn duckdb_plan(config: &DatabaseConfig, db: &Path, input: Option<&Path>) -> Result<Plan, String> {
    let mut args: Vec<String> = ["-batch", "-bail", "-json"]
        .into_iter()
        .map(String::from)
        .collect();
    if config.read_only {
        args.push("-readonly".into());
    }
    args.push(plain_path(db)?);
    let mut stdin = String::new();
    if let Some(file) = input {
        validate::identifier(&config.input_table)?;
        let reader = match validate::extension(&config.input).as_deref() {
            Some("csv" | "tsv") => "read_csv_auto",
            Some("json" | "ndjson" | "jsonl") => "read_json_auto",
            Some("parquet") => "read_parquet",
            _ => return Err(format!("{} is not a format DuckDB can load", config.input)),
        };
        stdin.push_str(&format!(
            "CREATE TEMP TABLE {} AS SELECT * FROM {reader}('{}');\n",
            config.input_table.trim(),
            plain_path(file)?
        ));
    }
    stdin.push_str(&query_statement(config));
    Ok(Plan {
        program: "duckdb".into(),
        args,
        env: Vec::new(),
        stdin,
        json: true,
    })
}

/// The pieces of a `scheme://user:password@host:port/database?k=v` URL, decoded.
#[derive(Debug, PartialEq)]
struct DbUrl {
    host: String,
    port: Option<u16>,
    user: Option<String>,
    password: Option<String>,
    database: Option<String>,
    params: Vec<(String, String)>,
}

fn parse_db_url(raw: &str, schemes: &[&str]) -> Result<DbUrl, String> {
    let url = url::Url::parse(raw.trim()).map_err(|e| format!("connection URL: {e}"))?;
    if !schemes.contains(&url.scheme()) {
        return Err(format!(
            "connection URL scheme must be {}, not `{}`",
            schemes.join(" or "),
            url.scheme()
        ));
    }
    let host = url
        .host_str()
        .ok_or("connection URL has no host")?
        .trim_matches(['[', ']'])
        .to_string();
    let decode = |s: &str| percent_decode(s);
    let user = (!url.username().is_empty()).then(|| decode(url.username()));
    let password = url.password().map(decode);
    let database = {
        let path = url.path().trim_start_matches('/');
        (!path.is_empty()).then(|| decode(path))
    };
    let params = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    Ok(DbUrl {
        host,
        port: url.port(),
        user,
        password,
        database,
        params,
    })
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len() + 0
            && let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3])
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `psql --csv` with the connection in libpq's environment variables, never in argv. With
/// `read_only`, the session is switched to read-only transactions before the query.
fn postgres_plan(config: &DatabaseConfig, url: &str) -> Result<Plan, String> {
    let parts = parse_db_url(url, &["postgres", "postgresql"])?;
    let mut env = vec![("PGHOST".to_string(), parts.host)];
    if let Some(port) = parts.port {
        env.push(("PGPORT".into(), port.to_string()));
    }
    if let Some(user) = parts.user {
        env.push(("PGUSER".into(), user));
    }
    if let Some(password) = parts.password {
        env.push(("PGPASSWORD".into(), password));
    }
    if let Some(database) = parts.database {
        env.push(("PGDATABASE".into(), database));
    }
    for (key, value) in parts.params {
        if key == "sslmode" {
            env.push(("PGSSLMODE".into(), value));
        }
    }
    let mut stdin = String::new();
    if config.read_only {
        stdin.push_str("SET default_transaction_read_only = on;\n");
    }
    stdin.push_str(&query_statement(config));
    Ok(Plan {
        program: "psql".into(),
        args: [
            "--csv",
            "--no-psqlrc",
            "--quiet",
            "--no-password",
            "--set",
            "ON_ERROR_STOP=1",
        ]
        .into_iter()
        .map(String::from)
        .collect(),
        env,
        stdin,
        json: false,
    })
}

/// `mysql --batch` with host, user and database in argv and the password in `MYSQL_PWD`.
fn mysql_plan(config: &DatabaseConfig, url: &str) -> Result<Plan, String> {
    let parts = parse_db_url(url, &["mysql"])?;
    let mut args: Vec<String> = vec!["--batch".into(), "--host".into(), parts.host];
    if let Some(port) = parts.port {
        args.push("--port".into());
        args.push(port.to_string());
    }
    if let Some(user) = parts.user {
        args.push("--user".into());
        args.push(user);
    }
    if let Some(database) = parts.database {
        args.push("--database".into());
        args.push(database);
    }
    let mut env = Vec::new();
    if let Some(password) = parts.password {
        env.push(("MYSQL_PWD".to_string(), password));
    }
    let mut stdin = String::new();
    if config.read_only {
        stdin.push_str("SET SESSION TRANSACTION READ ONLY;\n");
    }
    stdin.push_str(&query_statement(config));
    Ok(Plan {
        program: "mysql".into(),
        args,
        env,
        stdin,
        json: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{EdgeDoc, View};
    use crate::node_data::{DataFormat, ModelType, Provider};
    use crate::providers::tests::FakeHttp;
    use std::fs;

    fn scratch(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("uapz-exec-{tag}-{}-{nanos}", std::process::id()));
        for dir in [library::DATA_DIR, library::SCRIPTS_DIR, library::AGENTS_DIR] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        root
    }

    fn node(id: &str, data: Option<NodeData>) -> NodeDoc {
        NodeDoc {
            id: id.into(),
            x: 0.0,
            y: 0.0,
            kind: None,
            color: None,
            data,
        }
    }

    fn graph(nodes: Vec<NodeDoc>, edges: &[(&str, &str)]) -> GraphDoc {
        GraphDoc {
            version: 1,
            name: "t".into(),
            view: View::default(),
            nodes,
            edges: edges
                .iter()
                .map(|(from, to)| EdgeDoc {
                    from: from.to_string(),
                    to: to.to_string(),
                })
                .collect(),
        }
    }

    fn data(path: &str, format: DataFormat) -> Option<NodeData> {
        Some(NodeData::Data(DataConfig {
            path: path.into(),
            format,
        }))
    }

    fn shell(path: &str, args: &[&str], timeout_secs: u32) -> Option<NodeData> {
        Some(NodeData::Script(ScriptConfig {
            runtime: Runtime::Shell,
            path: path.into(),
            args: args.iter().map(|a| a.to_string()).collect(),
            timeout_secs,
        }))
    }

    fn fake_agent() -> AgentConfig {
        AgentConfig {
            provider: Provider::Custom,
            endpoint: "http://127.0.0.1:1/v1".into(),
            model: "m".into(),
            prompt: "Summarize.".into(),
            auth: Auth::Env {
                var: "UAPZ_TEST_KEY".into(),
            },
            ..AgentConfig::default()
        }
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn with_key(name: &str) -> Option<String> {
        (name == "UAPZ_TEST_KEY").then(|| "secret".to_string())
    }

    fn have(program: &str) -> bool {
        Command::new(program)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    }

    #[test]
    fn data_nodes_read_files_inside_data() {
        let root = scratch("data");
        fs::write(root.join("data/a.csv"), "x,y\n1,2\n").unwrap();
        fs::write(root.join("data/img.png"), [0x89, b'P', b'N', b'G', 0, 0xff]).unwrap();
        let http = FakeHttp::new(200, "{}");
        let doc = graph(
            vec![
                node("n0", data("a.csv", DataFormat::Csv)),
                node("n1", data("img.png", DataFormat::Image)),
                node("n2", data("../secret", DataFormat::Text)),
                node("n3", None),
            ],
            &[],
        );
        let report = run(&doc, "n0", Dirs::under(&root), &http, &no_env);
        let out = report.result.unwrap();
        assert_eq!(out.as_text(), Some("x,y\n1,2\n"));
        assert_eq!(out.label, "CSV · 8 B");
        assert_eq!(report.log.len(), 1, "{:?}", report.log);

        let out = run(&doc, "n1", Dirs::under(&root), &http, &no_env)
            .result
            .unwrap();
        assert!(!out.text && out.as_text().is_none());

        let err = run(&doc, "n2", Dirs::under(&root), &http, &no_env)
            .result
            .unwrap_err();
        assert!(err.contains("not valid") && err.contains(".."), "{err}");
        let err = run(&doc, "n3", Dirs::under(&root), &http, &no_env)
            .result
            .unwrap_err();
        assert!(err.contains("blank"), "{err}");
        let err = run(&doc, "n9", Dirs::under(&root), &http, &no_env)
            .result
            .unwrap_err();
        assert!(err.contains("not in the graph"), "{err}");
        assert!(
            read_bounded(&root.join("data/a.csv"), 3)
                .unwrap_err()
                .contains("limit")
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn chains_feed_text_downstream_and_save_agent_output() {
        let root = scratch("chain");
        fs::write(root.join("data/a.csv"), "x,y\n1,2\n").unwrap();
        fs::write(root.join("data/b.txt"), "note").unwrap();
        let http = FakeHttp::new(200, r#"{"choices":[{"message":{"content":"two rows"}}]}"#);
        let mut agent = node("n2", Some(NodeData::Agent(fake_agent())));
        agent.kind = Some("summarizer".into());
        let doc = graph(
            vec![
                node("n0", data("a.csv", DataFormat::Csv)),
                node("n1", data("b.txt", DataFormat::Text)),
                agent,
            ],
            &[("n0", "n2"), ("n1", "n2")],
        );
        let report = run(&doc, "n2", Dirs::under(&root), &http, &with_key);
        let out = report.result.expect("agent run");
        assert_eq!(out.as_text(), Some("two rows"));
        let saved = out.saved.unwrap();
        assert!(
            saved.starts_with(root.join("agents/summarizer/outputs")),
            "{saved:?}"
        );
        let file = saved.file_name().unwrap().to_str().unwrap();
        assert_eq!(file.len(), "20261024T153045123Z.txt".len(), "{file}");
        assert!(file.ends_with("Z.txt"));
        assert_eq!(fs::read_to_string(&saved).unwrap(), "two rows");

        let seen = http.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].url, "http://127.0.0.1:1/v1/chat/completions");
        assert_eq!(seen[0].headers[0].1, "Bearer secret");
        assert_eq!(seen[0].body["messages"][1]["content"], "x,y\n1,2\n\n\nnote");
        assert!(
            report.log.iter().any(|l| l.contains("with 2 inputs")),
            "{:?}",
            report.log
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn agents_without_a_preset_save_under_their_id() {
        let root = scratch("id");
        let http = FakeHttp::new(200, r#"{"choices":[{"message":{"content":"ok"}}]}"#);
        let doc = graph(vec![node("n7", Some(NodeData::Agent(fake_agent())))], &[]);
        let out = run(&doc, "n7", Dirs::under(&root), &http, &with_key)
            .result
            .unwrap();
        assert!(
            out.saved
                .unwrap()
                .starts_with(root.join("agents/n7/outputs"))
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn missing_keys_cycles_binary_inputs_and_context_are_errors() {
        let root = scratch("errors");
        fs::write(root.join("data/img.png"), [0u8, 1, 2]).unwrap();
        let http = FakeHttp::new(200, "{}");

        let doc = graph(vec![node("n0", Some(NodeData::Agent(fake_agent())))], &[]);
        let err = run(&doc, "n0", Dirs::under(&root), &http, &no_env)
            .result
            .unwrap_err();
        assert_eq!(err, "environment variable UAPZ_TEST_KEY is not set");

        let doc = graph(
            vec![
                node("n0", Some(NodeData::Agent(fake_agent()))),
                node("n1", Some(NodeData::Agent(fake_agent()))),
            ],
            &[("n0", "n1"), ("n1", "n0")],
        );
        let err = run(&doc, "n0", Dirs::under(&root), &http, &with_key)
            .result
            .unwrap_err();
        assert!(err.contains("cycle"), "{err}");

        let doc = graph(
            vec![
                node("n0", data("img.png", DataFormat::Image)),
                node("n1", Some(NodeData::Agent(fake_agent()))),
            ],
            &[("n0", "n1")],
        );
        let err = run(&doc, "n1", Dirs::under(&root), &http, &with_key)
            .result
            .unwrap_err();
        assert!(err.contains("binary output"), "{err}");

        let tiny = AgentConfig {
            context_size: 1,
            ..fake_agent()
        };
        let doc = graph(vec![node("n0", Some(NodeData::Agent(tiny)))], &[]);
        let err = run(&doc, "n0", Dirs::under(&root), &http, &with_key)
            .result
            .unwrap_err();
        assert!(err.contains("over the context size"), "{err}");
        assert!(http.seen.lock().unwrap().is_empty(), "nothing was sent");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn shell_scripts_get_stdin_args_a_clean_env_and_a_timeout() {
        let root = scratch("scripts");
        fs::write(
            root.join("scripts/echo.sh"),
            "echo \"args:$*\"; echo \"user:${USER:-none}\"; cat\n",
        )
        .unwrap();
        fs::write(root.join("scripts/slow.sh"), "sleep 5\n").unwrap();
        fs::write(root.join("scripts/fail.sh"), "echo boom >&2; exit 3\n").unwrap();
        fs::write(root.join("data/in.txt"), "payload").unwrap();
        let http = FakeHttp::new(200, "{}");
        let doc = graph(
            vec![
                node("n0", data("in.txt", DataFormat::Text)),
                node("n1", shell("echo.sh", &["--a", "b"], 10)),
                node("n2", shell("slow.sh", &[], 1)),
                node("n3", shell("fail.sh", &[], 10)),
            ],
            &[("n0", "n1")],
        );
        let report = run(&doc, "n1", Dirs::under(&root), &http, &no_env);
        let out = report.result.expect("echo");
        assert_eq!(out.as_text(), Some("args:--a b\nuser:none\npayload"));
        assert!(
            report.log.iter().any(|l| l.starts_with("$ sh ")),
            "{:?}",
            report.log
        );

        let started = Instant::now();
        let err = run(&doc, "n2", Dirs::under(&root), &http, &no_env)
            .result
            .unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "killed promptly"
        );

        let err = run(&doc, "n3", Dirs::under(&root), &http, &no_env)
            .result
            .unwrap_err();
        assert!(err.contains("code 3") && err.contains("boom"), "{err}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn script_commands_pick_the_interpreter() {
        let path = Path::new("/x/s.py");
        assert_eq!(
            script_command(Runtime::Python, path),
            (
                "uv".to_string(),
                vec!["run".into(), "--quiet".into(), "/x/s.py".into()]
            )
        );
        assert_eq!(
            script_command(Runtime::Shell, Path::new("/x/a.zsh")).0,
            "zsh"
        );
        assert_eq!(script_command(Runtime::Shell, Path::new("/x/a.sh")).0, "sh");
        assert_eq!(
            script_command(Runtime::Node, Path::new("/x/a.mjs")).0,
            "node"
        );
    }

    #[test]
    fn sqlite_runs_read_only_and_imports_csv_into_temp() {
        if !have("sqlite3") {
            eprintln!("sqlite3 not installed; skipping");
            return;
        }
        let root = scratch("sqlite");
        let db = root.join("data/shop.sqlite");
        let status = Command::new("sqlite3")
            .arg(&db)
            .arg("CREATE TABLE t(a INTEGER); INSERT INTO t VALUES (1),(2);")
            .status()
            .unwrap();
        assert!(status.success());
        fs::write(root.join("data/extra.csv"), "x,y\n1,2\n3,4\n").unwrap();
        let before = fs::read(&db).unwrap();
        let http = FakeHttp::new(200, "{}");

        let count = DatabaseConfig {
            path: "shop.sqlite".into(),
            query: "SELECT count(*) AS n FROM t".into(),
            ..DatabaseConfig::default()
        };
        let joined = DatabaseConfig {
            path: "shop.sqlite".into(),
            input: "extra.csv".into(),
            input_table: "extra".into(),
            query: "SELECT (SELECT count(*) FROM t) AS rows, x, y FROM extra ORDER BY x".into(),
            ..DatabaseConfig::default()
        };
        let empty = DatabaseConfig {
            path: "shop.sqlite".into(),
            query: "SELECT * FROM t WHERE a > 10".into(),
            ..DatabaseConfig::default()
        };
        let doc = graph(
            vec![
                node("n0", Some(NodeData::Database(count))),
                node("n1", Some(NodeData::Database(joined))),
                node("n2", Some(NodeData::Database(empty))),
            ],
            &[],
        );
        let out = run(&doc, "n0", Dirs::under(&root), &http, &no_env)
            .result
            .expect("count");
        assert_eq!(out.as_text().unwrap().trim(), r#"[{"n":2}]"#);

        let out = run(&doc, "n1", Dirs::under(&root), &http, &no_env)
            .result
            .expect("import");
        assert_eq!(
            out.as_text().unwrap().trim(),
            r#"[{"rows":2,"x":"1","y":"2"},
{"rows":2,"x":"3","y":"4"}]"#
        );
        let out = run(&doc, "n2", Dirs::under(&root), &http, &no_env)
            .result
            .expect("empty");
        assert_eq!(out.as_text().unwrap().trim(), "[]");
        assert_eq!(
            fs::read(&db).unwrap(),
            before,
            "read-only left the file alone"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn database_plans_keep_secrets_out_of_argv() {
        let config = DatabaseConfig {
            engine: DbEngine::Postgres,
            url_env: "DATABASE_URL".into(),
            query: "SELECT 1;".into(),
            ..DatabaseConfig::default()
        };
        let plan = postgres_plan(
            &config,
            "postgres://ann:p%40ss@db.example.com:5433/shop?sslmode=require",
        )
        .unwrap();
        assert_eq!(plan.program, "psql");
        assert!(
            !plan
                .args
                .iter()
                .any(|a| a.contains("p@ss") || a.contains("ann"))
        );
        assert!(
            plan.env
                .contains(&("PGHOST".into(), "db.example.com".into()))
        );
        assert!(plan.env.contains(&("PGPORT".into(), "5433".into())));
        assert!(plan.env.contains(&("PGUSER".into(), "ann".into())));
        assert!(plan.env.contains(&("PGPASSWORD".into(), "p@ss".into())));
        assert!(plan.env.contains(&("PGDATABASE".into(), "shop".into())));
        assert!(plan.env.contains(&("PGSSLMODE".into(), "require".into())));
        assert_eq!(
            plan.stdin,
            "SET default_transaction_read_only = on;\nSELECT 1;\n"
        );
        assert!(postgres_plan(&config, "mysql://x@h/d").is_err());

        let writable = DatabaseConfig {
            engine: DbEngine::Mysql,
            read_only: false,
            ..config
        };
        let plan = mysql_plan(&writable, "mysql://root:pw@localhost/shop").unwrap();
        assert_eq!(plan.program, "mysql");
        assert!(!plan.args.iter().any(|a| a == "pw"));
        assert_eq!(plan.env, vec![("MYSQL_PWD".to_string(), "pw".to_string())]);
        assert!(plan.args.contains(&"--database".to_string()));
        assert_eq!(plan.stdin, "SELECT 1;\n");
        assert!(mysql_plan(&writable, "not a url").is_err());
    }

    #[test]
    fn file_database_plans_quote_paths_and_pick_readers() {
        let config = DatabaseConfig {
            engine: DbEngine::DuckDb,
            path: "x.duckdb".into(),
            input: "rows.parquet".into(),
            input_table: "rows".into(),
            query: "SELECT * FROM rows".into(),
            ..DatabaseConfig::default()
        };
        let plan = duckdb_plan(
            &config,
            Path::new("/d/x.duckdb"),
            Some(Path::new("/d/rows.parquet")),
        )
        .unwrap();
        assert_eq!(
            plan.args,
            ["-batch", "-bail", "-json", "-readonly", "/d/x.duckdb"]
        );
        assert_eq!(
            plan.stdin,
            "CREATE TEMP TABLE rows AS SELECT * FROM read_parquet('/d/rows.parquet');\nSELECT * FROM rows;\n"
        );
        assert!(duckdb_plan(&config, Path::new("/d/it's.duckdb"), None).is_err());

        let sqlite = DatabaseConfig {
            engine: DbEngine::Sqlite,
            input: "rows.csv".into(),
            read_only: false,
            ..config
        };
        let plan = sqlite_plan(
            &sqlite,
            Path::new("/d/x.db"),
            Some(Path::new("/d/rows.csv")),
        )
        .unwrap();
        assert!(!plan.args.contains(&"-readonly".to_string()));
        assert!(
            plan.stdin
                .starts_with(".import --csv --schema temp \"/d/rows.csv\" rows\n")
        );
    }

    #[test]
    fn sizes_and_url_decoding() {
        assert_eq!(size(8), "8 B");
        assert_eq!(size(12_600), "12.3 KB");
        assert_eq!(size(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(percent_decode("p%40ss%2Fw"), "p@ss/w");
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
        let parts = parse_db_url("postgresql://h/db", &["postgresql"]).unwrap();
        assert_eq!(parts.host, "h");
        assert_eq!(parts.user, None);
        assert_eq!(parts.database.as_deref(), Some("db"));
    }
}
