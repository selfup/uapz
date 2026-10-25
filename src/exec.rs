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
//! stdin or an agent's user message. Database nodes take data from their input file; the
//! wire can only supply their SQL, and only when the node's own query is empty.
//!
//! [`write_sql`] is the inspector's helper: it describes the database's schema with
//! [`describe_schema`] and asks an agent for one statement.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Read, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::doc::{GraphDoc, NodeDoc};
use crate::library;
use crate::node_data::{
    AgentConfig, Auth, DataConfig, DatabaseConfig, DbEngine, DirectoryConfig, ModelType, NodeData,
    OutputType, Runtime, ScriptConfig,
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
/// How deep a directory node descends.
const MAX_DIRECTORY_DEPTH: usize = 32;

/// Where the executor looks for files and writes outputs. `root` is the working
/// directory, which directory nodes read.
#[derive(Clone, Debug)]
pub struct Dirs {
    pub root: PathBuf,
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
            root: root.to_path_buf(),
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
            NodeData::Database(config) => self.run_database(config, &input),
            NodeData::Agent(config) => self.run_agent(config, &name, &input),
            NodeData::Directory(config) => self.run_directory(config),
        }
    }

    // ----- directories -----

    /// Read a directory as text: the files that pass the filters, then their contents up
    /// to the budget. Hidden entries and symlinks are never read.
    fn run_directory(&mut self, config: &DirectoryConfig) -> Result<Output, String> {
        let dir = validate::resolve_directory_inside(&self.dirs.root, &config.path)?;
        let scan = scan_directory(&dir, config)?;
        let listed = scan.files.len();
        self.log(format!(
            "{}: {} file{} matched, {listed} listed",
            config.display_path(),
            scan.matched,
            if scan.matched == 1 { "" } else { "s" }
        ));
        let text = render_directory(&dir, config, &scan);
        Ok(Output::new(
            text.into_bytes(),
            true,
            &format!(
                "Directory · {listed} file{}",
                if listed == 1 { "" } else { "s" }
            ),
        ))
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

    /// Run the node's query, or the SQL arriving on the wire when the node has none. Wire
    /// SQL is checked here exactly as the inspector checks a typed query, with the node's
    /// read-only flag, since nothing validated it earlier.
    fn run_database(&mut self, config: &DatabaseConfig, input: &str) -> Result<Output, String> {
        let query = if config.query.trim().is_empty() {
            let wired = validate::unfenced(input);
            if wired.is_empty() {
                return Err("the query is empty and nothing wired in supplies one".into());
            }
            self.log(format!(
                "query from the wire: {}",
                excerpt(wired.as_bytes())
            ));
            wired
        } else {
            if !input.is_empty() {
                self.log("upstream text is ignored: the node has its own query (use the input file for data)");
            }
            config.query.clone()
        };
        validate::sql(&query, config.read_only)?;
        let config = &DatabaseConfig {
            query,
            ..config.clone()
        };
        let file = match config.input.trim() {
            "" => None,
            path => Some(validate::resolve_inside(&self.dirs.data, path)?),
        };
        let plan = match config.engine {
            DbEngine::Sqlite => {
                let db = validate::resolve_inside(&self.dirs.data, &config.path)?;
                sqlite_plan(config, &db, file.as_deref())?
            }
            DbEngine::DuckDb => {
                let db = validate::resolve_inside(&self.dirs.data, &config.path)?;
                duckdb_plan(config, &db, file.as_deref())?
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
        fits_context(config, input)?;
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

// ----- directory scans -----

/// The files a directory node found, as `/`-separated paths relative to the directory
/// with their sizes, sorted and cut to the node's limit; `matched` counts before the cut.
struct Scan {
    files: Vec<(String, u64)>,
    matched: usize,
}

fn glob_set(patterns: &[String]) -> Result<globset::GlobSet, String> {
    let mut builder = globset::GlobSetBuilder::new();
    for pattern in patterns {
        let glob = globset::Glob::new(pattern.trim()).map_err(|e| e.to_string())?;
        builder.add(glob);
    }
    builder.build().map_err(|e| e.to_string())
}

/// Walk `dir` the way a repository tool would: hidden entries skipped, symlinks not
/// followed, `.gitignore` honored when the node asks, bounded depth. Unreadable entries
/// are skipped silently; the listing says how many files were found.
fn scan_directory(dir: &Path, config: &DirectoryConfig) -> Result<Scan, String> {
    let include = glob_set(&config.include)?;
    let exclude = glob_set(&config.exclude)?;
    let walker = ignore::WalkBuilder::new(dir)
        .hidden(true)
        .follow_links(false)
        .git_ignore(config.gitignore)
        .git_global(config.gitignore)
        .git_exclude(config.gitignore)
        .ignore(config.gitignore)
        .parents(config.gitignore)
        .require_git(false)
        .max_depth(Some(MAX_DIRECTORY_DEPTH))
        .build();
    let mut files = Vec::new();
    for entry in walker.flatten() {
        let Some(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() || !kind.is_file() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(dir) else {
            continue;
        };
        let parts: Vec<&str> = rel.iter().filter_map(|c| c.to_str()).collect();
        if parts.len() != rel.iter().count() {
            continue;
        }
        let rel = parts.join("/");
        if (!config.include.is_empty() && !include.is_match(&rel)) || exclude.is_match(&rel) {
            continue;
        }
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        files.push((rel, size));
    }
    files.sort();
    let matched = files.len();
    files.truncate(config.max_files as usize);
    Ok(Scan { files, matched })
}

/// The directory as text: a header, one line per file, then each file's contents under
/// a `=== path ===` line until the byte budget is spent. Binary files are named, not
/// shown.
fn render_directory(dir: &Path, config: &DirectoryConfig, scan: &Scan) -> String {
    let listed = scan.files.len();
    let mut text = format!(
        "Directory {}: {} file{} matched",
        config.display_path(),
        scan.matched,
        if scan.matched == 1 { "" } else { "s" }
    );
    if listed < scan.matched {
        text.push_str(&format!(", first {listed} listed"));
    }
    text.push('\n');
    for (rel, bytes) in &scan.files {
        text.push_str(&format!("  {rel} ({})\n", size(*bytes as usize)));
    }
    if !config.contents {
        return text;
    }
    let mut budget = config.max_bytes as usize;
    for (i, (rel, _)) in scan.files.iter().enumerate() {
        if budget == 0 {
            let left = listed - i;
            text.push_str(&format!(
                "\n({left} more file{} not shown: the {} content budget is spent)\n",
                if left == 1 { "" } else { "s" },
                size(config.max_bytes as usize)
            ));
            break;
        }
        text.push_str(&format!("\n=== {rel} ===\n"));
        let (bytes, cut) = match read_prefix(&dir.join(rel), budget) {
            Ok(read) => read,
            Err(e) => {
                text.push_str(&format!("(unreadable: {e})\n"));
                continue;
            }
        };
        if bytes.contains(&0) {
            text.push_str("(binary, not shown)\n");
            continue;
        }
        budget -= bytes.len();
        let content = String::from_utf8_lossy(&bytes);
        text.push_str(&content);
        if !content.ends_with('\n') {
            text.push('\n');
        }
        if cut {
            text.push_str("… (cut here: the content budget is spent)\n");
            budget = 0;
        }
    }
    text
}

/// Up to `max` bytes of a file and whether more remained.
fn read_prefix(path: &Path, max: usize) -> Result<(Vec<u8>, bool), String> {
    let mut file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut bytes = Vec::new();
    (&mut file)
        .take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let cut = bytes.len() > max;
    bytes.truncate(max);
    Ok((bytes, cut))
}

/// Refuse to send a prompt and input that can't fit the agent's context window, at four
/// characters per token.
fn fits_context(config: &AgentConfig, input: &str) -> Result<(), String> {
    let estimate = (config.prompt.len() + input.len()) / 4;
    if estimate as u64 > u64::from(config.context_size) {
        return Err(format!(
            "prompt and input are about {estimate} tokens, over the context size of {}",
            config.context_size
        ));
    }
    Ok(())
}

// ----- writing SQL with an agent -----

const PG_SCHEMA_SQL: &str = "SELECT table_schema, table_name, column_name, data_type \
    FROM information_schema.columns \
    WHERE table_schema NOT IN ('pg_catalog', 'information_schema') \
    ORDER BY table_schema, table_name, ordinal_position";
const MYSQL_SCHEMA_SQL: &str = "SELECT table_name, column_name, data_type \
    FROM information_schema.columns WHERE table_schema = DATABASE() \
    ORDER BY table_name, ordinal_position";

/// A plain-text description of the database for a model that writes SQL: the engine's own
/// schema listing (`.schema` on the file engines, `information_schema` on the servers,
/// read-only either way) plus the input table's columns when the node loads an input file.
pub fn describe_schema(config: &DatabaseConfig, dirs: &Dirs, env: Env) -> Result<String, String> {
    let plan = match config.engine {
        DbEngine::Sqlite | DbEngine::DuckDb => {
            let db = validate::resolve_inside(&dirs.data, &config.path)?;
            let program = match config.engine {
                DbEngine::Sqlite => "sqlite3",
                _ => "duckdb",
            };
            Plan {
                program: program.into(),
                args: ["-batch", "-bail", "-readonly"]
                    .into_iter()
                    .map(String::from)
                    .chain(std::iter::once(plain_path(&db)?))
                    .collect(),
                env: Vec::new(),
                stdin: ".schema\n".into(),
                json: false,
            }
        }
        DbEngine::Postgres => postgres_plan(
            &with_query(config, PG_SCHEMA_SQL),
            &env_secret(env, &config.url_env)?,
        )?,
        DbEngine::Mysql => mysql_plan(
            &with_query(config, MYSQL_SCHEMA_SQL),
            &env_secret(env, &config.url_env)?,
        )?,
    };
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
    let mut schema = String::from_utf8_lossy(&run.stdout).trim().to_string();
    if schema.is_empty() {
        schema.push_str("(no tables yet)");
    }
    if !config.input.trim().is_empty() {
        let file = validate::resolve_inside(&dirs.data, &config.input)?;
        let columns = match validate::extension(&config.input).as_deref() {
            Some("csv" | "tsv") => first_line(&file)
                .map(|header| format!(" with columns: {header}"))
                .unwrap_or_default(),
            _ => String::new(),
        };
        schema.push_str(&format!(
            "\n\nTable {} is loaded from {}{columns}",
            config.input_table.trim(),
            config.input.trim()
        ));
    }
    Ok(schema)
}

/// The same connection, asked a read-only question of our own.
fn with_query(config: &DatabaseConfig, query: &str) -> DatabaseConfig {
    DatabaseConfig {
        query: query.into(),
        read_only: true,
        input: String::new(),
        ..config.clone()
    }
}

fn env_secret(env: Env, name: &str) -> Result<String, String> {
    env(name).ok_or_else(|| format!("environment variable {name} is not set"))
}

/// The first line of a text file, for a CSV header; bounded so a huge file costs nothing.
fn first_line(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut line = String::new();
    std::io::BufReader::new(file.take(64 * 1024))
        .read_line(&mut line)
        .ok()?;
    let line = line.trim();
    (!line.is_empty()).then(|| line.to_string())
}

/// Ask an agent for one SQL statement for a database node, given its schema and what the
/// user wants. The agent's own prompt is replaced by SQL-writing instructions; its
/// provider, model and settings are kept. The reply is unfenced but not checked: the
/// inspector runs the checker on it, so the user sees what came back even when it is
/// refused.
pub fn write_sql(
    agent: &AgentConfig,
    config: &DatabaseConfig,
    request: &str,
    dirs: &Dirs,
    http: &dyn Http,
    env: Env,
) -> Result<String, String> {
    if agent.model_type != ModelType::Generative {
        return Err(format!(
            "{} is {} model, not a generative one",
            agent.model.trim(),
            match agent.model_type {
                ModelType::Embedding => "an embedding",
                _ => "an image",
            }
        ));
    }
    let request = request.trim();
    if request.is_empty() {
        return Err("describe what the query should do first".into());
    }
    let schema = match describe_schema(config, dirs, env) {
        Ok(schema) => schema,
        Err(e) => format!("(schema unavailable: {e})"),
    };
    let key = match &agent.auth {
        Auth::Env { var } => Some(env_secret(env, var)?),
        Auth::None => None,
    };
    let agent = AgentConfig {
        prompt: sql_instructions(config, &schema),
        output: OutputType::Text,
        ..agent.clone()
    };
    fits_context(&agent, request)?;
    let bytes = providers::generate(http, &agent, key.as_deref(), request)?;
    let text = String::from_utf8(bytes).map_err(|_| "the model's reply is not text".to_string())?;
    let sql = validate::unfenced(&text);
    if sql.is_empty() {
        return Err("the model returned nothing".into());
    }
    Ok(sql)
}

/// The system prompt [`write_sql`] sends.
fn sql_instructions(config: &DatabaseConfig, schema: &str) -> String {
    let mut text = format!(
        "You write SQL for {}. Reply with exactly one SQL statement and nothing else: \
         no explanation, no comments, no code fences.",
        config.engine.label()
    );
    if config.read_only {
        text.push_str(
            " The statement must only read: start with SELECT, WITH, EXPLAIN, SHOW, DESCRIBE \
             or VALUES and never modify data or schema.",
        );
    }
    text.push_str("\n\nSchema:\n");
    text.push_str(schema);
    text
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
            && i + 2 < bytes.len()
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

    /// The SQLite node runs sqlite3, so a machine without it can't claim the suite passes.
    fn require(program: &str) {
        assert!(
            have(program),
            "{program} is required by this test and by the app; install it"
        );
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
        require("sqlite3");
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

    // ----- live tests -----
    //
    // These talk to a real OpenAI-compatible server and are ignored by default. Load a
    // chat model and an embedding model (for example in LM Studio) and run:
    //
    //   UAPZ_LIVE_MODEL=mistralai/ministral-3-3b \
    //   UAPZ_LIVE_EMBED_MODEL=text-embedding-nomic-embed-text-v1.5 \
    //   cargo test live_ -- --ignored --nocapture
    //
    // UAPZ_LIVE_ENDPOINT overrides the base URL; empty means LM Studio's default,
    // http://localhost:1234/v1. The replies are printed so a failure shows what the model
    // actually said.

    const LIVE_CSV: &str = "city,country\nLisbon,Portugal\nOslo,Norway\nLima,Peru\n";

    fn live_agent(model_var: &str, model_type: ModelType, output: OutputType) -> AgentConfig {
        let model = std::env::var(model_var)
            .unwrap_or_else(|_| panic!("set {model_var} to a model id loaded on the server"));
        AgentConfig {
            provider: Provider::LmStudio,
            endpoint: std::env::var("UAPZ_LIVE_ENDPOINT").unwrap_or_default(),
            model_type,
            model,
            auth: Auth::None,
            prompt: String::new(),
            context_size: 4096,
            temperature: 0.0,
            output,
        }
    }

    fn live_run(doc: &GraphDoc, id: &str, root: &Path) -> Report {
        let http = providers::UreqHttp::new(HTTP_TIMEOUT);
        let report = run(doc, id, Dirs::under(root), &http, &process_env);
        println!("log:\n  {}", report.log.join("\n  "));
        if let Ok(out) = &report.result {
            println!(
                "{} in {:.1}s:\n{}",
                out.label,
                report.elapsed.as_secs_f32(),
                out.as_text()
                    .map_or("<binary>".into(), |t| excerpt(t.as_bytes()))
            );
        }
        report
    }

    #[test]
    #[ignore = "needs a live model server; see the comment above"]
    fn live_text_flows_from_data_through_a_script_into_an_agent() {
        let root = scratch("live-text");
        fs::write(root.join("data/cities.csv"), LIVE_CSV).unwrap();
        fs::write(
            root.join("scripts/count.sh"),
            "#!/bin/sh\nprintf 'Note: the CSV has %s lines including its header.\\n' \"$(wc -l | tr -d ' ')\"\n",
        )
        .unwrap();
        let mut agent = live_agent("UAPZ_LIVE_MODEL", ModelType::Generative, OutputType::Text);
        agent.prompt = "You get a CSV file and a note about it. Reply with one sentence that \
                        names every city in the CSV."
            .into();
        let mut n2 = node("n2", Some(NodeData::Agent(agent)));
        n2.kind = Some("namer".into());
        let doc = graph(
            vec![
                node("n0", data("cities.csv", DataFormat::Csv)),
                node("n1", shell("count.sh", &[], 30)),
                n2,
            ],
            &[("n0", "n1"), ("n0", "n2"), ("n1", "n2")],
        );
        let report = live_run(&doc, "n2", &root);
        let out = report.result.expect("live text run");
        let text = out.as_text().expect("text output");
        for city in ["Lisbon", "Oslo", "Lima"] {
            assert!(text.contains(city), "reply does not mention {city}: {text}");
        }
        let saved = out.saved.clone().unwrap();
        assert!(
            saved.starts_with(root.join("agents/namer/outputs")),
            "{saved:?}"
        );
        assert_eq!(fs::read_to_string(&saved).unwrap(), text);
        assert!(
            report.log.iter().any(|l| l.contains("with 2 inputs")),
            "{:?}",
            report.log
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[ignore = "needs a live model server; see the comment above"]
    fn live_json_output_is_native_json() {
        let root = scratch("live-json");
        fs::write(root.join("data/cities.csv"), LIVE_CSV).unwrap();
        let mut agent = live_agent("UAPZ_LIVE_MODEL", ModelType::Generative, OutputType::Json);
        agent.prompt = "Return an object with two keys: \"rows\", the number of data rows in \
                        the CSV not counting the header, and \"cities\", an array of the city \
                        names in order."
            .into();
        let doc = graph(
            vec![
                node("n0", data("cities.csv", DataFormat::Csv)),
                node("n1", Some(NodeData::Agent(agent))),
            ],
            &[("n0", "n1")],
        );
        let report = live_run(&doc, "n1", &root);
        let out = report.result.expect("live json run");
        let text = out.as_text().unwrap();
        let value: serde_json::Value = serde_json::from_str(text).expect("valid JSON");
        assert_eq!(value["rows"], 3, "{text}");
        assert_eq!(value["cities"].as_array().map(Vec::len), Some(3), "{text}");
        let saved = out.saved.clone().unwrap();
        assert_eq!(saved.extension().and_then(|e| e.to_str()), Some("json"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[ignore = "needs a live model server; see the comment above"]
    fn live_embeddings_are_float_vectors() {
        let root = scratch("live-embed");
        fs::write(
            root.join("data/note.txt"),
            "Lisbon is the capital of Portugal.",
        )
        .unwrap();
        let agent = live_agent(
            "UAPZ_LIVE_EMBED_MODEL",
            ModelType::Embedding,
            OutputType::Embedding,
        );
        let doc = graph(
            vec![
                node("n0", data("note.txt", DataFormat::Text)),
                node("n1", Some(NodeData::Agent(agent))),
            ],
            &[("n0", "n1")],
        );
        let report = live_run(&doc, "n1", &root);
        let out = report.result.expect("live embedding run");
        let floats: Vec<f64> = serde_json::from_slice(&out.bytes).expect("array of numbers");
        println!("{} dimensions", floats.len());
        assert!(floats.len() >= 64, "{} dimensions", floats.len());
        assert!(floats.iter().any(|f| *f != 0.0));
        let saved = out.saved.clone().unwrap();
        assert_eq!(saved.extension().and_then(|e| e.to_str()), Some("json"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[ignore = "needs a live model server; see the comment above"]
    fn live_unknown_model_surfaces_the_servers_message() {
        let root = scratch("live-404");
        let mut agent = live_agent("UAPZ_LIVE_MODEL", ModelType::Generative, OutputType::Text);
        agent.model = "uapz-no-such-model".into();
        agent.prompt = "Say hi.".into();
        let doc = graph(vec![node("n0", Some(NodeData::Agent(agent)))], &[]);
        let err = live_run(&doc, "n0", &root).result.unwrap_err();
        println!("error: {err}");
        assert!(err.contains("HTTP 4"), "{err}");
        assert!(err.contains("uapz-no-such-model"), "{err}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[ignore = "needs a PostgreSQL server; set UAPZ_LIVE_PG_URL, e.g. postgres://localhost/postgres"]
    fn live_postgres_runs_through_psql_and_the_engine_enforces_read_only() {
        let url = std::env::var("UAPZ_LIVE_PG_URL")
            .expect("set UAPZ_LIVE_PG_URL to a PostgreSQL URL, e.g. postgres://localhost/postgres");
        let root = scratch("live-pg");
        let url_for_setup = url.clone();
        let env = move |name: &str| (name == "PG_URL").then(|| url.clone());
        let db = |query: &str, read_only: bool| {
            Some(NodeData::Database(DatabaseConfig {
                engine: DbEngine::Postgres,
                url_env: "PG_URL".into(),
                query: query.into(),
                read_only,
                ..DatabaseConfig::default()
            }))
        };
        // nextval() is not a writing keyword, so the checker lets it through, but
        // PostgreSQL refuses it inside a read-only transaction. The sequence is created
        // and dropped here, outside the executor.
        let psql = |sql: &str| {
            let status = Command::new("psql")
                .args([url_for_setup.as_str(), "--quiet", "--no-psqlrc", "-c", sql])
                .status()
                .expect("run psql");
            assert!(status.success(), "{sql}");
        };
        psql("CREATE SEQUENCE IF NOT EXISTS uapz_live_seq");
        psql("CREATE TABLE IF NOT EXISTS uapz_live_schema(x integer, y text)");
        let schema = describe_schema(
            &DatabaseConfig {
                engine: DbEngine::Postgres,
                url_env: "PG_URL".into(),
                ..DatabaseConfig::default()
            },
            &Dirs::under(&root),
            &env,
        )
        .expect("describe schema");
        println!("schema:\n{schema}");
        assert!(schema.contains("uapz_live_schema,x,integer"), "{schema}");
        psql("DROP TABLE uapz_live_schema");
        let doc = graph(
            vec![
                node("n0", db("SELECT 1 + 1 AS two, current_user AS who", true)),
                node("n1", db("SELECT nextval('uapz_live_seq') AS id", true)),
                node("n2", db("SELECT nextval('uapz_live_seq') AS id", false)),
            ],
            &[],
        );
        let http = FakeHttp::new(200, "{}");

        let report = run(&doc, "n0", Dirs::under(&root), &http, &env);
        println!("log:\n  {}", report.log.join("\n  "));
        let out = report.result.expect("read-only select");
        let text = out.as_text().unwrap();
        println!("{}:\n{text}", out.label);
        assert!(text.starts_with("two,who\n2,"), "{text}");

        let err = run(&doc, "n1", Dirs::under(&root), &http, &env)
            .result
            .unwrap_err();
        println!("read-only refusal: {err}");
        assert!(err.contains("read-only transaction"), "{err}");

        let out = run(&doc, "n2", Dirs::under(&root), &http, &env)
            .result
            .expect("write allowed when read_only is off");
        assert_eq!(out.as_text(), Some("id\n1\n"));
        psql("DROP SEQUENCE uapz_live_seq");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn wire_text_is_the_query_when_the_node_has_none() {
        require("sqlite3");
        let root = scratch("wire-sql");
        let db = root.join("data/shop.sqlite");
        let status = Command::new("sqlite3")
            .arg(&db)
            .arg("CREATE TABLE t(a INTEGER); INSERT INTO t VALUES (1),(2);")
            .status()
            .unwrap();
        assert!(status.success());
        fs::write(
            root.join("data/count.sql.md"),
            "```sql\nSELECT count(*) AS n FROM t\n```",
        )
        .unwrap();
        fs::write(root.join("data/delete.txt"), "DELETE FROM t").unwrap();
        let http = FakeHttp::new(200, "{}");
        let db_node = |query: &str, read_only: bool| {
            Some(NodeData::Database(DatabaseConfig {
                path: "shop.sqlite".into(),
                query: query.into(),
                read_only,
                ..DatabaseConfig::default()
            }))
        };
        let doc = graph(
            vec![
                node("n0", data("count.sql.md", DataFormat::Markdown)),
                node("n1", db_node("", true)),
                node("n2", data("delete.txt", DataFormat::Text)),
                node("n3", db_node("", true)),
                node("n4", db_node("", false)),
                node("n5", db_node("SELECT a FROM t ORDER BY a", true)),
            ],
            &[("n0", "n1"), ("n2", "n3"), ("n0", "n5")],
        );

        // Fenced SQL from the wire runs, and the log says where it came from.
        let report = run(&doc, "n1", Dirs::under(&root), &http, &no_env);
        let out = report.result.expect("wired query");
        assert_eq!(out.as_text().unwrap().trim(), r#"[{"n":2}]"#);
        assert!(
            report
                .log
                .iter()
                .any(|l| l.starts_with("query from the wire: SELECT count")),
            "{:?}",
            report.log
        );

        // The checker guards wire SQL with the node's read-only flag.
        let err = run(&doc, "n3", Dirs::under(&root), &http, &no_env)
            .result
            .unwrap_err();
        assert!(err.contains("read-only"), "{err}");

        // Nothing wired in and no query is an error, not a silent no-op.
        let err = run(&doc, "n4", Dirs::under(&root), &http, &no_env)
            .result
            .unwrap_err();
        assert!(err.contains("nothing wired in"), "{err}");

        // A node with its own query ignores the wire and says so.
        let report = run(&doc, "n5", Dirs::under(&root), &http, &no_env);
        assert_eq!(
            report.result.unwrap().as_text().unwrap().trim(),
            r#"[{"a":1},
{"a":2}]"#
        );
        assert!(
            report.log.iter().any(|l| l.contains("ignored")),
            "{:?}",
            report.log
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn schema_descriptions_list_tables_and_the_input_file() {
        require("sqlite3");
        let root = scratch("schema");
        let db = root.join("data/shop.sqlite");
        let status = Command::new("sqlite3")
            .arg(&db)
            .arg("CREATE TABLE orders(id INTEGER PRIMARY KEY, total REAL);")
            .status()
            .unwrap();
        assert!(status.success());
        fs::write(root.join("data/extra.csv"), "x,y\n1,2\n").unwrap();
        let config = DatabaseConfig {
            path: "shop.sqlite".into(),
            ..DatabaseConfig::default()
        };
        let schema = describe_schema(&config, &Dirs::under(&root), &no_env).unwrap();
        assert!(schema.contains("CREATE TABLE orders"), "{schema}");
        assert!(!schema.contains("loaded from"));

        let with_input = DatabaseConfig {
            input: "extra.csv".into(),
            input_table: "extra".into(),
            ..config.clone()
        };
        let schema = describe_schema(&with_input, &Dirs::under(&root), &no_env).unwrap();
        assert!(
            schema.ends_with("Table extra is loaded from extra.csv with columns: x,y"),
            "{schema}"
        );

        let missing = DatabaseConfig {
            path: "nope.sqlite".into(),
            ..config
        };
        assert!(describe_schema(&missing, &Dirs::under(&root), &no_env).is_err());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn write_sql_sends_the_schema_and_unfences_the_reply() {
        require("sqlite3");
        let root = scratch("write-sql");
        let db = root.join("data/shop.sqlite");
        let status = Command::new("sqlite3")
            .arg(&db)
            .arg("CREATE TABLE t(a INTEGER);")
            .status()
            .unwrap();
        assert!(status.success());
        let http = FakeHttp::new(
            200,
            r#"{"choices":[{"message":{"content":"```sql\nSELECT count(*) FROM t;\n```"}}]}"#,
        );
        let agent = AgentConfig {
            prompt: "You are a poet.".into(),
            output: OutputType::Json,
            ..fake_agent()
        };
        let config = DatabaseConfig {
            path: "shop.sqlite".into(),
            ..DatabaseConfig::default()
        };
        let sql = write_sql(
            &agent,
            &config,
            " count the rows in t ",
            &Dirs::under(&root),
            &http,
            &with_key,
        )
        .unwrap();
        assert_eq!(sql, "SELECT count(*) FROM t;");
        validate::sql(&sql, true).unwrap();

        let seen = http.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let body = &seen[0].body;
        let system = body["messages"][0]["content"].as_str().unwrap();
        assert!(system.starts_with("You write SQL for SQLite."), "{system}");
        assert!(system.contains("must only read"), "{system}");
        assert!(system.contains("CREATE TABLE t"), "{system}");
        assert!(!system.contains("poet"));
        assert_eq!(body["messages"][1]["content"], "count the rows in t");
        assert!(body.get("response_format").is_none(), "{body}");
        assert_eq!(seen[0].headers[0].1, "Bearer secret");
        drop(seen);

        // A writable node drops the read-only instruction; a missing database is reported
        // inside the prompt rather than refusing to help.
        let writable = DatabaseConfig {
            path: "new.sqlite".into(),
            read_only: false,
            ..config.clone()
        };
        write_sql(
            &agent,
            &writable,
            "x",
            &Dirs::under(&root),
            &http,
            &with_key,
        )
        .ok();
        let seen = http.seen.lock().unwrap();
        let system = seen[1].body["messages"][0]["content"].as_str().unwrap();
        assert!(!system.contains("must only read"), "{system}");
        assert!(system.contains("schema unavailable"), "{system}");
        drop(seen);

        let embedder = AgentConfig {
            model_type: ModelType::Embedding,
            ..agent.clone()
        };
        let err = write_sql(
            &embedder,
            &config,
            "x",
            &Dirs::under(&root),
            &http,
            &with_key,
        )
        .unwrap_err();
        assert!(err.contains("embedding"), "{err}");
        let err =
            write_sql(&agent, &config, "  ", &Dirs::under(&root), &http, &with_key).unwrap_err();
        assert!(err.contains("describe"), "{err}");
        let err = write_sql(&agent, &config, "x", &Dirs::under(&root), &http, &no_env).unwrap_err();
        assert!(err.contains("UAPZ_TEST_KEY"), "{err}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[ignore = "needs a live model server; see the comment above"]
    fn live_agent_writes_a_query_that_runs() {
        let root = scratch("live-sql");
        let db = root.join("data/cities.sqlite");
        let status = Command::new("sqlite3")
            .arg(&db)
            .arg("CREATE TABLE cities(city TEXT, country TEXT); INSERT INTO cities VALUES ('Lisbon','Portugal'),('Oslo','Norway'),('Lima','Peru');")
            .status()
            .unwrap();
        assert!(status.success());
        let agent = live_agent("UAPZ_LIVE_MODEL", ModelType::Generative, OutputType::Text);
        let config = DatabaseConfig {
            path: "cities.sqlite".into(),
            ..DatabaseConfig::default()
        };
        let http = providers::UreqHttp::new(HTTP_TIMEOUT);
        let sql = write_sql(
            &agent,
            &config,
            "How many cities are there?",
            &Dirs::under(&root),
            &http,
            &process_env,
        )
        .expect("live write_sql");
        println!("model wrote: {sql}");
        validate::sql(&sql, true).expect("passes the read-only checker");

        let doc = graph(
            vec![node(
                "n0",
                Some(NodeData::Database(DatabaseConfig {
                    query: sql,
                    ..config
                })),
            )],
            &[],
        );
        let report = live_run(&doc, "n0", &root);
        let text = report
            .result
            .expect("query runs")
            .as_text()
            .unwrap()
            .to_string();
        assert!(text.contains('3'), "{text}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn directory_nodes_list_and_read_files_within_limits() {
        let root = scratch("dir");
        fs::create_dir_all(root.join("repo/src")).unwrap();
        fs::create_dir_all(root.join("repo/target")).unwrap();
        fs::create_dir_all(root.join("repo/.git")).unwrap();
        fs::write(root.join("repo/.gitignore"), "/target\n").unwrap();
        fs::write(root.join("repo/README.md"), "# demo\n").unwrap();
        fs::write(root.join("repo/src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("repo/src/lib.rs"), "pub fn lib() {}").unwrap();
        fs::write(root.join("repo/src/notes.txt"), "notes").unwrap();
        fs::write(root.join("repo/target/out.rs"), "// build output\n").unwrap();
        fs::write(root.join("repo/.git/HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(
            root.join("repo/logo.png"),
            [0x89, b'P', b'N', b'G', 0, 1, 2],
        )
        .unwrap();
        fs::write(root.join("outside.rs"), "// not in the repo\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("outside.rs"), root.join("repo/src/link.rs")).unwrap();
        let http = FakeHttp::new(200, "{}");
        let dir = |config: DirectoryConfig| Some(NodeData::Directory(config));
        let doc = graph(
            vec![
                node(
                    "n0",
                    dir(DirectoryConfig {
                        path: "repo".into(),
                        ..DirectoryConfig::default()
                    }),
                ),
                node(
                    "n1",
                    dir(DirectoryConfig {
                        path: "repo".into(),
                        include: vec!["**/*.rs".into(), "*.md".into()],
                        exclude: vec!["src/lib*".into()],
                        contents: false,
                        ..DirectoryConfig::default()
                    }),
                ),
                node(
                    "n2",
                    dir(DirectoryConfig {
                        path: "repo/src".into(),
                        max_files: 1,
                        max_bytes: 5,
                        ..DirectoryConfig::default()
                    }),
                ),
                node(
                    "n3",
                    dir(DirectoryConfig {
                        path: "repo".into(),
                        gitignore: false,
                        include: vec!["target/**".into()],
                        ..DirectoryConfig::default()
                    }),
                ),
                node(
                    "n4",
                    dir(DirectoryConfig {
                        path: "../elsewhere".into(),
                        ..DirectoryConfig::default()
                    }),
                ),
                node(
                    "n5",
                    dir(DirectoryConfig {
                        path: "repo/README.md".into(),
                        ..DirectoryConfig::default()
                    }),
                ),
            ],
            &[],
        );

        // Everything readable, with .gitignore, hidden entries and symlinks honored.
        let report = run(&doc, "n0", Dirs::under(&root), &http, &no_env);
        let out = report.result.expect("directory read");
        let text = out.as_text().unwrap();
        assert!(
            text.starts_with("Directory repo: 5 files matched\n"),
            "{text}"
        );
        for listed in [
            "  README.md (7 B)",
            "  src/main.rs",
            "  src/lib.rs",
            "  src/notes.txt",
            "  logo.png",
        ] {
            assert!(text.contains(listed), "{text}");
        }
        for skipped in [
            "target",
            "HEAD",
            "link.rs",
            "not in the repo",
            "build output",
        ] {
            assert!(!text.contains(skipped), "{skipped} in {text}");
        }
        assert!(
            text.contains("=== src/main.rs ===\nfn main() {}\n"),
            "{text}"
        );
        assert!(
            text.contains("=== logo.png ===\n(binary, not shown)\n"),
            "{text}"
        );
        assert_eq!(
            out.label,
            format!("Directory · 5 files · {}", size(out.bytes.len()))
        );
        assert!(
            report.log[1].contains("5 files matched, 5 listed"),
            "{:?}",
            report.log
        );

        // Include and exclude globs; listing only.
        let text = run(&doc, "n1", Dirs::under(&root), &http, &no_env)
            .result
            .unwrap();
        let text = text.as_text().unwrap();
        assert!(
            text.starts_with("Directory repo: 2 files matched\n"),
            "{text}"
        );
        assert!(
            text.contains("README.md") && text.contains("src/main.rs"),
            "{text}"
        );
        assert!(
            !text.contains("lib.rs") && !text.contains("notes.txt"),
            "{text}"
        );
        assert!(!text.contains("==="), "{text}");

        // File and byte limits: one file listed, its content cut at five bytes.
        let text = run(&doc, "n2", Dirs::under(&root), &http, &no_env)
            .result
            .unwrap();
        let text = text.as_text().unwrap();
        assert!(
            text.starts_with("Directory repo/src: 3 files matched, first 1 listed\n"),
            "{text}"
        );
        assert!(
            text.contains("=== lib.rs ===\npub f\n… (cut here"),
            "{text}"
        );

        // Without .gitignore the build output is readable.
        let text = run(&doc, "n3", Dirs::under(&root), &http, &no_env)
            .result
            .unwrap();
        assert!(
            text.as_text()
                .unwrap()
                .contains("=== target/out.rs ===\n// build output\n")
        );

        let err = run(&doc, "n4", Dirs::under(&root), &http, &no_env)
            .result
            .unwrap_err();
        assert!(err.contains("not valid") && err.contains(".."), "{err}");
        let err = run(&doc, "n5", Dirs::under(&root), &http, &no_env)
            .result
            .unwrap_err();
        assert!(err.contains("not a directory"), "{err}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn directory_text_flows_into_an_agent() {
        let root = scratch("dir-agent");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/add.rs"),
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
        )
        .unwrap();
        let http = FakeHttp::new(
            200,
            r##"{"choices":[{"message":{"content":"#[test] fn adds() {}"}}]}"##,
        );
        let mut agent = fake_agent();
        agent.prompt = "Write tests.".into();
        let doc = graph(
            vec![
                node(
                    "n0",
                    Some(NodeData::Directory(DirectoryConfig {
                        include: vec!["src/*.rs".into()],
                        ..DirectoryConfig::default()
                    })),
                ),
                node("n1", Some(NodeData::Agent(agent))),
            ],
            &[("n0", "n1")],
        );
        let out = run(&doc, "n1", Dirs::under(&root), &http, &with_key)
            .result
            .expect("agent run");
        assert_eq!(out.as_text(), Some("#[test] fn adds() {}"));
        let seen = http.seen.lock().unwrap();
        let user = seen[0].body["messages"][1]["content"].as_str().unwrap();
        assert!(
            user.starts_with("Directory .: 1 file matched\n  src/add.rs"),
            "{user}"
        );
        assert!(user.contains("=== src/add.rs ===\npub fn add"), "{user}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[ignore = "needs a live model server; see the comment above"]
    fn live_agent_writes_tests_for_a_directory() {
        let root = scratch("live-dir");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/math.rs"),
            "/// Adds two numbers.\npub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        )
        .unwrap();
        let mut agent = live_agent("UAPZ_LIVE_MODEL", ModelType::Generative, OutputType::Text);
        agent.prompt = "You get a Rust source tree. Write one Rust unit test for each public \
                        function in it. Reply with Rust code only, no explanation."
            .into();
        let doc = graph(
            vec![
                node(
                    "n0",
                    Some(NodeData::Directory(DirectoryConfig {
                        include: vec!["src/**/*.rs".into()],
                        ..DirectoryConfig::default()
                    })),
                ),
                node("n1", Some(NodeData::Agent(agent))),
            ],
            &[("n0", "n1")],
        );
        let report = live_run(&doc, "n1", &root);
        let out = report.result.expect("live directory → agent run");
        let text = out.as_text().unwrap();
        assert!(text.contains("#[test]"), "{text}");
        assert!(text.contains("add("), "{text}");
        fs::remove_dir_all(&root).unwrap();
    }
}
