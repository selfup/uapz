//! Running nodes.
//!
//! [`run_with`] executes one node of a [`GraphDoc`] after running everything wired into it
//! and returns a [`Report`]: the output plus a log of what happened. The contract with the
//! rest of the app:
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
//! - agent results are written with [`library::write_agent_output`];
//! - write-file nodes only touch paths checked by [`validate::resolve_writable_inside`],
//!   including the destination after following symlinks, so they stay in the working
//!   directory and out of the app's own folders.
//!
//! Text flows along the wires: a node's upstream outputs are joined and become a script's
//! stdin, an agent's user message or a write-file node's content. Database nodes take data
//! from their input file; the wire can only supply their SQL, and only when the node's own
//! query is empty. An agent can also place its inputs by name: `{{schema}}` in its prompt
//! or message stands for the output of the upstream node named `schema` (or the node
//! `n3` for `{{n3}}`), and the inputs no placeholder names follow as before.
//!
//! [`run_all_with`] runs several targets in one pass, and [`sinks`] picks the targets
//! that cover the whole graph, for the inspector's Run all. Every output's
//! [`content_key`] is what the run history keeps it under, through [`Cache::keep`].
//!
//! The [`Control`] carries a [`Cache`] of earlier outputs so a downstream run does not
//! re-call every upstream agent, a cancel flag checked between steps and inside
//! child-process and streaming waits, and a progress sink that receives log lines and
//! agent text as it streams in. Agents and scripts are looked up by their configuration,
//! their inputs' outputs and (for scripts) the script file itself; data, directory,
//! database and write-file nodes always run, since they read or change live state. Every
//! node's output is hashed for the nodes after it, so an edit that leaves an output the
//! same leaves everything downstream cached. A persistent cache also keeps every entry
//! under `cache/` as `<key>.json` + `<key>.bin`, so a restart does not re-call anything;
//! keys are content hashes, stable across runs and builds.
//!
//! [`write_sql`] is the inspector's helper: it describes the database's schema with
//! [`describe_schema`] and asks an agent for one statement.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, Read, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::doc::{GraphDoc, NodeDoc};
use crate::library;
use crate::node_data::{
    AgentConfig, Auth, DataConfig, DatabaseConfig, DbEngine, DirectoryConfig, FileConfig,
    ModelType, NodeData, NodeType, OutputType, Runtime, ScriptConfig, WriteMode,
};
use crate::providers::{self, Http};
use crate::storage;
use crate::validate;
use crate::{calls, history, review, workflow};

pub const HTTP_TIMEOUT: Duration = Duration::from_secs(300);
pub const DB_TIMEOUT: Duration = Duration::from_secs(300);
/// Largest data file a node will read.
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Largest stdout a child process may produce.
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
/// Largest combined text input a node may consume, including separators.
pub const MAX_INPUT_BYTES: usize = 64 * 1024 * 1024;
/// Environment variables child processes inherit. Nothing else leaks in.
pub const INHERITED_ENV: &[&str] = &["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "TERM"];
const POLL: Duration = Duration::from_millis(25);
/// How deep a directory node descends.
const MAX_DIRECTORY_DEPTH: usize = 32;
/// The error a cancelled run ends with.
pub const CANCELLED: &str = "the run was cancelled";
/// For callers that have no cancel flag.
static NEVER: AtomicBool = AtomicBool::new(false);
/// The on-disk cache is pruned, oldest first, past this many bytes.
pub const MAX_CACHE_BYTES: u64 = 1024 * 1024 * 1024;
/// Most files one bundled write-file answer may create.
pub const MAX_BUNDLE_FILES: usize = 500;
/// Outputs over this size are not copied into the cache for the run history; the record
/// keeps a preview of them instead.
pub const MAX_HISTORY_BYTES: usize = 8 * 1024 * 1024;

/// Where the executor looks for files and writes outputs. `root` is the working
/// directory, which directory nodes read and write-file nodes write under.
#[derive(Clone, Debug)]
pub struct Dirs {
    pub root: PathBuf,
    pub data: PathBuf,
    pub scripts: PathBuf,
    pub agents: PathBuf,
    /// Where a persistent [`Cache`] keeps its entries.
    pub cache: PathBuf,
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
            cache: root.join(library::CACHE_DIR),
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
    /// Prepared immutable child graphs, including unstarted calls, by relative path.
    pub workflows: BTreeMap<String, GraphDoc>,
    /// Published child ports, keyed by local Workflow node id then output name.
    pub named_outputs: BTreeMap<String, BTreeMap<String, Output>>,
    /// Child runs with the exact graph snapshots used, keyed by local Workflow node id.
    pub calls: BTreeMap<String, history::WorkflowRun>,
    /// Resolved run-form values, including defaults, for replay and history.
    pub inputs: workflow::Values,
    pub log: Vec<String>,
    pub result: Result<Output, String>,
    pub elapsed: Duration,
    /// The nodes served from the cache instead of running, by document id.
    pub reused: Vec<String>,
    /// The output of every node that ran or was found in the cache, by document id, so
    /// the UI can show upstream outputs and not only the target's.
    pub outputs: HashMap<String, Output>,
    /// The node whose own step failed when `result` is an error: the deepest one, not the
    /// nodes that failed only because it did.
    pub failed: Option<String>,
}

impl Report {
    /// Resolve a local node output or a published port produced by a Workflow node.
    pub fn output_handle(&self, handle: &str) -> Option<&Output> {
        match handle.split_once('.') {
            Some((id, name)) => self.named_outputs.get(id)?.get(name),
            None => self.outputs.get(handle),
        }
    }

    fn failure(error: String, elapsed: Duration) -> Self {
        Self {
            workflows: BTreeMap::new(),
            named_outputs: BTreeMap::new(),
            calls: BTreeMap::new(),
            inputs: workflow::Values::new(),
            log: Vec::new(),
            result: Err(error),
            elapsed,
            reused: Vec::new(),
            outputs: HashMap::new(),
            failed: None,
        }
    }
}

/// Something that happened during a run, delivered as it happens.
#[derive(Clone, Debug, PartialEq)]
pub enum Progress {
    /// A prepared immutable child snapshot, published before execution starts.
    Workflow { id: String, graph: Arc<GraphDoc> },
    /// A log line; the same line ends up in the [`Report`].
    Log(String),
    /// A piece of text an agent streamed back. `id` is the node producing it.
    Text { id: String, text: String },
    /// A node changed state, so a canvas can show the run moving.
    Node { id: String, state: NodeState },
}

/// Where a node is in the run. A node that is never reached (an upstream failure, a
/// cancel) gets no state at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeState {
    /// Its inputs are ready and its own step has started.
    Running,
    /// Its own step finished with output.
    Done,
    /// Its output came from the cache; it never ran.
    Reused,
    /// Its own step failed (not a cancel).
    Failed,
}

/// A content hash: the key an agent's or script's output is cached under, and what a
/// node's output contributes to the keys of the nodes after it.
pub type Key = u128;

/// Outputs from earlier runs. In memory they are kept by node id and reused only when the
/// key, built from the node's configuration and everything upstream, still matches. A
/// persistent cache also keeps every entry on disk by key alone, so another graph or a
/// later session asking the same question of the same inputs gets the same answer back.
#[derive(Debug, Default)]
pub struct Cache {
    entries: HashMap<String, Entry>,
    /// Where entries also live on disk.
    dir: Option<PathBuf>,
    /// Entries on disk, counted when the cache opens and kept current after.
    on_disk: usize,
}

#[derive(Clone, Debug)]
struct Entry {
    key: Key,
    output: Output,
}

/// The sidecar of an on-disk entry; the bytes are in the `.bin` next to it.
#[derive(Serialize, Deserialize)]
struct Meta {
    label: String,
    text: bool,
    saved: Option<PathBuf>,
    /// When it was stored, for people reading the directory.
    stored: String,
    /// Detect incomplete or mixed writes before any cached bytes reach a downstream node.
    bytes: usize,
    checksum: String,
}

#[allow(clippy::len_without_is_empty)]
impl Cache {
    /// A cache that also keeps its entries under `dir`, created on first store.
    pub fn persistent(dir: PathBuf) -> Self {
        let on_disk = cache_directory(&dir, false)
            .ok()
            .and_then(|dir| std::fs::read_dir(dir).ok())
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| {
                        e.file_type().is_ok_and(|t| t.is_file())
                            && e.path().extension().is_some_and(|x| x == "json")
                    })
                    .count()
            })
            .unwrap_or(0);
        Self {
            entries: HashMap::new(),
            dir: Some(dir),
            on_disk,
        }
    }

    /// How many outputs are kept: on disk for a persistent cache, else in memory.
    pub fn len(&self) -> usize {
        match &self.dir {
            Some(_) => self.on_disk,
            None => self.entries.len(),
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        if let Some(dir) = &self.dir
            && let Ok(dir) = cache_directory(dir, false)
            && let Ok(entries) = std::fs::read_dir(dir)
        {
            for entry in entries.flatten() {
                let path = entry.path();
                if path
                    .extension()
                    .is_some_and(|x| x == "json" || x == "bin" || x == "tmp")
                {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
        self.on_disk = 0;
    }

    fn lookup(&mut self, id: &str, key: Key) -> Option<Output> {
        if let Some(output) = self
            .entries
            .get(id)
            .filter(|e| e.key == key)
            .map(|e| e.output.clone())
        {
            return Some(output);
        }
        let output = self.read(key)?;
        self.entries.insert(
            id.to_string(),
            Entry {
                key,
                output: output.clone(),
            },
        );
        Some(output)
    }

    fn store(&mut self, id: &str, key: Key, output: Output) {
        if let Err(e) = self.write(key, &output) {
            // A full or read-only disk costs persistence, not the run.
            eprintln!("cache: {e}");
        }
        self.entries.insert(id.to_string(), Entry { key, output });
    }

    /// Keep an output under its [`content_key`] so the run history can show it later.
    /// Only a persistent cache keeps anything, and nothing over [`MAX_HISTORY_BYTES`].
    /// An entry already there is touched so pruning treats it as recent. Returns whether
    /// the bytes are on disk.
    pub fn keep(&mut self, key: Key, output: &Output) -> bool {
        let Some((meta_path, bin_path)) = self.paths(key) else {
            return false;
        };
        if output.bytes.len() > MAX_HISTORY_BYTES {
            return false;
        }
        if self.read(key).is_some() {
            let now = std::time::SystemTime::now();
            for path in [&meta_path, &bin_path] {
                if let Ok(file) = std::fs::File::options().append(true).open(path) {
                    let _ = file.set_modified(now);
                }
            }
            return true;
        }
        match self.write(key, output) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("cache: {e}");
                false
            }
        }
    }

    /// The output kept under a key, when its files are still there.
    pub fn fetch(&self, key: Key) -> Option<Output> {
        self.read(key)
    }

    fn paths(&self, key: Key) -> Option<(PathBuf, PathBuf)> {
        let dir = self.dir.as_ref()?;
        Some((
            dir.join(format!("{key:032x}.json")),
            dir.join(format!("{key:032x}.bin")),
        ))
    }

    fn read(&self, key: Key) -> Option<Output> {
        cache_directory(self.dir.as_ref()?, false).ok()?;
        let (meta_path, bin_path) = self.paths(key)?;
        let meta: Meta =
            serde_json::from_slice(&storage::read(&meta_path, 64 * 1024).ok()?).ok()?;
        if meta.bytes > MAX_OUTPUT_BYTES {
            return None;
        }
        let bytes = storage::read(&bin_path, meta.bytes as u64).ok()?;
        if bytes.len() != meta.bytes || meta.checksum != format!("{:032x}", key_of(&[&bytes])) {
            return None;
        }
        Some(Output {
            text: meta.text && std::str::from_utf8(&bytes).is_ok(),
            bytes,
            label: meta.label,
            saved: meta.saved,
        })
    }

    fn write(&mut self, key: Key, output: &Output) -> Result<(), String> {
        let Some((meta_path, bin_path)) = self.paths(key) else {
            return Ok(());
        };
        storage::check_size(output.bytes.len(), MAX_OUTPUT_BYTES as u64)?;
        let dir = cache_directory(self.dir.as_deref().unwrap_or(Path::new("")), true)?;
        let fresh = !meta_path.exists();
        write_atomically(&bin_path, &output.bytes)?;
        let meta = Meta {
            label: output.label.clone(),
            text: output.text,
            saved: output.saved.clone(),
            stored: library::timestamp(),
            bytes: output.bytes.len(),
            checksum: format!("{:032x}", key_of(&[&output.bytes])),
        };
        let json = serde_json::to_vec_pretty(&meta).map_err(|e| e.to_string())?;
        write_atomically(&meta_path, &json)?;
        if fresh {
            self.on_disk += 1;
        }
        self.prune(&dir);
        Ok(())
    }

    /// Drop the oldest entries while the directory is over [`MAX_CACHE_BYTES`].
    fn prune(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        // (modified, key stem, size) per `.bin`.
        let mut bins: Vec<(std::time::SystemTime, PathBuf, u64)> = entries
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                (path.extension()? == "bin").then_some(())?;
                let meta = e.metadata().ok()?;
                if !meta.is_file() {
                    return None;
                }
                Some((
                    meta.modified().unwrap_or(std::time::UNIX_EPOCH),
                    path,
                    meta.len(),
                ))
            })
            .collect();
        let mut total = bins.iter().fold(0u64, |sum, b| sum.saturating_add(b.2));
        if total <= MAX_CACHE_BYTES {
            return;
        }
        bins.sort_by_key(|b| b.0);
        for (_, path, size) in bins {
            if total <= MAX_CACHE_BYTES {
                break;
            }
            let _ = std::fs::remove_file(path.with_extension("json"));
            let _ = std::fs::remove_file(&path);
            total = total.saturating_sub(size);
            self.on_disk = self.on_disk.saturating_sub(1);
        }
        // Memory entries whose files went away are still correct outputs; keep them.
    }
}

fn cache_directory(dir: &Path, create: bool) -> Result<PathBuf, String> {
    let parent = dir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = dir.file_name().ok_or("cache directory has no name")?;
    storage::directory(parent, Path::new(name), create)
}

/// Write through a temporary file and rename, so a reader never sees half a file.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), String> {
    storage::write_atomic(path, bytes)
}

/// What the caller holds while a run is in flight. The default caches nothing, is never
/// cancelled and reports nothing.
#[derive(Clone, Default)]
pub struct Control {
    pub inputs: workflow::Values,
    pub review: Option<review::Handler>,
    /// Outputs of earlier runs. `None` runs every node.
    pub cache: Option<Arc<Mutex<Cache>>>,
    /// Run every node even when the cache has its output; the cache is still updated.
    pub fresh: bool,
    /// Set to true to stop the run at its next check.
    pub cancel: Arc<AtomicBool>,
    /// Receives progress from the thread running the graph.
    pub progress: Option<Arc<dyn Fn(Progress) + Send + Sync>>,
}

impl Control {
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// A node that has run (or been found in the cache) and a hash of its output, which the
/// nodes after it put into their own keys.
#[derive(Clone, Debug)]
struct Done {
    output: Arc<Output>,
    key: Key,
}

pub(crate) fn lock(cache: &Mutex<Cache>) -> std::sync::MutexGuard<'_, Cache> {
    cache.lock().unwrap_or_else(|e| e.into_inner())
}

/// The content key of a node's output: what it contributes to the keys of the nodes after
/// it, and what the run history keeps the output under in the cache.
pub fn content_key(node_type: NodeType, output: &Output) -> Key {
    key_of(&[node_type.label().as_bytes(), &output.bytes])
}

/// One key over several byte strings, with lengths mixed in so the boundaries count.
/// Two FNV-1a passes, one forwards and one backwards, give a stable 128-bit key that
/// does not depend on the standard library's hasher.
pub(crate) fn key_of(parts: &[&[u8]]) -> Key {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let fnv = |mut h: u64, bytes: &mut dyn Iterator<Item = u8>| {
        for byte in bytes {
            h = (h ^ u64::from(byte)).wrapping_mul(PRIME);
        }
        h
    };
    let forwards = parts.iter().flat_map(|p| {
        (p.len() as u64)
            .to_le_bytes()
            .into_iter()
            .chain(p.iter().copied())
    });
    let backwards = parts.iter().rev().flat_map(|p| {
        p.iter()
            .rev()
            .copied()
            .chain((p.len() as u64).to_le_bytes())
    });
    let a = fnv(OFFSET, &mut { forwards });
    let b = fnv(OFFSET ^ 0x9e37_79b9_7f4a_7c15, &mut { backwards });
    (Key::from(a) << 64) | Key::from(b)
}

/// Looks up an environment variable. The real one is [`process_env`]; tests inject keys.
pub type Env<'a> = &'a (dyn Fn(&str) -> Option<String> + Sync);

pub fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Run node `id` (a document id such as `n3`) and everything wired into it.
pub fn run_with(
    doc: &GraphDoc,
    id: &str,
    dirs: Dirs,
    http: &dyn Http,
    env: Env,
    control: &Control,
) -> Report {
    run_all_with(doc, &[id.to_string()], dirs, http, env, control)
}

/// Run several targets in order, each after everything wired into it, in one pass over
/// the graph: a node feeding two targets runs once, and a node that failed is not tried
/// again for the next target. A failed target does not stop the others; a cancel does.
/// The result is the last target's output, or the first error.
pub fn run_all_with(
    doc: &GraphDoc,
    targets: &[String],
    dirs: Dirs,
    http: &dyn Http,
    env: Env,
    control: &Control,
) -> Report {
    let started = Instant::now();
    let plans = calls::prepare(doc, targets, &dirs.root, &control.cancel);
    publish_workflows(&plans, "", control);
    let mut report = run_prepared(
        doc,
        targets,
        dirs,
        http,
        env,
        control,
        Scope {
            plans: &plans,
            namespace: String::new(),
            key: None,
        },
    );
    report.elapsed = started.elapsed();
    report
}

fn publish_workflows(plans: &calls::Plans, namespace: &str, control: &Control) {
    let Some(progress) = &control.progress else {
        return;
    };
    for (id, prepared) in plans {
        if control.cancelled() {
            return;
        }
        let Ok(prepared) = prepared else { continue };
        let id = format!("{namespace}{id}");
        progress(Progress::Workflow {
            id: id.clone(),
            graph: prepared.graph.clone(),
        });
        publish_workflows(&prepared.calls, &format!("{id}/"), control);
    }
}

fn workflow_snapshots(plans: &calls::Plans) -> BTreeMap<String, GraphDoc> {
    let mut snapshots = BTreeMap::new();
    let mut pending = vec![(String::new(), plans)];
    while let Some((namespace, plans)) = pending.pop() {
        for (id, prepared) in plans {
            let Ok(prepared) = prepared else { continue };
            let path = format!("{namespace}{id}");
            snapshots.insert(path.clone(), prepared.graph.as_ref().clone());
            pending.push((format!("{path}/"), &prepared.calls));
        }
    }
    snapshots
}

struct Scope<'a> {
    plans: &'a calls::Plans,
    namespace: String,
    key: Option<Key>,
}

fn run_prepared(
    doc: &GraphDoc,
    targets: &[String],
    dirs: Dirs,
    http: &dyn Http,
    env: Env,
    control: &Control,
    scope: Scope<'_>,
) -> Report {
    let start = Instant::now();
    let workflows = workflow_snapshots(scope.plans);
    let resolved = if control.cancelled() {
        Err(CANCELLED.into())
    } else {
        workflow::fields(doc, targets)
            .and_then(|fields| workflow::resolve(&fields, &control.inputs, &dirs.data))
    };
    let inputs = match resolved {
        Ok(inputs) => inputs,
        Err(error) => {
            let mut report = Report::failure(error, start.elapsed());
            report.workflows = workflows;
            return report;
        }
    };
    let mut runner = Runner {
        named_outputs: BTreeMap::new(),
        scope,
        calls: BTreeMap::new(),
        inputs,
        graph: GraphIndex::new(doc),
        dirs,
        http,
        env,
        control,
        log: Vec::new(),
        done: HashMap::new(),
        errors: HashMap::new(),
        reused: Vec::new(),
        failed: None,
    };
    let all = targets.len() != 1;
    if all {
        runner.log(format!("run all: {}", targets.join(", ")));
    }
    let mut result: Result<Arc<Output>, String> =
        Err("nothing to run: no node has content".to_string());
    let mut first_error: Option<String> = None;
    for target in targets {
        match runner.run_node(target) {
            Ok(done) => result = Ok(done.output),
            Err(e) => {
                let cancelled = e == CANCELLED;
                if cancelled {
                    first_error = Some(e);
                    break;
                }
                first_error.get_or_insert(e);
            }
        }
    }
    if let Some(e) = first_error {
        result = Err(e);
    }
    if all {
        let finished = targets
            .iter()
            .filter(|t| runner.done.contains_key(*t))
            .count();
        runner.log(format!("run all: {finished} of {} finished", targets.len()));
    }
    Report {
        workflows,
        named_outputs: runner.named_outputs,
        calls: runner.calls,
        inputs: runner.inputs,
        log: runner.log,
        result: result.map(Arc::unwrap_or_clone),
        elapsed: start.elapsed(),
        reused: runner.reused,
        outputs: runner
            .done
            .into_iter()
            .map(|(id, done)| (id, Arc::unwrap_or_clone(done.output)))
            .collect(),
        failed: runner.failed,
    }
}

struct Runner<'a> {
    named_outputs: BTreeMap<String, BTreeMap<String, Output>>,
    scope: Scope<'a>,
    calls: BTreeMap<String, history::WorkflowRun>,
    inputs: workflow::Values,
    graph: GraphIndex<'a>,
    dirs: Dirs,
    http: &'a dyn Http,
    env: Env<'a>,
    control: &'a Control,
    log: Vec<String>,
    /// Nodes already run this time, so a node feeding two others runs once.
    done: HashMap<String, Done>,
    /// Nodes that failed this time (or could not run because an input did), so a later
    /// target does not try them again.
    errors: HashMap<String, String>,
    /// Nodes served from the cache.
    reused: Vec<String>,
    /// The first node whose own step failed.
    failed: Option<String>,
}

/// Index once so walking a deep graph does not rescan every node and edge at each step.
struct GraphIndex<'a> {
    nodes: HashMap<&'a str, &'a NodeDoc>,
    upstream: HashMap<&'a str, Vec<&'a str>>,
    wires: HashMap<&'a str, Vec<&'a crate::doc::EdgeDoc>>,
}

impl<'a> GraphIndex<'a> {
    fn new(doc: &'a GraphDoc) -> Self {
        let mut nodes = HashMap::new();
        for node in &doc.nodes {
            nodes.entry(node.id.as_str()).or_insert(node);
        }
        let mut upstream: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut wires: HashMap<&str, Vec<&crate::doc::EdgeDoc>> = HashMap::new();
        for edge in &doc.edges {
            upstream.entry(&edge.to).or_default().push(&edge.from);
            wires.entry(&edge.to).or_default().push(edge);
        }
        for inputs in upstream.values_mut() {
            inputs.sort_unstable();
            inputs.dedup();
        }
        for edges in wires.values_mut() {
            edges.sort_by_key(|edge| edge.handle());
            edges.dedup();
        }
        Self {
            nodes,
            upstream,
            wires,
        }
    }

    fn upstream(&self, id: &str) -> &[&'a str] {
        self.upstream.get(id).map(Vec::as_slice).unwrap_or_default()
    }

    fn cover_upstream(&self, id: &'a str, covered: &mut HashSet<&'a str>) {
        let mut pending = vec![id];
        while let Some(id) = pending.pop() {
            if covered.insert(id) {
                pending.extend(self.upstream(id));
            }
        }
    }

    fn order(
        &self,
        target: &str,
        mut validate: impl FnMut(&str) -> Result<bool, String>,
    ) -> Result<Vec<String>, (String, String)> {
        let mut stack = vec![(target.to_string(), false)];
        let mut active = HashSet::new();
        let mut visited = HashSet::new();
        let mut order = Vec::new();
        while let Some((id, exiting)) = stack.pop() {
            if exiting {
                active.remove(&id);
                visited.insert(id.clone());
                order.push(id);
                continue;
            }
            if visited.contains(&id) {
                continue;
            }
            let skip = validate(&id).map_err(|e| (id.clone(), e))?;
            if skip {
                visited.insert(id.clone());
                order.push(id);
                continue;
            }
            if !active.insert(id.clone()) {
                return Err((id.clone(), format!("{id} is wired in a cycle")));
            }
            stack.push((id.clone(), true));
            for from in self.upstream(&id).iter().rev() {
                stack.push(((*from).to_string(), false));
            }
        }
        Ok(order)
    }
}

fn validate_content(node: &NodeDoc) -> Result<(), String> {
    let id = &node.id;
    let data = node
        .data
        .as_ref()
        .ok_or_else(|| format!("{id} is blank; give it content first"))?;
    let issues = data.validate();
    if let Some(first) = issues.first() {
        return Err(format!(
            "{id} is not valid: {}: {}",
            first.field, first.message
        ));
    }
    Ok(())
}

/// Read-only validation for a saved child, using execution's iterative dependency walk.
pub(crate) fn preflight(doc: &GraphDoc, target: &str, cancel: &AtomicBool) -> Result<(), String> {
    let mut ids = HashSet::new();
    for node in &doc.nodes {
        if !validate::is_node_id(&node.id) || !ids.insert(&node.id) {
            return Err(format!(
                "invalid or duplicate workflow node id: {}",
                node.id
            ));
        }
    }
    let graph = GraphIndex::new(doc);
    graph
        .order(target, |id| {
            if cancel.load(Ordering::Relaxed) {
                return Err(CANCELLED.into());
            }
            let node = graph
                .nodes
                .get(id)
                .ok_or_else(|| format!("{id} is not in the graph"))?;
            validate_content(node)?;
            Ok(false)
        })
        .map(|_| ())
        .map_err(|(_, e)| e)
}

fn validate_wire(
    edge: &crate::doc::EdgeDoc,
    graph: &GraphIndex<'_>,
    plans: &calls::Plans,
) -> Result<(), String> {
    let source = graph
        .nodes
        .get(edge.from.as_str())
        .ok_or_else(|| format!("{} is not in the graph", edge.from))?;
    let config = match source.data.as_ref() {
        Some(NodeData::Workflow(config)) if config.output.is_empty() => config,
        _ => {
            return if edge.output.is_some() {
                Err(format!(
                    "{} has no named outputs; select a Workflow using published outputs",
                    edge.from
                ))
            } else {
                Ok(())
            };
        }
    };
    let prepared = plans
        .get(&edge.from)
        .ok_or_else(|| format!("{}: workflow was not prepared", edge.from))?
        .as_ref()
        .map_err(Clone::clone)?;
    match edge.output.as_deref() {
        Some(name) => {
            validate::output_name(name).map_err(|error| format!("{}: {error}", edge.handle()))?;
            if !prepared.graph.outputs.contains_key(name) {
                return Err(format!(
                    "{} does not publish {name}; choose {}",
                    config.graph,
                    prepared
                        .graph
                        .outputs
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        None if prepared.graph.outputs.len() > 1 => {
            return Err(format!(
                "{} publishes multiple outputs; choose {}",
                edge.from,
                prepared
                    .graph
                    .outputs
                    .keys()
                    .map(|name| format!("{}.{name}", edge.from))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        None => {}
    }
    Ok(())
}

/// Follow statically binary outputs without reading files or invoking providers.
/// File chains are iterative; recursion only follows the bounded prepared call tree.
fn known_binary(graph: &GraphIndex<'_>, plans: &calls::Plans, handle: &str) -> bool {
    let mut handle = handle.to_string();
    let mut seen = HashSet::new();
    while seen.insert(handle.clone()) {
        let (id, port) = handle
            .split_once('.')
            .map_or((handle.as_str(), None), |(id, port)| (id, Some(port)));
        let Some(node) = graph.nodes.get(id) else {
            return false;
        };
        match node.data.as_ref() {
            Some(NodeData::Data(config)) => return !config.format.is_text(),
            Some(NodeData::Agent(config)) => return config.output == OutputType::Image,
            Some(NodeData::File(config)) if !config.bundle => {
                let Some(edges) = graph.wires.get(id) else {
                    return false;
                };
                if let [edge] = edges.as_slice() {
                    handle = edge.handle();
                } else {
                    return false;
                }
            }
            Some(NodeData::Workflow(config)) => {
                let Some(Ok(prepared)) = plans.get(id) else {
                    return false;
                };
                let target = if config.output.is_empty() {
                    match port {
                        Some(name) => prepared.graph.outputs.get(name),
                        None if prepared.graph.outputs.len() == 1 => {
                            prepared.graph.outputs.values().next()
                        }
                        None => None,
                    }
                } else {
                    Some(&config.output)
                };
                return target.is_some_and(|target| {
                    known_binary(&GraphIndex::new(&prepared.graph), &prepared.calls, target)
                });
            }
            _ => return false,
        }
    }
    false
}

fn validate_binary_bindings(
    id: &str,
    graph: &GraphIndex<'_>,
    plans: &calls::Plans,
) -> Result<(), String> {
    if let Some(NodeData::Workflow(config)) =
        graph.nodes.get(id).and_then(|node| node.data.as_ref())
    {
        for (name, binding) in &config.inputs {
            if let calls::Binding::Output { node } = binding
                && known_binary(graph, plans, node)
            {
                return Err(format!(
                    "{id}: {name}: {node} produces binary data; workflow inputs require text or a data/ file path"
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn preflight_wires(
    doc: &GraphDoc,
    targets: &[String],
    plans: &calls::Plans,
) -> Result<(), String> {
    let graph = GraphIndex::new(doc);
    for target in targets {
        graph
            .order(target, |id| {
                validate_binary_bindings(id, &graph, plans)?;
                for edge in graph.wires.get(id).into_iter().flatten() {
                    validate_wire(edge, &graph, plans)?;
                }
                Ok(false)
            })
            .map_err(|(_, error)| error)?;
    }
    Ok(())
}

pub(crate) fn preflight_publications(doc: &GraphDoc, plans: &calls::Plans) -> Result<(), String> {
    let graph = GraphIndex::new(doc);
    for (name, handle) in &doc.outputs {
        let (from, output) = handle
            .split_once('.')
            .map_or((handle.as_str(), None), |(id, port)| {
                (id, Some(port.to_string()))
            });
        let edge = crate::doc::EdgeDoc {
            from: from.into(),
            to: String::new(),
            output,
        };
        validate_wire(&edge, &graph, plans)
            .map_err(|error| format!("published output {name}: {error}"))?;
    }
    Ok(())
}

impl Runner<'_> {
    fn log(&mut self, line: impl Into<String>) {
        let line = line.into();
        if let Some(progress) = &self.control.progress {
            progress(Progress::Log(line.clone()));
        }
        self.log.push(line);
    }

    fn check_cancel(&self) -> Result<(), String> {
        if self.control.cancelled() {
            Err(CANCELLED.into())
        } else {
            Ok(())
        }
    }

    fn state(&self, id: &str, state: NodeState) {
        if let Some(progress) = &self.control.progress {
            progress(Progress::Node {
                id: id.to_string(),
                state,
            });
        }
    }

    fn node(&self, id: &str) -> Result<&NodeDoc, String> {
        self.graph
            .nodes
            .get(id)
            .copied()
            .ok_or_else(|| format!("{id} is not in the graph"))
    }

    fn secret(&self, name: &str) -> Result<String, String> {
        (self.env)(name).ok_or_else(|| format!("environment variable {name} is not set"))
    }

    fn run_node(&mut self, id: &str) -> Result<Done, String> {
        self.check_cancel()?;
        if let Some(done) = self.done.get(id) {
            return Ok(done.clone());
        }
        if let Some(e) = self.errors.get(id) {
            return Err(e.clone());
        }
        let order = match self.execution_order(id) {
            Ok(order) => order,
            Err((from, e)) => {
                self.record_failure(&from, &e);
                self.errors.insert(id.to_string(), e.clone());
                return Err(e);
            }
        };
        for step in order {
            self.check_cancel()?;
            if self.done.contains_key(&step) {
                continue;
            }
            if let Some(e) = self.errors.get(&step).cloned() {
                self.errors.insert(id.to_string(), e.clone());
                return Err(e);
            }
            match self.run_fresh(&step) {
                Ok(done) => {
                    self.done.insert(step, done);
                }
                Err(e) => {
                    self.record_failure(&step, &e);
                    self.errors.insert(id.to_string(), e.clone());
                    return Err(e);
                }
            }
        }
        self.done
            .get(id)
            .cloned()
            .ok_or_else(|| format!("{id} did not run"))
    }

    fn record_failure(&mut self, id: &str, error: &str) {
        if error != CANCELLED && !self.errors.contains_key(id) {
            self.failed.get_or_insert_with(|| id.to_string());
            self.state(id, NodeState::Failed);
        }
        self.errors.insert(id.to_string(), error.to_string());
    }

    /// Preflight the target's entire dependency graph before performing any step. An
    /// explicit stack avoids overflowing the thread stack on long chains. The exit
    /// marker produces the same stable upstream-first order as recursive traversal.
    fn execution_order(&self, target: &str) -> Result<Vec<String>, (String, String)> {
        self.graph.order(target, |id| {
            self.check_cancel()?;
            if let Some(error) = self.errors.get(id) {
                return Err(error.clone());
            }
            if self.done.contains_key(id) {
                return Ok(true);
            }
            self.validate_node(id)?;
            Ok(false)
        })
    }

    /// A node's name as a placeholder may use it: set and valid, else none.
    fn input_name(&self, id: &str) -> Option<String> {
        let (base, port) = id
            .split_once('.')
            .map_or((id, None), |(base, port)| (base, Some(port)));
        let name = self.node(base).ok()?.name.as_deref()?.trim();
        validate::node_name(name)
            .ok()
            .map(|()| port.map_or_else(|| name.to_string(), |port| format!("{name}.{port}")))
    }

    fn validate_node(&self, id: &str) -> Result<(), String> {
        let node = self.node(id)?;
        validate_content(node)?;
        validate_binary_bindings(id, &self.graph, self.scope.plans)?;
        for edge in self.graph.wires.get(id).into_iter().flatten() {
            validate_wire(edge, &self.graph, self.scope.plans)?;
        }
        if matches!(node.data, Some(NodeData::Workflow(_))) {
            self.scope
                .plans
                .get(id)
                .ok_or_else(|| format!("{id}: workflow was not prepared"))?
                .as_ref()
                .map_err(|e| format!("{id}: {e}"))?;
        }
        Ok(())
    }

    fn run_fresh(&mut self, id: &str) -> Result<Done, String> {
        let node = self.node(id)?.clone();
        let data = node.data.as_ref().ok_or_else(|| format!("{id} is blank"))?;
        let upstream: Vec<String> = self
            .graph
            .wires
            .get(id)
            .into_iter()
            .flatten()
            .map(|edge| edge.handle())
            .collect();
        let mut inputs = Vec::with_capacity(upstream.len());
        for from in &upstream {
            let (base, port) = from
                .split_once('.')
                .map_or((from.as_str(), None), |(base, port)| (base, Some(port)));
            let mut done = self
                .done
                .get(base)
                .cloned()
                .ok_or_else(|| format!("{from} did not run"))?;
            if let Some(port) = port {
                let output = self
                    .named_outputs
                    .get(base)
                    .and_then(|ports| ports.get(port))
                    .ok_or_else(|| format!("{from} did not publish an output"))?;
                done.key = key_of(&[
                    b"workflow-port-v1",
                    &done.key.to_le_bytes(),
                    port.as_bytes(),
                    &output.bytes,
                    &[u8::from(output.text)],
                ]);
                done.output = Arc::new(output.clone());
            }
            inputs.push((from.clone(), done));
        }
        self.log(format!(
            "{id}: {} ({}) with {} input{}",
            data.node_type().label().to_lowercase(),
            data.summary(),
            upstream.len(),
            if upstream.len() == 1 { "" } else { "s" }
        ));
        let name = node
            .kind
            .clone()
            .unwrap_or_else(|| format!("{}{id}", self.scope.namespace.replace('/', "-")));

        // Agents and scripts are the slow, costly nodes and depend only on their config,
        // their inputs and (for scripts) the file itself, so those make the key. The rest
        // read or change live state and always run.
        let config_json = serde_json::to_vec(data).unwrap_or_default();
        let upstream_keys: Vec<Vec<u8>> = inputs
            .iter()
            .map(|(_, done)| done.key.to_le_bytes().to_vec())
            .collect();
        let cache_key = match data {
            NodeData::Agent(config) => {
                // Templates can address ids as well as names. Include the binding map
                // whenever a template is used, so rewiring equal bytes cannot reuse an
                // answer to a different (or now invalid) prompt. Plain graphs keep
                // their existing cache keys.
                let names: Vec<String> = upstream
                    .iter()
                    .map(|from| self.input_name(from).unwrap_or_default())
                    .collect();
                let has_templates = !validate::placeholders(&config.prompt).is_empty()
                    || !validate::placeholders(&config.message).is_empty();
                let extra = if has_templates {
                    serde_json::to_vec(&("input-bindings-v1", &upstream, &names))
                        .map_err(|e| e.to_string())?
                } else if names.iter().all(String::is_empty) {
                    Vec::new()
                } else {
                    names.join("\n").into_bytes()
                };
                Some(cache_key(&config_json, &extra, &upstream_keys))
            }
            NodeData::Script(config) => {
                let path = validate::resolve_inside(&self.dirs.scripts, &config.path)?;
                let source = read_bounded(&path, MAX_FILE_BYTES)?;
                Some(cache_key(&config_json, &source, &upstream_keys))
            }
            NodeData::Data(_)
            | NodeData::Workflow(_)
            | NodeData::Input(_)
            | NodeData::Database(_)
            | NodeData::Directory(_)
            | NodeData::File(_) => None,
        };
        let cache_key = cache_key.map(|key| match self.scope.key {
            Some(scope) => key_of(&[
                b"workflow-cache-v1",
                &scope.to_le_bytes(),
                &key.to_le_bytes(),
            ]),
            None => key,
        });
        let cache_id = format!("{}{id}", self.scope.namespace);
        if let Some(key) = cache_key
            && !self.control.fresh
            && let Some(cache) = &self.control.cache
            && let Some(output) = lock(cache).lookup(&cache_id, key)
        {
            self.log(format!("{id}: reused the cached {}", output.label));
            self.reused.push(id.to_string());
            self.state(id, NodeState::Reused);
            return Ok(Done {
                key: content_key(data.node_type(), &output),
                output: Arc::new(output),
            });
        }

        self.check_cancel()?;
        self.state(id, NodeState::Running);
        if let NodeData::Workflow(config) = data {
            let start = Instant::now();
            let result = self.run_workflow(id, config, &inputs);
            // A bad dynamic mapping can fail before the child runner starts. Retain
            // its snapshot and reason too, with no claim that any child node ran.
            if let Err(error) = &result
                && !self.calls.contains_key(id)
                && let Some(Ok(prepared)) = self.scope.plans.get(id)
            {
                let mut report = Report::failure(error.clone(), start.elapsed());
                report.workflows = workflow_snapshots(&prepared.calls);
                self.calls.insert(
                    id.to_string(),
                    history::WorkflowRun {
                        graph: prepared.graph.as_ref().clone(),
                        run: history::record(
                            &prepared.graph,
                            &prepared.targets,
                            &report,
                            self.control.cache.as_deref(),
                        ),
                    },
                );
            }
            let done = result?;
            self.state(id, NodeState::Done);
            return Ok(done);
        }
        let outputs: Vec<Arc<Output>> = inputs.into_iter().map(|(_, done)| done.output).collect();
        let output = match data {
            NodeData::Workflow(_) => unreachable!("workflow handled above"),
            NodeData::Input(config) => {
                let field = node.name.as_deref().unwrap_or_default().trim();
                let value = self
                    .inputs
                    .get(field)
                    .ok_or_else(|| format!("{id}: missing run input {field}"))?;
                Ok(
                    if config.kind == workflow::InputType::File && !value.is_empty() {
                        let path = validate::resolve_inside(&self.dirs.data, value)?;
                        Output::new(
                            read_bounded(&path, MAX_FILE_BYTES)?,
                            true,
                            &format!("Input {field}: {value}"),
                        )
                    } else {
                        Output::new(value.as_bytes().to_vec(), true, &format!("Input {field}"))
                    },
                )
            }
            NodeData::Data(config) => self.run_data(config),
            NodeData::Script(config) => {
                self.run_script(config, &join_text(id, &upstream, &outputs)?)
            }
            NodeData::Database(config) => {
                self.run_database(config, &join_text(id, &upstream, &outputs)?)
            }
            NodeData::Agent(config) => {
                let inputs: Vec<Input> = upstream
                    .iter()
                    .zip(outputs)
                    .map(|(from, output)| Input {
                        id: from.clone(),
                        name: self.input_name(from),
                        output,
                    })
                    .collect();
                let filled = fill_agent(id, config, &inputs)?;
                if let Some(note) = &filled.note {
                    self.log(format!("{id}: {note}"));
                }
                self.run_agent(&filled.config, id, &name, &filled.input)
            }
            NodeData::Directory(config) => self.run_directory(config),
            NodeData::File(config) if config.bundle => {
                self.run_bundle(id, config, &join_text(id, &upstream, &outputs)?)
            }
            NodeData::File(config) => match outputs.as_slice() {
                // A single binary input (an image from an agent) is written as it is.
                [only] if !only.text => self.run_file(id, config, only.bytes.clone(), false),
                _ => self.run_file(
                    id,
                    config,
                    join_text(id, &upstream, &outputs)?.into_bytes(),
                    true,
                ),
            },
        }?;
        if let (Some(key), Some(cache)) = (cache_key, &self.control.cache) {
            lock(cache).store(&cache_id, key, output.clone());
        }
        self.state(id, NodeState::Done);
        let key = if let NodeData::Input(_) = data {
            let name = node.name.as_deref().unwrap_or_default().trim();
            let value = self
                .inputs
                .get(name)
                .map(String::as_str)
                .unwrap_or_default();
            key_of(&[
                b"run-input-v1",
                &config_json,
                name.as_bytes(),
                value.as_bytes(),
                &output.bytes,
            ])
        } else {
            content_key(data.node_type(), &output)
        };
        Ok(Done {
            key,
            output: Arc::new(output),
        })
    }

    fn run_workflow(
        &mut self,
        id: &str,
        config: &calls::Config,
        inputs: &[(String, Done)],
    ) -> Result<Done, String> {
        let prepared = self
            .scope
            .plans
            .get(id)
            .ok_or("workflow was not prepared")?
            .as_ref()
            .map_err(Clone::clone)?
            .clone();
        let mut supplied = workflow::Values::new();
        let mut bytes = 0usize;
        for (name, binding) in &config.inputs {
            let value = match binding {
                calls::Binding::Value { value } => value.as_str(),
                calls::Binding::Output { node } => {
                    let output = &inputs
                        .iter()
                        .find(|(from, _)| from == node)
                        .ok_or_else(|| format!("{name}: {node} is not wired into {id}"))?
                        .1
                        .output;
                    output.as_text().ok_or_else(|| format!("{name}: {node} produced binary data; workflow inputs require text or a data/ file path"))?
                }
            };
            // Check before copying large upstream outputs into run values.
            validate::text(name, value, workflow::MAX_VALUE_BYTES)?;
            bytes = bytes.saturating_add(value.len());
            if bytes > workflow::MAX_VALUES_BYTES {
                return Err("workflow inputs exceed the 1 MB combined limit".into());
            }
            supplied.insert(name.clone(), value.to_string());
        }
        let values = workflow::resolve(&prepared.fields, &supplied, &self.dirs.data)?;
        let encoded = serde_json::to_vec(&values).map_err(|e| e.to_string())?;
        let upstream: Vec<Key> = inputs.iter().map(|(_, done)| done.key).collect();
        let binding = serde_json::to_vec(&(config, upstream)).map_err(|e| e.to_string())?;
        let scope_key = key_of(&[
            b"workflow-run-v1",
            &prepared.key.to_le_bytes(),
            &self.scope.key.unwrap_or_default().to_le_bytes(),
            &binding,
            &encoded,
        ]);
        let mut control = self.control.clone();
        control.inputs = values;
        if let Some(progress) = control.progress.clone() {
            let prefix = id.to_string();
            control.progress = Some(Arc::new(move |event| {
                progress(match event {
                    Progress::Log(line) => Progress::Log(format!("{prefix}/{line}")),
                    Progress::Node { id, state } => Progress::Node {
                        id: format!("{prefix}/{id}"),
                        state,
                    },
                    Progress::Text { id, text } => Progress::Text {
                        id: format!("{prefix}/{id}"),
                        text,
                    },
                    Progress::Workflow { id, graph } => Progress::Workflow {
                        id: format!("{prefix}/{id}"),
                        graph,
                    },
                })
            }));
        }
        if let Some(review) = control.review.clone() {
            let prefix = id.to_string();
            control.review = Some(Arc::new(move |request| {
                review(Arc::new(review::Request {
                    node: format!("{prefix}/{}", request.node),
                    files: request.files.clone(),
                }))
            }));
        }
        let targets = &prepared.targets;
        let report = run_prepared(
            &prepared.graph,
            targets,
            self.dirs.clone(),
            self.http,
            self.env,
            &control,
            Scope {
                plans: &prepared.calls,
                namespace: format!("{}{id}/", self.scope.namespace),
                key: Some(scope_key),
            },
        );
        let run = history::record(&prepared.graph, targets, &report, control.cache.as_deref());
        self.calls.insert(
            id.to_string(),
            history::WorkflowRun {
                graph: prepared.graph.as_ref().clone(),
                run,
            },
        );
        let mut ports = BTreeMap::new();
        if config.output.is_empty() && report.result.is_ok() {
            for (name, target) in &prepared.graph.outputs {
                let value = report
                    .output_handle(target)
                    .ok_or_else(|| format!("published output {name}: {target} did not run"))?;
                ports.insert(name.clone(), value.clone());
            }
        }
        self.log
            .extend(report.log.into_iter().map(|line| format!("{id}/{line}")));
        self.check_cancel()?;
        let mut output = report.result.map_err(|e| {
            if e == CANCELLED {
                e
            } else {
                format!("{id} ({}): {e}", config.graph)
            }
        })?;
        if config.output.is_empty() {
            output = if ports.len() == 1 {
                ports
                    .values()
                    .next()
                    .cloned()
                    .ok_or("workflow has no outputs")?
            } else {
                let manifest = ports
                    .iter()
                    .map(|(name, value)| format!("{name}: {}", value.label))
                    .collect::<Vec<_>>()
                    .join("\n");
                Output::new(manifest.into_bytes(), true, "Workflow outputs")
            };
            self.named_outputs.insert(id.to_string(), ports);
        }
        Ok(Done {
            key: key_of(&[
                b"workflow-output-v1",
                &scope_key.to_le_bytes(),
                &output.bytes,
            ]),
            output: Arc::new(output),
        })
    }

    // ----- files -----

    /// Write the wire's text (or a lone binary input) to the node's file, creating the
    /// directories on the way. The path is checked again here and resolved after symlinks.
    fn run_file(
        &mut self,
        id: &str,
        config: &FileConfig,
        bytes: Vec<u8>,
        textual: bool,
    ) -> Result<Output, String> {
        let path = validate::resolve_writable_inside(&self.dirs.root, &config.path)?;
        let bytes = if textual && config.unfence {
            let mut text = validate::unfenced(&String::from_utf8_lossy(&bytes));
            if text.is_empty() {
                return Err("nothing to write: the text arriving on the wire is empty".into());
            }
            text.push('\n');
            text.into_bytes()
        } else if bytes.is_empty() {
            return Err("nothing to write: the input is empty".into());
        } else {
            bytes
        };
        let exists = path.exists();
        if exists && config.mode == WriteMode::Create {
            return Err(format!(
                "{} exists; set the mode to Overwrite or Append to change it",
                config.path
            ));
        }
        self.review_writes(id, config, &[(&config.path, &path, &bytes)])?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        write_file_bytes(&path, &bytes, config.mode)?;
        self.log(format!(
            "{} {} ({})",
            match (config.mode, exists) {
                (WriteMode::Append, true) => "appended to",
                (_, true) => "replaced",
                _ => "created",
            },
            config.path,
            size(bytes.len())
        ));
        let mut output = Output::new(bytes, textual, &format!("Wrote {}", config.path));
        output.saved = Some(path);
        Ok(output)
    }

    /// Write every `=== path ===` section of the wire's text to its own file under the
    /// node's directory: the format a directory node emits, so an agent asked to answer
    /// in it can change or add several files at once. Every path is checked and resolved
    /// before anything is written; in Create mode one existing file stops the whole
    /// write. The output lists what was written.
    fn run_bundle(&mut self, id: &str, config: &FileConfig, text: &str) -> Result<Output, String> {
        let base = validate::directory_path(&config.path)?;
        let sections = split_sections(text);
        if sections.is_empty() {
            return Err(
                "no `=== path ===` sections in the input; ask the agent to answer with one \
                 per file, or turn the sections switch off to write one file"
                    .into(),
            );
        }
        if sections.len() > MAX_BUNDLE_FILES {
            return Err(format!(
                "{} sections, over the {MAX_BUNDLE_FILES} files one answer may write",
                sections.len()
            ));
        }
        let mut plan: Vec<(String, PathBuf, Vec<u8>, bool)> = Vec::new();
        for (rel, content) in sections {
            let joined = base.join(validate::relative_path(&rel)?);
            let shown = joined.to_string_lossy().replace('\\', "/");
            if plan.iter().any(|(p, ..)| *p == shown) {
                return Err(format!("{shown} appears twice in the answer"));
            }
            // The whole path from the working directory is judged, not only the section:
            // `=== scripts/x.sh ===` under `out/` is `out/scripts/x.sh` and harmless, while
            // a bundle rooted at `.` may not plant an app directory.
            let real = validate::resolve_writable_inside(&self.dirs.root, &shown)?;
            if let Some((other, ..)) = plan
                .iter()
                .find(|(_, path, ..)| real.starts_with(path) || path.starts_with(&real))
            {
                return Err(format!(
                    "{shown} conflicts with {other} after resolving paths; nothing was written"
                ));
            }
            let exists = real.exists();
            if exists && config.mode == WriteMode::Create {
                return Err(format!(
                    "{shown} exists; nothing was written. Set the mode to Overwrite or Append"
                ));
            }
            let mut bytes = if config.unfence {
                let mut text = validate::unfenced(&content);
                if !text.is_empty() {
                    text.push('\n');
                }
                text.into_bytes()
            } else {
                content.into_bytes()
            };
            if bytes.is_empty() {
                bytes.clear();
            }
            plan.push((shown, real, bytes, exists));
        }
        let files: Vec<_> = plan
            .iter()
            .map(|(shown, real, bytes, _)| (shown.as_str(), real.as_path(), bytes.as_slice()))
            .collect();
        self.review_writes(id, config, &files)?;
        let mut summary = format!(
            "Wrote {} file{} under {}/\n",
            plan.len(),
            if plan.len() == 1 { "" } else { "s" },
            config.display_path()
        );
        let mut total = 0;
        for (shown, real, bytes, exists) in &plan {
            if let Some(parent) = real.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("create {}: {e}", parent.display()))?;
            }
            write_file_bytes(real, bytes, config.mode)?;
            let verb = match (config.mode, exists) {
                (WriteMode::Append, true) => "appended to",
                (_, true) => "replaced",
                _ => "created",
            };
            self.log(format!("{verb} {shown} ({})", size(bytes.len())));
            summary.push_str(&format!("  {shown} ({}) {verb}\n", size(bytes.len())));
            total += bytes.len();
        }
        Ok(Output {
            bytes: summary.into_bytes(),
            text: true,
            label: format!(
                "Wrote {} file{} · {}",
                plan.len(),
                if plan.len() == 1 { "" } else { "s" },
                size(total)
            ),
            saved: validate::resolve_directory_inside(&self.dirs.root, &config.path).ok(),
        })
    }

    // ----- directories -----

    fn review_writes(
        &mut self,
        id: &str,
        config: &FileConfig,
        files: &[(&str, &Path, &[u8])],
    ) -> Result<(), String> {
        self.check_cancel()?;
        if !config.review {
            return Ok(());
        }
        let handler = self.control.review.clone().ok_or(
            "this write-file node requires review; run it in the app to approve the changes",
        )?;
        let batch = review::Batch::prepare(&self.dirs.root, id, files, config.mode)?;
        self.log(format!(
            "{id}: waiting for review of {} file(s)",
            files.len()
        ));
        let decision = handler(batch.request.clone())?;
        self.check_cancel()?;
        if decision == review::Decision::Reject {
            return Err(review::REJECTED.into());
        }
        batch.verify(&self.dirs.root)?;
        self.check_cancel()?;
        self.log(format!("{id}: approved {} file(s)", files.len()));
        Ok(())
    }

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
        let run = run_process(
            &program,
            &args,
            &cwd,
            &[],
            input.as_bytes(),
            timeout,
            &self.control.cancel,
        )?;
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
        validate::sql_for_engine(&query, config.read_only, config.engine)?;
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
                let db = database_file(&self.dirs, config)?;
                sqlite_plan(config, &db, file.as_deref())?
            }
            DbEngine::DuckDb => {
                let db = database_file(&self.dirs, config)?;
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
            &self.control.cancel,
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

    /// One provider call. Generative replies stream in: each piece goes to the progress
    /// sink and the cancel flag is checked between pieces.
    fn run_agent(
        &mut self,
        config: &AgentConfig,
        id: &str,
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
        let progress = self.control.progress.clone();
        let cancel = self.control.cancel.clone();
        let mut on_text = |text: &str| {
            if let Some(progress) = &progress {
                progress(Progress::Text {
                    id: id.to_string(),
                    text: text.to_string(),
                });
            }
            !cancel.load(Ordering::Relaxed)
        };
        let streamed = match providers::generate_streaming(
            self.http,
            config,
            key.as_deref(),
            input,
            &mut on_text,
        ) {
            Ok(streamed) => streamed,
            Err(e) => {
                self.check_cancel()?;
                return Err(e);
            }
        };
        // Whole replies, embeddings and images may never call on_text. Cancellation
        // while their request was in flight still prevents persistence and cache reuse.
        self.check_cancel()?;
        if let Some(answered) = substituted_model(config, streamed.model.as_deref()) {
            self.log(format!(
                "⚠ answered by {answered}, not {}: the server substituted a model",
                config.model.trim()
            ));
        }
        let bytes = streamed.bytes;
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

/// The model that answered when it is not the one asked for. Servers name snapshots
/// (`gpt-4o-2024-08-06` for `gpt-4o`) and tags (`llama3.1:latest` for `llama3.1`), so a
/// longer name with the requested one as prefix is the same model; a different name
/// (LM Studio answering an unknown model id with whatever is loaded) is reported.
fn substituted_model(config: &AgentConfig, answered: Option<&str>) -> Option<String> {
    let answered = answered?.trim();
    let requested = config.model.trim();
    let requested = requested.strip_prefix("models/").unwrap_or(requested);
    let answered_bare = answered.strip_prefix("models/").unwrap_or(answered);
    (!answered_bare.starts_with(requested)).then(|| answered.to_string())
}

/// The `=== path ===` sections of a text, in order: each path with the lines up to the
/// next header. Text before the first header (an agent's preamble) is dropped. A fenced
/// answer is unwrapped first, since models like to fence the whole thing.
fn split_sections(text: &str) -> Vec<(String, String)> {
    let text = validate::unfenced(text);
    let mut sections: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        let header = line
            .trim()
            .strip_prefix("===")
            .and_then(|rest| rest.strip_suffix("==="))
            .map(str::trim)
            .filter(|path| !path.is_empty());
        match header {
            Some(path) => sections.push((path.to_string(), String::new())),
            None => {
                if let Some((_, content)) = sections.last_mut() {
                    content.push_str(line);
                    content.push('\n');
                }
            }
        }
    }
    sections
}

/// Let the open operation enforce Create/Append even if another writer creates the
/// file after preflight. In particular, Create must never truncate an existing file.
fn write_file_bytes(path: &Path, bytes: &[u8], mode: WriteMode) -> Result<(), String> {
    let mut options = std::fs::OpenOptions::new();
    match mode {
        WriteMode::Create => {
            options.write(true).create_new(true);
        }
        WriteMode::Overwrite => {
            options.write(true).create(true).truncate(true);
        }
        WriteMode::Append => {
            options.append(true).create(true);
        }
    }
    let mut file = options
        .open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    file.write_all(bytes)
        .map_err(|e| format!("write {}: {e}", path.display()))
}

/// Start at every content sink, plus representatives of cyclic components that have
/// no sink. Those representatives let Run all report cycles instead of omitting them.
pub fn sinks(doc: &GraphDoc) -> Vec<String> {
    let graph = GraphIndex::new(doc);
    let content: HashSet<&str> = doc
        .nodes
        .iter()
        .filter(|n| n.data.is_some())
        .map(|n| n.id.as_str())
        .collect();
    let mut next: HashMap<&str, &str> = HashMap::new();
    for edge in &doc.edges {
        if content.contains(edge.from.as_str()) && content.contains(edge.to.as_str()) {
            let to = next.entry(&edge.from).or_insert(&edge.to);
            *to = (*to).min(&edge.to);
        }
    }
    let mut sinks: Vec<&str> = content
        .iter()
        .copied()
        .filter(|id| !next.contains_key(id))
        .collect();
    let mut covered = HashSet::new();
    for &sink in &sinks {
        graph.cover_upstream(sink, &mut covered);
    }
    let mut remaining: Vec<&str> = content.into_iter().collect();
    remaining.sort_by_key(|id| id_order(id));
    for mut id in remaining {
        if covered.contains(id) {
            continue;
        }
        // Every uncovered node has a successor; follow it to a cycle, then cover
        // everything feeding that cycle without turning its ancestors into targets.
        let mut path = HashSet::new();
        while path.insert(id) {
            id = next[id];
        }
        sinks.push(id);
        graph.cover_upstream(id, &mut covered);
    }
    sinks.sort_by_key(|id| id_order(id));
    sinks.dedup();
    sinks.into_iter().map(str::to_string).collect()
}

/// Node ids in numeric order (`n2` before `n10`); ids of another shape after, by text.
fn id_order(id: &str) -> (u8, u128, &str) {
    match id
        .strip_prefix('n')
        .filter(|_| validate::is_node_id(id))
        .and_then(|digits| digits.parse::<u128>().ok())
    {
        Some(n) => (0, n, id),
        None => (1, 0, id),
    }
}

/// An upstream output as an agent's templates see it.
struct Input {
    id: String,
    /// The node's name, when it has a valid one.
    name: Option<String>,
    output: Arc<Output>,
}

/// An agent's prompt and message with their placeholders filled.
struct Filled {
    /// The config with the filled prompt in place of the template.
    config: AgentConfig,
    /// The user message: the filled message, the inputs nothing named, or both.
    input: String,
    /// What went where, for the log, once any placeholder was filled.
    note: Option<String>,
}

/// Fill the `{{name}}` placeholders of an agent's prompt and message from its inputs. A
/// placeholder names an input by the node's name, else by its id. The inputs nothing
/// names are joined with blank lines: on their own they are the message, as for every
/// other node; after a message with text they follow it. Binary output has no place in
/// text.
fn fill_agent(id: &str, config: &AgentConfig, inputs: &[Input]) -> Result<Filled, String> {
    let mut used: Vec<usize> = Vec::new();
    let mut lookup = |hole: &str| -> Result<&str, String> {
        let by_name: Vec<usize> = inputs
            .iter()
            .enumerate()
            .filter(|(_, input)| input.name.as_deref() == Some(hole))
            .map(|(n, _)| n)
            .collect();
        let index = match by_name.as_slice() {
            [one] => *one,
            [] => inputs
                .iter()
                .position(|input| input.id == hole)
                .ok_or_else(|| {
                    let available: Vec<&str> = inputs
                        .iter()
                        .map(|input| input.name.as_deref().unwrap_or(&input.id))
                        .collect();
                    if available.is_empty() {
                        format!("{id}: {{{{{hole}}}}} names no input: nothing is wired into {id}")
                    } else {
                        format!(
                            "{id}: {{{{{hole}}}}} names no input; the inputs are {}",
                            available.join(", ")
                        )
                    }
                })?,
            many => {
                let ids: Vec<&str> = many.iter().map(|&n| inputs[n].id.as_str()).collect();
                return Err(format!(
                    "{id}: {{{{{hole}}}}} is ambiguous: {} share the name {hole}",
                    ids.join(" and ")
                ));
            }
        };
        let input = &inputs[index];
        let text = input.output.as_text().ok_or_else(|| {
            format!(
                "{} produced binary output ({}), which {id} can't place in text",
                input.id, input.output.label
            )
        })?;
        if !used.contains(&index) {
            used.push(index);
        }
        Ok(text)
    };
    let limit = context_bytes(config);
    let over_context = |e: String| {
        format!(
            "{id}: {e}; prompt and input are over the context size of {}",
            config.context_size
        )
    };
    // Preserve lookup diagnostics; only size errors need the context explanation.
    let render_error = |e| match e {
        validate::RenderError::Limit(limit) => {
            over_context(format!("text exceeds the {limit}-byte limit"))
        }
        validate::RenderError::Lookup(error) => error,
    };
    let prompt = validate::render(&config.prompt, limit, &mut lookup).map_err(render_error)?;
    let remaining = limit - prompt.len();
    let message =
        validate::render(&config.message, remaining, &mut lookup).map_err(render_error)?;

    let mut rest: Vec<&str> = Vec::with_capacity(inputs.len());
    for (n, input) in inputs.iter().enumerate() {
        if used.contains(&n) {
            continue;
        }
        rest.push(input.output.as_text().ok_or_else(|| {
            format!(
                "{} produced binary output ({}), which {id} can't take as text",
                input.id, input.output.label
            )
        })?);
    }
    let message = if message.trim().is_empty() {
        String::new()
    } else {
        message
    };
    let rest = join_bounded(&rest, remaining - message.len()).map_err(over_context)?;
    let input = if message.trim().is_empty() {
        rest
    } else if rest.is_empty() {
        message
    } else {
        join_bounded(&[&message, &rest], remaining).map_err(over_context)?
    };
    let note = (!used.is_empty()).then(|| {
        let parts: Vec<String> = inputs
            .iter()
            .enumerate()
            .map(|(n, input)| {
                if used.contains(&n) {
                    format!(
                        "{{{{{}}}}} from {}",
                        input.name.as_deref().unwrap_or(&input.id),
                        input.id
                    )
                } else {
                    format!("{} appended", input.id)
                }
            })
            .collect();
        format!("placed inputs: {}", parts.join(" · "))
    });
    Ok(Filled {
        config: AgentConfig {
            prompt,
            ..config.clone()
        },
        input,
        note,
    })
}

/// The upstream outputs as one text, blank-line separated. Binary output is an error:
/// only a write-file node can take it, and only alone.
fn join_text(id: &str, upstream: &[String], outputs: &[Arc<Output>]) -> Result<String, String> {
    let mut texts = Vec::with_capacity(outputs.len());
    for (from, output) in upstream.iter().zip(outputs) {
        match output.as_text() {
            Some(text) => texts.push(text),
            None => {
                return Err(format!(
                    "{from} produced binary output ({}), which {id} can't take as text",
                    output.label
                ));
            }
        }
    }
    join_bounded(&texts, MAX_INPUT_BYTES).map_err(|e| format!("{id}: combined input {e}"))
}

/// Count the complete join before allocating, including blank lines between empty inputs.
fn join_bounded(texts: &[&str], limit: usize) -> Result<String, String> {
    let mut len = texts.len().saturating_sub(1).saturating_mul(2);
    for text in texts {
        len = len.saturating_add(text.len());
        if len > limit {
            return Err(format!("text exceeds the {limit}-byte limit"));
        }
    }
    Ok(texts.join("\n\n"))
}

/// The cache key of an agent or script: its configuration, one extra (the script's
/// source; nothing for an agent, whose name only decides where the output is saved) and
/// its inputs' keys.
fn cache_key(config_json: &[u8], extra: &[u8], upstream_keys: &[Vec<u8>]) -> Key {
    let mut parts: Vec<&[u8]> = vec![config_json, extra];
    parts.extend(upstream_keys.iter().map(Vec::as_slice));
    key_of(&parts)
}

/// The file of a file-based database node. A read-only node needs an existing file; a
/// writable one may name a new file in an existing directory, which the engine creates.
fn database_file(dirs: &Dirs, config: &DatabaseConfig) -> Result<PathBuf, String> {
    if config.read_only {
        return validate::resolve_inside(&dirs.data, &config.path);
    }
    let rel = validate::relative_path(&config.path)?;
    let path = validate::resolve_new_inside(&dirs.data, &rel)?;
    match path.parent() {
        Some(parent) if parent.is_dir() => Ok(path),
        Some(parent) => Err(format!(
            "{} does not exist; create the directory first",
            parent.display()
        )),
        None => Err(format!("{} has no parent directory", path.display())),
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

/// The byte budget used by the approximate four-bytes-per-token context check.
fn context_bytes(config: &AgentConfig) -> usize {
    (config.context_size as usize).saturating_mul(4)
}

/// Refuse to send a prompt and input that can't fit the agent's estimated context window.
fn fits_context(config: &AgentConfig, input: &str) -> Result<(), String> {
    let estimate = config.prompt.len().saturating_add(input.len()).div_ceil(4);
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
            let db = database_file(dirs, config)?;
            if !db.exists() {
                return Ok("(new database, no tables yet)".to_string());
            }
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
        &NEVER,
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
                .map(|header| format!(" with text columns: {header}"))
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
    storage::read(path, max)
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
/// and collecting both output streams, killing it when `timeout` passes or `cancel` is
/// set (which ends in [`CANCELLED`]).
fn run_process(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &[(String, String)],
    stdin: &[u8],
    timeout: Duration,
    cancel: &AtomicBool,
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
    let mut cancelled;
    let mut exited = None;
    let status = loop {
        if exited.is_none() {
            match child.try_wait() {
                Ok(status) => exited = status,
                Err(e) => {
                    kill_group(child.id());
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("waiting for `{program}`: {e}"));
                }
            }
        }
        cancelled = cancel.load(Ordering::Relaxed);
        if cancelled || start.elapsed() >= timeout {
            timed_out = !cancelled;
            kill_group(child.id());
            let _ = child.kill();
            break child
                .wait()
                .map_err(|e| format!("waiting for `{program}`: {e}"))?;
        }
        // A finished parent does not imply EOF: grandchildren may still own its pipe
        // ends. Keep the deadline and cancellation active until all I/O is finished.
        if let Some(status) = exited
            && writer.is_finished()
            && stdout.is_finished()
            && stderr.is_finished()
        {
            break status;
        }
        thread::sleep(POLL);
    };
    if cancelled {
        return Err(CANCELLED.into());
    }
    let _ = writer.join();
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    if stdout.overflowed || stderr.overflowed {
        return Err(format!(
            "`{program}` produced more than {} of output",
            size(MAX_OUTPUT_BYTES)
        ));
    }
    Ok(Finished {
        status,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
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

/// What a child wrote on one stream, and whether it went past the cap.
#[derive(Default)]
struct Capped {
    bytes: Vec<u8>,
    /// More than [`MAX_OUTPUT_BYTES`] arrived; only a prefix was kept.
    overflowed: bool,
}

/// Read a stream until it ends, keeping at most [`MAX_OUTPUT_BYTES`] and noticing when
/// more arrives. Reading stops once the cap is crossed: a child that runs away is not
/// worth the I/O, and the caller closes the pipe, which ends its next write with SIGPIPE.
fn read_capped(mut reader: impl Read) -> Capped {
    let mut out = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    let mut overflowed = false;
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = MAX_OUTPUT_BYTES.saturating_sub(out.len());
                if n > room {
                    out.extend_from_slice(&buf[..room]);
                    overflowed = true;
                    break;
                }
                out.extend_from_slice(&buf[..n]);
            }
        }
    }
    Capped {
        bytes: out,
        overflowed,
    }
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
/// which stays writable on a read-only connection and vanishes with the process. The table
/// is created from the file's header with TEXT columns before the import, so every value
/// arrives as the file spelled it on every sqlite3 version: the shell's own table (TEXT
/// columns up to 3.53, ANY from 3.54) would otherwise make `007` a 7 on one machine and a
/// `"007"` on another.
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
        let table = config.input_table.trim();
        let columns = csv_header(file)?
            .iter()
            .map(|name| format!("{} TEXT", quote_identifier(name)))
            .collect::<Vec<_>>()
            .join(", ");
        stdin.push_str(&format!(
            "CREATE TABLE temp.{}({columns});\n.import --csv --skip 1 --schema temp \"{}\" {table}\n",
            quote_identifier(table),
            plain_path(file)?,
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

/// The column names in a CSV file's header line (RFC 4180: comma-separated, a quoted field
/// may hold commas and doubled quotes), with a byte-order mark dropped. An empty name
/// becomes `column_<n>` so a trailing comma still imports, and two names that differ only in
/// case are refused here, where the message can name them, rather than by the shell.
fn csv_header(path: &Path) -> Result<Vec<String>, String> {
    let line = first_line(path).ok_or_else(|| {
        format!(
            "{} has no header line to name the input table's columns",
            path.display()
        )
    })?;
    let line = line.strip_prefix('\u{feff}').unwrap_or(&line);
    let mut names = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted => {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            }
            '"' if field.is_empty() => quoted = true,
            ',' if !quoted => names.push(std::mem::take(&mut field)),
            _ => field.push(c),
        }
    }
    names.push(field);
    let mut seen = HashSet::new();
    let mut columns = Vec::with_capacity(names.len());
    for (i, name) in names.into_iter().enumerate() {
        let name = name.trim();
        let name = if name.is_empty() {
            format!("column_{}", i + 1)
        } else {
            name.to_string()
        };
        if !seen.insert(name.to_lowercase()) {
            return Err(format!(
                "{} names column {name} twice; SQLite column names are case-insensitive",
                path.display()
            ));
        }
        columns.push(name);
    }
    Ok(columns)
}

/// A name as a double-quoted SQL identifier.
fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
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
            name: None,
            color: None,
            data,
        }
    }

    fn graph(nodes: Vec<NodeDoc>, edges: &[(&str, &str)]) -> GraphDoc {
        GraphDoc {
            outputs: BTreeMap::new(),
            version: 1,
            name: "t".into(),
            view: View::default(),
            nodes,
            edges: edges
                .iter()
                .map(|(from, to)| EdgeDoc {
                    output: None,
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

    /// [`run_with`] under the default control: no cache, no cancel, no progress.
    fn run(doc: &GraphDoc, id: &str, dirs: Dirs, http: &dyn Http, env: Env) -> Report {
        run_with(doc, id, dirs, http, env, &Control::default())
    }

    /// A control that records progress and caches, with its cancel flag to hand.
    fn recording_control() -> (Control, Arc<Mutex<Vec<Progress>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let control = Control {
            cache: Some(Arc::new(Mutex::new(Cache::default()))),
            fresh: false,
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Some(Arc::new(move |p| sink.lock().unwrap().push(p))),
            ..Control::default()
        };
        (control, seen)
    }

    fn sse(pieces: &[&str]) -> String {
        let mut body = String::new();
        for piece in pieces {
            body.push_str(&format!(
                "data: {}\n\n",
                serde_json::json!({"choices": [{"delta": {"content": piece}}]})
            ));
        }
        body.push_str("data: [DONE]\n");
        body
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
        // Every node's output is in the report, not only the target's.
        let mut ids: Vec<&String> = report.outputs.keys().collect();
        ids.sort();
        assert_eq!(ids, ["n0", "n1", "n2"]);
        assert_eq!(report.outputs["n1"].as_text(), Some("note"));
        assert_eq!(report.failed, None);
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

        let report = run(&doc, "n3", Dirs::under(&root), &http, &no_env);
        let err = report.result.unwrap_err();
        assert!(err.contains("code 3") && err.contains("boom"), "{err}");
        assert_eq!(report.failed.as_deref(), Some("n3"));

        // A failure upstream is attributed to the node that failed, and the outputs made
        // before it are still reported.
        let doc = graph(
            vec![
                node("n0", data("in.txt", DataFormat::Text)),
                node("n1", shell("fail.sh", &[], 10)),
                node("n2", shell("echo.sh", &[], 10)),
            ],
            &[("n0", "n1"), ("n1", "n2")],
        );
        let report = run(&doc, "n2", Dirs::under(&root), &http, &no_env);
        assert!(report.result.is_err());
        assert_eq!(report.failed.as_deref(), Some("n1"));
        let mut ids: Vec<&String> = report.outputs.keys().collect();
        ids.sort();
        assert_eq!(ids, ["n0"]);
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
    }

    #[test]
    fn child_processes_see_only_the_inherited_variables() {
        // A secret in this process's environment, named exactly like one agents and
        // databases read. The child must not see it: `env_clear()` plus INHERITED_ENV is
        // the whole of the isolation, so it is asserted from both sides.
        const SECRET: &str = "UAPZ_TEST_SECRET_TOKEN";
        // SAFETY: this test owns the variable for its duration and reads it back itself.
        unsafe { std::env::set_var(SECRET, "s3cr3t") };
        let root = scratch("env");
        // The names come in as arguments, so no `${...}` has to survive Rust's formatting.
        fs::write(
            root.join("scripts/env.sh"),
            concat!(
                "for name in \"$@\"; do\n",
                "  value=$(printenv \"$name\" || true)\n",
                "  if [ -n \"$value\" ]; then echo \"$name=$value\"; else echo \"$name=unset\"; fi\n",
                "done\n",
                "if [ -n \"$PATH\" ]; then echo PATH=set; else echo PATH=unset; fi\n",
            ),
        )
        .unwrap();
        let http = FakeHttp::new(200, "{}");
        let names = [SECRET, "DATABASE_URL", "ANTHROPIC_API_KEY"];
        let doc = graph(vec![node("n0", shell("env.sh", &names, 10))], &[]);
        let out = run(&doc, "n0", Dirs::under(&root), &http, &no_env)
            .result
            .expect("env");
        let text = out.as_text().unwrap();
        for absent in names {
            assert!(
                text.contains(&format!("{absent}=unset")),
                "{absent} leaked to the child: {text}"
            );
            assert!(!text.contains("s3cr3t"), "{text}");
        }
        // What a script does need is there.
        assert!(text.contains("PATH=set"), "{text}");
        // SAFETY: the same test removes the variable it set.
        unsafe { std::env::remove_var(SECRET) };
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn database_children_get_their_secret_and_nothing_else() {
        // The one variable a database node is given is the connection URL; a key that
        // belongs to an agent must not ride along.
        const URI: &str = "UAPZ_TEST_PG_URL";
        const KEY: &str = "UAPZ_TEST_AGENT_KEY";
        let env = |name: &str| match name {
            URI => Some("postgres://localhost/db".to_string()),
            KEY => Some("sk-should-not-travel".to_string()),
            _ => None,
        };
        let config = DatabaseConfig {
            engine: DbEngine::Postgres,
            url_env: URI.into(),
            query: "SELECT 1".into(),
            ..DatabaseConfig::default()
        };
        let plan = postgres_plan(&config, &env_secret(&env, &config.url_env).unwrap()).unwrap();
        assert!(plan.env.contains(&("PGDATABASE".into(), "db".into())));
        // Everything in `env` reaches the child through `run_process`; nothing outside it
        // does, because the command starts from an empty environment.
        assert!(!plan.env.iter().any(|(k, _)| k == KEY));
        assert!(!plan.env.iter().any(|(_, v)| v.contains("sk-")));
    }

    #[test]
    fn read_stops_at_the_cap_and_says_so() {
        // A reader with more than the cap to give: only the cap is kept and nothing past
        // the chunk that crossed it is read.
        struct Endless {
            /// How much the reader handed over in total.
            handed: usize,
            /// What it has left to hand over.
            left: usize,
        }
        impl Read for Endless {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = self.left.min(buf.len());
                buf[..n].fill(b'x');
                self.left -= n;
                self.handed += n;
                Ok(n)
            }
        }
        // Twice the cap, so a run that drains the reader is unmistakable.
        let mut endless = Endless {
            handed: 0,
            left: MAX_OUTPUT_BYTES * 2,
        };
        let read = read_capped(&mut endless);
        assert!(read.overflowed, "the overflow is reported");
        assert_eq!(
            read.bytes.len(),
            MAX_OUTPUT_BYTES,
            "exactly the cap is kept"
        );
        assert!(
            endless.handed <= MAX_OUTPUT_BYTES + 64 * 1024,
            "reading stopped at the cap, handed {}",
            endless.handed
        );
        assert!(
            endless.left > 0,
            "the stream was not drained: {} bytes left",
            endless.left
        );

        // A short stream is passed through untouched, without claiming an overflow.
        let short = read_capped(std::io::Cursor::new(b"hello"));
        assert!(!short.overflowed);
        assert_eq!(short.bytes, b"hello");
    }

    #[test]
    fn script_runtime_selection() {
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
        let root = scratch("sqlite-plan");
        let rows = root.join("data/rows.csv");
        fs::write(
            &rows,
            "\u{feff}id,\"full name\",\"say \"\"hi\"\"\",\n1,Ann,x,y\n",
        )
        .unwrap();
        let plan = sqlite_plan(&sqlite, Path::new("/d/x.db"), Some(&rows)).unwrap();
        assert!(!plan.args.contains(&"-readonly".to_string()));
        assert_eq!(
            plan.stdin,
            format!(
                "CREATE TABLE temp.\"rows\"(\"id\" TEXT, \"full name\" TEXT, \"say \"\"hi\"\"\" TEXT, \"column_4\" TEXT);\n\
                 .import --csv --skip 1 --schema temp \"{}\" rows\nSELECT * FROM rows;\n",
                rows.display()
            )
        );

        fs::write(&rows, "a,A\n1,2\n").unwrap();
        let err = sqlite_plan(&sqlite, Path::new("/d/x.db"), Some(&rows)).unwrap_err();
        assert!(err.contains("names column A twice"), "{err}");
        fs::write(&rows, "").unwrap();
        let err = sqlite_plan(&sqlite, Path::new("/d/x.db"), Some(&rows)).unwrap_err();
        assert!(err.contains("no header line"), "{err}");
        fs::remove_dir_all(&root).unwrap();
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

    #[test]
    fn sql_client_commands_are_refused_before_the_process_starts() {
        require("sqlite3");
        let root = scratch("metacommands");
        let http = FakeHttp::new(200, "{}");
        // What every attack tries to create. Each client runs the query from stdin, so a
        // query carrying a meta-command becomes a shell command run as this user; an
        // absolute path keeps the proof out of the working directory.
        let planted = root.join("planted");
        let planted = plain_path(&planted).unwrap();
        let plant = format!("touch {planted}");
        assert!(
            Command::new("sqlite3")
                .arg(root.join("data/t.sqlite"))
                .arg("CREATE TABLE t(a INTEGER);")
                .status()
                .unwrap()
                .success()
        );
        let db = |id: &str, query: &str, read_only: bool| {
            node(
                id,
                Some(NodeData::Database(DatabaseConfig {
                    path: "t.sqlite".into(),
                    query: query.into(),
                    read_only,
                    ..DatabaseConfig::default()
                })),
            )
        };
        let doc = graph(
            vec![
                db("n0", &format!("SELECT 1\n.shell {plant}\n"), true),
                db("n1", &format!("SELECT 1;\n.shell {plant}\n;\n"), true),
                db("n2", &format!("SELECT 1 \\! {plant}"), true),
                db("n3", &format!("SELECT 1\n\\! {plant}\n"), true),
                db("n4", &format!("SELECT 1;\nsystem {plant}\n"), false),
                db("n5", "SELECT 1", true),
                node("n6", data("wired.txt", DataFormat::Text)),
                db("n7", "", true),
                db("n8", "SELECT 1 \\q", true),
                db("n9", &format!("SELECT '\\';\n.shell {plant}\n-- '"), true),
                db("n10", &format!("SELECT '\\';\n.shell {plant}\n-- '"), false),
                db("n11", &format!("VACUUM INTO '{planted}'"), false),
            ],
            &[("n6", "n7")],
        );
        // SQL arriving on the wire is validated by `run_database`, not by any config.
        fs::write(
            root.join("data/wired.txt"),
            format!("SELECT 1\n.shell {plant}\n"),
        )
        .unwrap();
        let go = |id: &str| run(&doc, id, Dirs::under(&root), &http, &no_env);

        for id in ["n0", "n1", "n2", "n3", "n4", "n8", "n9", "n10", "n11"] {
            let report = go(id);
            let err = report.result.unwrap_err();
            assert!(
                err.contains("not valid: query") || err.contains("not allowed"),
                "{id}: {err}"
            );
            // The proof that nothing reached a shell: no client was started at all.
            assert!(
                !report.log.iter().any(|l| l.starts_with("$ sqlite3")),
                "{id}: a client ran: {:?}",
                report.log
            );
        }
        // The wire's SQL is refused by the executor's own check, not by a config check.
        let report = go("n7");
        let err = report.result.unwrap_err();
        assert!(err.contains('.'), "{err}");
        assert!(!report.log.iter().any(|l| l.starts_with("$ sqlite3")));
        fs::write(
            root.join("data/wired.txt"),
            format!("SELECT '\\';\n.shell {plant}\n-- '"),
        )
        .unwrap();
        let report = go("n7");
        assert!(report.result.unwrap_err().contains("ambiguous"));
        assert!(!report.log.iter().any(|l| l.starts_with("$ sqlite3")));
        // The plain query still runs, so the check did not simply break the node.
        let out = go("n5").result.expect("plain select still runs");
        assert!(out.text);
        assert_eq!(out.as_text().unwrap().trim(), r#"[{"1":1}]"#);
        // `query_statement` appends `;`, which a shell command would take as part of its
        // argument; both spellings are checked so the marker can't hide behind it.
        for marker in ["planted", "planted;"] {
            assert!(!root.join(marker).exists(), "a shell command ran: {marker}");
        }
        fs::remove_dir_all(&root).unwrap();
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
            message: String::new(),
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
    fn live_unknown_model_is_an_error_or_a_logged_substitution() {
        // LM Studio has answered an unknown model id both ways: HTTP 404 naming the model,
        // and HTTP 200 from whichever model is loaded. Either must be visible to the user.
        let root = scratch("live-404");
        let mut agent = live_agent("UAPZ_LIVE_MODEL", ModelType::Generative, OutputType::Text);
        agent.model = "uapz-no-such-model".into();
        agent.prompt = "Say hi.".into();
        let doc = graph(vec![node("n0", Some(NodeData::Agent(agent)))], &[]);
        let report = live_run(&doc, "n0", &root);
        match &report.result {
            Err(err) => {
                assert!(err.contains("HTTP 4"), "{err}");
                assert!(err.contains("uapz-no-such-model"), "{err}");
            }
            Ok(_) => {
                let warning = report
                    .log
                    .iter()
                    .find(|l| l.starts_with("⚠ answered by "))
                    .unwrap_or_else(|| panic!("no substitution warning in {:?}", report.log));
                assert!(warning.contains("not uapz-no-such-model"), "{warning}");
            }
        }
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
            schema.ends_with("Table extra is loaded from extra.csv with text columns: x,y"),
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

        // A writable node drops the read-only instruction; a database file that does not
        // exist yet is one the node will create, and the prompt says so.
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
        assert!(system.contains("new database, no tables yet"), "{system}");
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
    fn expanded_prompts_messages_and_unclaimed_inputs_share_one_budget() {
        let input = |id: &str, text: &str| Input {
            id: id.into(),
            name: None,
            output: Arc::new(Output::new(text.as_bytes().to_vec(), true, "text")),
        };
        let mut config = AgentConfig {
            prompt: "{{n0}}".into(),
            message: "{{n1}}".into(),
            context_size: 2,
            ..fake_agent()
        };
        let inputs = [input("n0", "abcd"), input("n1", "efgh")];
        let filled = fill_agent("n2", &config, &inputs).unwrap();
        assert_eq!(filled.config.prompt, "abcd");
        assert_eq!(filled.input, "efgh");
        config.message.push('x');
        assert!(
            fill_agent("n2", &config, &inputs)
                .err()
                .unwrap()
                .contains("context size")
        );
        config.prompt = "{{n0}}{{n0}}{{n0}}".into();
        config.message.clear();
        assert!(fill_agent("n2", &config, &inputs).is_err());

        config.prompt.clear();
        let inputs = [input("n0", "abcd"), input("n1", "ef")];
        assert_eq!(
            fill_agent("n2", &config, &inputs).unwrap().input,
            "abcd\n\nef"
        );
        config.message = "x".into();
        assert!(fill_agent("n2", &config, &inputs).is_err());

        // A nonempty message needs its own separator before unclaimed inputs.
        config.message = "abcdef".into();
        assert!(fill_agent("n2", &config, &[input("n0", "x")]).is_err());
        config.message = "abcde".into();
        assert_eq!(
            fill_agent("n2", &config, &[input("n0", "x")])
                .unwrap()
                .input,
            "abcde\n\nx"
        );
        config.prompt = "12345678".into();
        assert!(fits_context(&config, "").is_ok());
        assert!(
            fits_context(&config, "x").is_err(),
            "round partial tokens up"
        );
    }

    #[test]
    fn combined_inputs_include_separators_in_the_limit() {
        assert_eq!(join_bounded(&["", ""], 2).unwrap(), "\n\n");
        assert!(join_bounded(&["", ""], 1).is_err());
        assert_eq!(join_bounded(&["ab", "cd"], 6).unwrap(), "ab\n\ncd");
        assert!(join_bounded(&["ab", "cd"], 5).is_err());
        assert_eq!(join_bounded(&[], 0).unwrap(), "");

        // One allocation is shared by both wires. Their aggregate would exceed the
        // real limit even though each upstream output is individually within bounds.
        let output = Arc::new(Output::new(vec![b'a'; MAX_INPUT_BYTES / 2], true, "text"));
        let err =
            join_text("n2", &["n0".into(), "n1".into()], &[output.clone(), output]).unwrap_err();
        assert!(
            err.contains("combined input") && err.contains("limit"),
            "{err}"
        );
    }

    #[test]
    fn agents_place_named_inputs_where_their_templates_say() {
        let root = scratch("named");
        fs::write(root.join("data/schema.txt"), "CREATE TABLE t(a)").unwrap();
        fs::write(root.join("data/question.txt"), "how many rows?").unwrap();
        fs::write(root.join("data/notes.txt"), "notes").unwrap();
        let http = FakeHttp::new(
            200,
            r#"{"choices":[{"message":{"content":"SELECT count(*) FROM t"}}]}"#,
        );
        let agent = |prompt: &str, message: &str| {
            Some(NodeData::Agent(AgentConfig {
                prompt: prompt.into(),
                message: message.into(),
                ..fake_agent()
            }))
        };
        let doc = |agent: Option<NodeData>, names: [Option<&str>; 3]| {
            let mut nodes = vec![
                node("n0", data("schema.txt", DataFormat::Text)),
                node("n1", data("question.txt", DataFormat::Text)),
                node("n2", data("notes.txt", DataFormat::Text)),
            ];
            for (n, name) in nodes.iter_mut().zip(names) {
                n.name = name.map(str::to_string);
            }
            nodes.push(node("n3", agent));
            graph(nodes, &[("n0", "n3"), ("n1", "n3"), ("n2", "n3")])
        };
        let go = |doc: &GraphDoc| run(doc, "n3", Dirs::under(&root), &http, &with_key);
        let messages = |i: usize| -> Vec<(String, String)> {
            http.seen.lock().unwrap()[i].body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| {
                    (
                        m["role"].as_str().unwrap().to_string(),
                        m["content"].as_str().unwrap().to_string(),
                    )
                })
                .collect()
        };
        let names = [Some("schema"), Some("question"), None];
        let pair = |role: &str, text: &str| (role.to_string(), text.to_string());

        // Placeholders in the prompt and the message; the unnamed input follows the
        // message; the log says what went where.
        let report = go(&doc(
            agent("Schema:\n{{schema}}", "Q: {{ question }}"),
            names,
        ));
        report.result.as_ref().expect("agent run");
        assert_eq!(
            messages(0),
            [
                pair("system", "Schema:\nCREATE TABLE t(a)"),
                pair("user", "Q: how many rows?\n\nnotes"),
            ]
        );
        assert!(
            report.log.iter().any(|l| l
                == "n3: placed inputs: {{schema}} from n0 · {{question}} from n1 · n2 appended"),
            "{:?}",
            report.log
        );

        // An id works as a name, and a prompt that places every input with no message
        // left becomes the user message, as a prompt with no input always has.
        go(&doc(agent("{{schema}} {{n1}} {{n2}}", ""), names))
            .result
            .expect("ids as names");
        assert_eq!(
            messages(1),
            [pair("user", "CREATE TABLE t(a) how many rows? notes")]
        );

        // No placeholders: the inputs joined in id order, and nothing to say about it.
        let plain = go(&doc(agent("Summarize.", ""), names));
        plain.result.as_ref().expect("plain run");
        assert_eq!(
            messages(2)[1],
            pair("user", "CREATE TABLE t(a)\n\nhow many rows?\n\nnotes")
        );
        assert!(!plain.log.iter().any(|l| l.contains("placed inputs")));

        // Unknown and ambiguous placeholders are errors before anything is sent.
        let calls = http.seen.lock().unwrap().len();
        assert_eq!(
            go(&doc(agent("{{nope}}", ""), names)).result.unwrap_err(),
            "n3: {{nope}} names no input; the inputs are schema, question, n2"
        );
        assert_eq!(
            go(&doc(
                agent("{{schema}}", ""),
                [Some("schema"), Some("schema"), None]
            ))
            .result
            .unwrap_err(),
            "n3: {{schema}} is ambiguous: n0 and n1 share the name schema"
        );
        let alone = graph(vec![node("n3", agent("{{x}}", ""))], &[]);
        assert_eq!(
            go(&alone).result.unwrap_err(),
            "n3: {{x}} names no input: nothing is wired into n3"
        );
        assert_eq!(http.seen.lock().unwrap().len(), calls);

        // A name that does not validate is no name: the node is still reachable by id,
        // and unnamed inputs follow a message that has text.
        go(&doc(agent("{{n0}}", "ask"), [Some("bad name"), None, None]))
            .result
            .expect("invalid names are ignored");
        assert_eq!(
            messages(calls)[1],
            pair("user", "ask\n\nhow many rows?\n\nnotes")
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn naming_an_input_is_part_of_the_agents_cache_key() {
        let root = scratch("named-cache");
        fs::write(root.join("data/a.txt"), "v1").unwrap();
        let http = FakeHttp::new(200, r#"{"choices":[{"message":{"content":"ok"}}]}"#);
        let mut doc = graph(
            vec![
                node("n0", data("a.txt", DataFormat::Text)),
                node("n1", Some(NodeData::Agent(fake_agent()))),
            ],
            &[("n0", "n1")],
        );
        let (control, _) = recording_control();
        let calls = || http.seen.lock().unwrap().len();
        let reused = |doc: &GraphDoc| {
            run_with(doc, "n1", Dirs::under(&root), &http, &with_key, &control)
                .reused
                .len()
        };
        assert_eq!((reused(&doc), calls()), (0, 1));
        assert_eq!((reused(&doc), calls()), (1, 1));
        doc.nodes[0].name = Some("schema".into());
        assert_eq!((reused(&doc), calls()), (0, 2), "the name enters the key");
        assert_eq!((reused(&doc), calls()), (1, 2));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn cached_agents_revalidate_input_ids_used_in_templates() {
        let root = scratch("id-cache");
        fs::write(root.join("data/a.txt"), "same contents").unwrap();
        let http = FakeHttp::new(200, r#"{"choices":[{"message":{"content":"ok"}}]}"#);
        let mut config = fake_agent();
        config.prompt = "Summarize {{n0}}".into();
        let mut doc = graph(
            vec![
                node("n0", data("a.txt", DataFormat::Text)),
                node("n2", Some(NodeData::Agent(config))),
            ],
            &[("n0", "n2")],
        );
        let (control, _) = recording_control();
        let go =
            |doc: &GraphDoc| run_with(doc, "n2", Dirs::under(&root), &http, &with_key, &control);
        go(&doc).result.unwrap();
        assert_eq!(go(&doc).reused, ["n2"]);
        // The bytes did not change, but the named input no longer exists. A cached
        // reply must not bypass template validation, including after reopening a graph.
        doc.nodes[0].id = "n1".into();
        doc.edges[0].from = "n1".into();
        let report = go(&doc);
        assert!(
            report.result.is_err(),
            "an invalid template reused a cached answer"
        );
        assert!(report.reused.is_empty());
        assert_eq!(http.seen.lock().unwrap().len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn graph_preflight_rejects_cycles_and_invalid_dependencies_before_effects() {
        let root = scratch("preflight");
        fs::write(root.join("data/a.txt"), "payload").unwrap();
        let http = FakeHttp::new(200, "{}");
        let mut doc = graph(
            vec![
                node("n0", data("a.txt", DataFormat::Text)),
                node(
                    "n1",
                    Some(NodeData::File(FileConfig {
                        path: "written.txt".into(),
                        ..FileConfig::default()
                    })),
                ),
                node("n2", Some(NodeData::Agent(fake_agent()))),
                node("n3", Some(NodeData::Agent(fake_agent()))),
                node("n4", Some(NodeData::Agent(fake_agent()))),
            ],
            &[
                ("n0", "n1"),
                ("n1", "n4"),
                ("n2", "n3"),
                ("n3", "n2"),
                ("n3", "n4"),
            ],
        );
        for expected in ["cycle", "not valid", "not in the graph"] {
            let report = run(&doc, "n4", Dirs::under(&root), &http, &with_key);
            let error = report.result.unwrap_err();
            assert!(error.contains(expected), "{error}");
            assert!(report.outputs.is_empty());
            assert!(!root.join("written.txt").exists());
            assert!(http.seen.lock().unwrap().is_empty());
            if expected == "cycle" {
                doc.nodes[2].data = data("../outside.txt", DataFormat::Text);
            } else {
                doc.nodes.retain(|node| node.id != "n2");
            }
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn deep_graphs_run_without_recursive_stack_growth() {
        let root = scratch("deep");
        fs::write(root.join("data/a.txt"), "x").unwrap();
        let mut doc = graph(Vec::new(), &[]);
        for i in 0..4096 {
            doc.nodes
                .push(node(&format!("n{i}"), data("a.txt", DataFormat::Text)));
            if i > 0 {
                doc.edges.push(EdgeDoc {
                    output: None,
                    from: format!("n{}", i - 1),
                    to: format!("n{i}"),
                });
            }
        }
        assert_eq!(sinks(&doc), ["n4095"]);
        let http = FakeHttp::new(200, "{}");
        let report = run(&doc, "n4095", Dirs::under(&root), &http, &no_env);
        assert_eq!(report.result.unwrap().as_text(), Some("x"));
        assert_eq!(report.outputs.len(), 4096);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn run_all_includes_components_without_sinks() {
        let root = scratch("cycle-sinks");
        fs::write(root.join("data/a.txt"), "x").unwrap();
        let http = FakeHttp::new(200, "{}");
        let doc = graph(
            ["n0", "n1", "n2", "n3", "n9"]
                .iter()
                .map(|id| node(id, data("a.txt", DataFormat::Text)))
                .collect(),
            &[("n0", "n1"), ("n1", "n2"), ("n2", "n1"), ("n3", "n3")],
        );
        assert_eq!(sinks(&doc), ["n1", "n3", "n9"]);
        let report = run_all_with(
            &doc,
            &sinks(&doc),
            Dirs::under(&root),
            &http,
            &no_env,
            &Control::default(),
        );
        assert!(report.result.unwrap_err().contains("cycle"));
        assert_eq!(report.outputs.len(), 1);
        assert!(report.outputs.contains_key("n9"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn independent_failures_each_get_a_failed_state() {
        let root = scratch("failure-states");
        let doc = graph(
            ["n0", "n1"]
                .iter()
                .map(|id| node(id, data("missing.txt", DataFormat::Text)))
                .collect(),
            &[],
        );
        let http = FakeHttp::new(200, "{}");
        let (control, seen) = recording_control();
        let report = run_all_with(
            &doc,
            &sinks(&doc),
            Dirs::under(&root),
            &http,
            &no_env,
            &control,
        );
        assert!(report.result.is_err());
        assert_eq!(report.failed.as_deref(), Some("n0"));
        let failed: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .filter_map(|p| match p {
                Progress::Node {
                    id,
                    state: NodeState::Failed,
                } => Some(id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(failed, ["n0", "n1"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancellation_takes_precedence_over_an_earlier_target_failure() {
        let root = scratch("failure-cancel");
        let doc = graph(
            ["n0", "n1"]
                .iter()
                .map(|id| node(id, data("missing.txt", DataFormat::Text)))
                .collect(),
            &[],
        );
        let http = FakeHttp::new(200, "{}");
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let control = Control {
            cancel,
            progress: Some(Arc::new(move |p| {
                if matches!(
                    p,
                    Progress::Node {
                        state: NodeState::Failed,
                        ..
                    }
                ) {
                    flag.store(true, Ordering::Relaxed);
                }
            })),
            ..Control::default()
        };
        let report = run_all_with(
            &doc,
            &sinks(&doc),
            Dirs::under(&root),
            &http,
            &no_env,
            &control,
        );
        assert_eq!(report.result.unwrap_err(), CANCELLED);
        assert!(report.outputs.is_empty());
        assert!(!report.log.iter().any(|line| line.starts_with("n1:")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reviewed_note_example_runs_offline_with_per_run_overrides() {
        let root = scratch("reviewed-example");
        let doc: GraphDoc =
            serde_json::from_str(include_str!("../examples/reviewed-note.json")).unwrap();
        let original = serde_json::to_string(&doc).unwrap();
        let http = FakeHttp::new(200, "{}");
        let mut control = Control {
            inputs: workflow::Values::from([("note".into(), "# Changed for this run\n".into())]),
            review: Some(Arc::new(|_| Ok(review::Decision::Reject))),
            ..Control::default()
        };
        assert_eq!(
            run_all_with(
                &doc,
                &sinks(&doc),
                Dirs::under(&root),
                &http,
                &no_env,
                &control
            )
            .result
            .unwrap_err(),
            review::REJECTED
        );
        assert!(!root.join("generated").exists());
        control.review = Some(Arc::new(|request| {
            assert_eq!(request.files[0].path, "generated/note.md");
            assert!(request.files[0].diff.contains("+# Changed for this run"));
            Ok(review::Decision::Apply)
        }));
        let report = run_all_with(
            &doc,
            &sinks(&doc),
            Dirs::under(&root),
            &http,
            &no_env,
            &control,
        );
        report.result.unwrap();
        assert_eq!(report.inputs["note"], "# Changed for this run\n");
        assert_eq!(
            fs::read_to_string(root.join("generated/note.md")).unwrap(),
            "# Changed for this run\n"
        );
        assert_eq!(serde_json::to_string(&doc).unwrap(), original);
        assert!(http.seen.lock().unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn run_inputs_flow_through_templates_and_invalidate_cached_agents() {
        let root = scratch("run-inputs");
        let mut question = node(
            "n0",
            Some(NodeData::Input(workflow::InputConfig {
                default: "Default question".into(),
                ..workflow::InputConfig::default()
            })),
        );
        question.name = Some("question".into());
        let mut count = node(
            "n1",
            Some(NodeData::Input(workflow::InputConfig {
                kind: workflow::InputType::Number,
                default: "2".into(),
                required: true,
            })),
        );
        count.name = Some("count".into());
        let doc = graph(
            vec![
                question,
                count,
                node(
                    "n2",
                    Some(NodeData::Agent(AgentConfig {
                        prompt: "Question: {{question}}".into(),
                        message: "Count: {{count}}".into(),
                        ..fake_agent()
                    })),
                ),
            ],
            &[("n0", "n2"), ("n1", "n2")],
        );
        let original = serde_json::to_string(&doc).unwrap();
        let http = FakeHttp::new(200, r#"{"choices":[{"message":{"content":"answer"}}]}"#);
        let (mut control, _) = recording_control();
        let first = run_with(&doc, "n2", Dirs::under(&root), &http, &with_key, &control);
        first.result.as_ref().unwrap();
        assert_eq!(first.inputs["question"], "Default question");
        assert_eq!(first.inputs["count"], "2");
        assert_eq!(
            http.seen.lock().unwrap()[0].body["messages"][0]["content"],
            "Question: Default question"
        );
        let repeated = run_with(&doc, "n2", Dirs::under(&root), &http, &with_key, &control);
        repeated.result.unwrap();
        assert_eq!(repeated.reused, ["n2"]);
        control.inputs.insert(
            "question".into(),
            "{{literal}} is not another template".into(),
        );
        let changed = run_with(&doc, "n2", Dirs::under(&root), &http, &with_key, &control);
        changed.result.as_ref().unwrap();
        assert!(changed.reused.is_empty());
        assert_eq!(http.seen.lock().unwrap().len(), 2);
        assert_eq!(
            http.seen.lock().unwrap()[1].body["messages"][0]["content"],
            "Question: {{literal}} is not another template"
        );
        let record = crate::history::record(&doc, &["n2".into()], &changed, None);
        assert_eq!(record.inputs, changed.inputs);
        assert_eq!(
            serde_json::to_string(&doc).unwrap(),
            original,
            "run values never mutate the graph"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_input_values_and_file_contents_both_enter_the_cache_key() {
        let root = scratch("file-inputs");
        for name in ["a.txt", "b.txt"] {
            fs::write(root.join("data").join(name), "same bytes").unwrap();
        }
        let mut input = node(
            "n0",
            Some(NodeData::Input(workflow::InputConfig {
                kind: workflow::InputType::File,
                default: "a.txt".into(),
                required: true,
            })),
        );
        input.name = Some("attachment".into());
        let doc = graph(
            vec![input, node("n1", Some(NodeData::Agent(fake_agent())))],
            &[("n0", "n1")],
        );
        let http = FakeHttp::new(200, r#"{"choices":[{"message":{"content":"answer"}}]}"#);
        let (mut control, _) = recording_control();
        run_with(&doc, "n1", Dirs::under(&root), &http, &with_key, &control)
            .result
            .unwrap();
        control.inputs.insert("attachment".into(), "b.txt".into());
        run_with(&doc, "n1", Dirs::under(&root), &http, &with_key, &control)
            .result
            .unwrap();
        assert_eq!(
            http.seen.lock().unwrap().len(),
            2,
            "a different selection is a different run input"
        );
        fs::write(root.join("data/b.txt"), "changed bytes").unwrap();
        run_with(&doc, "n1", Dirs::under(&root), &http, &with_key, &control)
            .result
            .unwrap();
        assert_eq!(http.seen.lock().unwrap().len(), 3);
        control
            .inputs
            .insert("attachment".into(), "../outside.txt".into());
        assert!(
            run_with(&doc, "n1", Dirs::under(&root), &http, &with_key, &control)
                .result
                .is_err()
        );
        assert_eq!(http.seen.lock().unwrap().len(), 3);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_or_invalid_run_values_fail_before_any_side_effect() {
        let root = scratch("input-preflight");
        fs::write(root.join("data/source.txt"), "payload").unwrap();
        let mut input = node(
            "n2",
            Some(NodeData::Input(workflow::InputConfig {
                kind: workflow::InputType::Number,
                ..workflow::InputConfig::default()
            })),
        );
        input.name = Some("amount".into());
        let doc = graph(
            vec![
                node("n0", data("source.txt", DataFormat::Text)),
                node(
                    "n1",
                    Some(NodeData::File(FileConfig {
                        path: "out.txt".into(),
                        ..FileConfig::default()
                    })),
                ),
                input,
                node("n3", Some(NodeData::Agent(fake_agent()))),
            ],
            &[("n0", "n1"), ("n1", "n3"), ("n2", "n3")],
        );
        let http = FakeHttp::new(200, "{}");
        for supplied in [
            workflow::Values::new(),
            workflow::Values::from([("amount".into(), "NaN".into())]),
        ] {
            let report = run_with(
                &doc,
                "n3",
                Dirs::under(&root),
                &http,
                &with_key,
                &Control {
                    inputs: supplied,
                    ..Control::default()
                },
            );
            assert!(report.result.unwrap_err().contains("amount"));
            assert!(report.outputs.is_empty());
            assert!(!root.join("out.txt").exists());
        }
        assert!(http.seen.lock().unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reviewed_writes_pause_downstream_execution_until_the_batch_is_approved() {
        let root = scratch("review-apply");
        fs::write(
            root.join("data/answer.txt"),
            "=== a.txt ===\nfirst\n=== b.txt ===\nsecond\n",
        )
        .unwrap();
        let doc = graph(
            vec![
                node("n0", data("answer.txt", DataFormat::Text)),
                node(
                    "n1",
                    Some(NodeData::File(FileConfig {
                        path: "out".into(),
                        bundle: true,
                        review: true,
                        ..FileConfig::default()
                    })),
                ),
                node("n2", Some(NodeData::Agent(fake_agent()))),
            ],
            &[("n0", "n1"), ("n1", "n2")],
        );
        let http = Arc::new(FakeHttp::new(
            200,
            r#"{"choices":[{"message":{"content":"done"}}]}"#,
        ));
        let (tx, rx) = std::sync::mpsc::channel();
        let (mut control, _) = recording_control();
        let cancel = control.cancel.clone();
        control.review = Some(Arc::new(move |request| {
            review::wait(request, &cancel, |pending| tx.send(pending).unwrap())
        }));
        let worker_root = root.clone();
        let worker_http = http.clone();
        let worker = thread::spawn(move || {
            run_with(
                &doc,
                "n2",
                Dirs::under(&worker_root),
                &*worker_http,
                &with_key,
                &control,
            )
        });
        let pending = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(pending.request.node, "n1");
        assert_eq!(pending.request.files.len(), 2);
        assert!(pending.request.files[0].diff.contains("+first"));
        assert!(!root.join("out").exists());
        assert!(http.seen.lock().unwrap().is_empty());
        assert!(pending.decide(review::Decision::Apply));
        let report = worker.join().unwrap();
        assert_eq!(report.result.unwrap().as_text(), Some("done"));
        assert_eq!(
            fs::read_to_string(root.join("out/a.txt")).unwrap(),
            "first\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("out/b.txt")).unwrap(),
            "second\n"
        );
        assert_eq!(http.seen.lock().unwrap().len(), 1);
        assert!(report.log.iter().any(|line| line.contains("approved 2")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejecting_or_cancelling_review_never_writes_or_runs_downstream() {
        let root = scratch("review-reject");
        fs::write(root.join("data/answer.txt"), "new").unwrap();
        fs::write(root.join("out.txt"), "old").unwrap();
        let doc = graph(
            vec![
                node("n0", data("answer.txt", DataFormat::Text)),
                node(
                    "n1",
                    Some(NodeData::File(FileConfig {
                        path: "out.txt".into(),
                        mode: WriteMode::Overwrite,
                        review: true,
                        ..FileConfig::default()
                    })),
                ),
                node("n2", Some(NodeData::Agent(fake_agent()))),
            ],
            &[("n0", "n1"), ("n1", "n2")],
        );
        let http = FakeHttp::new(200, "{}");
        let no_reviewer = run(&doc, "n2", Dirs::under(&root), &http, &with_key);
        assert!(no_reviewer.result.unwrap_err().contains("requires review"));
        for cancel in [false, true] {
            let mut control = Control::default();
            let flag = control.cancel.clone();
            control.review = Some(Arc::new(move |request| {
                assert!(request.files[0].diff.contains("-old"));
                if cancel {
                    flag.store(true, Ordering::Relaxed);
                    Ok(review::Decision::Apply)
                } else {
                    Ok(review::Decision::Reject)
                }
            }));
            let report = run_with(&doc, "n2", Dirs::under(&root), &http, &with_key, &control);
            assert_eq!(
                report.result.unwrap_err(),
                if cancel { CANCELLED } else { review::REJECTED }
            );
            assert!(!report.outputs.contains_key("n1"));
            assert!(!report.outputs.contains_key("n2"));
            assert_eq!(fs::read_to_string(root.join("out.txt")).unwrap(), "old");
        }
        assert!(http.seen.lock().unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_stale_file_stops_an_approved_batch_before_its_first_write() {
        let root = scratch("review-stale");
        fs::create_dir(root.join("out")).unwrap();
        fs::write(root.join("out/b.txt"), "original").unwrap();
        fs::write(
            root.join("data/answer.txt"),
            "=== a.txt ===\nnew file\n=== b.txt ===\nreplacement\n",
        )
        .unwrap();
        let doc = graph(
            vec![
                node("n0", data("answer.txt", DataFormat::Text)),
                node(
                    "n1",
                    Some(NodeData::File(FileConfig {
                        path: "out".into(),
                        mode: WriteMode::Overwrite,
                        bundle: true,
                        review: true,
                        ..FileConfig::default()
                    })),
                ),
            ],
            &[("n0", "n1")],
        );
        let edited = root.join("out/b.txt");
        let control = Control {
            review: Some(Arc::new(move |_| {
                fs::write(&edited, "edited during review").unwrap();
                Ok(review::Decision::Apply)
            })),
            ..Control::default()
        };
        let report = run_with(
            &doc,
            "n1",
            Dirs::under(&root),
            &FakeHttp::new(200, "{}"),
            &no_env,
            &control,
        );
        assert!(report.result.unwrap_err().contains("changed since review"));
        assert!(!root.join("out/a.txt").exists());
        assert_eq!(
            fs::read_to_string(root.join("out/b.txt")).unwrap(),
            "edited during review"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn run_all_covers_every_sink_once_and_outlives_a_failure() {
        let root = scratch("run-all");
        fs::write(root.join("data/a.txt"), "v1").unwrap();
        fs::write(root.join("scripts/count.sh"), "wc -c | tr -d ' '\n").unwrap();
        fs::write(root.join("scripts/fail.sh"), "exit 2\n").unwrap();
        let http = FakeHttp::new(200, "{}");
        // n0 feeds n1 and n2; n2 fails and feeds n3; n1 feeds n10, which feeds only the
        // blank n4. The sinks are n3 and n10, in numeric order.
        let doc = graph(
            vec![
                node("n0", data("a.txt", DataFormat::Text)),
                node("n1", shell("count.sh", &[], 10)),
                node("n2", shell("fail.sh", &[], 10)),
                node("n3", shell("count.sh", &[], 10)),
                node("n4", None),
                node("n10", shell("count.sh", &[], 10)),
            ],
            &[
                ("n0", "n1"),
                ("n0", "n2"),
                ("n2", "n3"),
                ("n1", "n10"),
                ("n10", "n4"),
            ],
        );
        assert_eq!(sinks(&doc), ["n3", "n10"]);
        assert!(sinks(&graph(vec![node("n0", None)], &[])).is_empty());
        let states = |seen: &Arc<Mutex<Vec<Progress>>>, id: &str| -> Vec<NodeState> {
            seen.lock()
                .unwrap()
                .iter()
                .filter_map(|p| match p {
                    Progress::Node { id: i, state } if i == id => Some(*state),
                    _ => None,
                })
                .collect()
        };
        let (control, seen) = recording_control();
        let go = |targets: &[String], control: &Control| {
            run_all_with(&doc, targets, Dirs::under(&root), &http, &no_env, control)
        };

        let report = go(&sinks(&doc), &control);
        let err = report.result.unwrap_err();
        assert!(err.contains("fail.sh"), "{err}");
        assert_eq!(report.failed.as_deref(), Some("n2"));
        let mut ran: Vec<&str> = report.outputs.keys().map(String::as_str).collect();
        ran.sort();
        assert_eq!(ran, ["n0", "n1", "n10"]);
        assert_eq!(report.log[0], "run all: n3, n10");
        assert_eq!(report.log.last().unwrap(), "run all: 1 of 2 finished");
        // The shared input ran once; the failure was marked once.
        assert_eq!(states(&seen, "n0"), [NodeState::Running, NodeState::Done]);
        assert_eq!(states(&seen, "n2"), [NodeState::Running, NodeState::Failed]);

        // A target asked for twice, or after a failed input, is not run or retried.
        seen.lock().unwrap().clear();
        let twice: Vec<String> = ["n3", "n10", "n3", "n10"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let report = go(&twice, &control);
        assert_eq!(report.log.last().unwrap(), "run all: 2 of 4 finished");
        assert_eq!(states(&seen, "n2").len(), 2);
        assert_eq!(states(&seen, "n10"), [NodeState::Reused]);

        // A cancel stops before the first target; nothing to run is an error.
        let cancelled = Control {
            cancel: Arc::new(AtomicBool::new(true)),
            ..control.clone()
        };
        let report = go(&sinks(&doc), &cancelled);
        assert_eq!(report.result.unwrap_err(), CANCELLED);
        assert!(report.outputs.is_empty());
        assert_eq!(
            go(&[], &control).result.unwrap_err(),
            "nothing to run: no node has content"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn cache_reuses_agents_and_scripts_until_their_inputs_change() {
        let root = scratch("cache");
        fs::write(root.join("data/a.txt"), "v1").unwrap();
        fs::write(root.join("scripts/count.sh"), "wc -c | tr -d ' '\n").unwrap();
        let http = FakeHttp::new(200, r#"{"choices":[{"message":{"content":"ok"}}]}"#);
        let doc = graph(
            vec![
                node("n0", data("a.txt", DataFormat::Text)),
                node("n1", shell("count.sh", &[], 10)),
                node("n2", Some(NodeData::Agent(fake_agent()))),
            ],
            &[("n0", "n1"), ("n1", "n2")],
        );
        let calls = || http.seen.lock().unwrap().len();
        let (control, _) = recording_control();
        let go =
            |control: &Control| run_with(&doc, "n2", Dirs::under(&root), &http, &with_key, control);

        let first = go(&control);
        assert_eq!(first.result.as_ref().unwrap().as_text(), Some("ok"));
        assert_eq!((first.reused.len(), calls()), (0, 1));

        // Nothing changed: the script and the agent come from the cache.
        let again = go(&control);
        assert_eq!((again.reused.len(), calls()), (2, 1));
        assert_eq!(again.reused, ["n1", "n2"]);
        assert!(
            again
                .log
                .iter()
                .any(|l| l == "n2: reused the cached Text · 2 B"),
            "{:?}",
            again.log
        );
        assert_eq!(lock(control.cache.as_ref().unwrap()).len(), 2);

        // The data changed, so the script output changes and the agent runs again.
        fs::write(root.join("data/a.txt"), "v222").unwrap();
        assert_eq!((go(&control).reused.len(), calls()), (0, 2));

        // The script file changed but its output did not: only the script reruns.
        fs::write(
            root.join("scripts/count.sh"),
            "# counts bytes\nwc -c | tr -d ' '\n",
        )
        .unwrap();
        assert_eq!((go(&control).reused.len(), calls()), (1, 2));

        // Fresh ignores the cache and refills it.
        let fresh = Control {
            fresh: true,
            ..control.clone()
        };
        assert_eq!((go(&fresh).reused.len(), calls()), (0, 3));
        assert_eq!((go(&control).reused.len(), calls()), (2, 3));

        // Clearing it, or running with no cache at all, runs everything.
        lock(control.cache.as_ref().unwrap()).clear();
        assert_eq!((go(&control).reused.len(), calls()), (0, 4));
        assert_eq!((go(&Control::default()).reused.len(), calls()), (0, 5));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn progress_reports_each_nodes_state() {
        let root = scratch("states");
        fs::write(root.join("data/a.txt"), "v1").unwrap();
        fs::write(root.join("scripts/count.sh"), "wc -c | tr -d ' '\n").unwrap();
        fs::write(root.join("scripts/fail.sh"), "exit 2\n").unwrap();
        let http = FakeHttp::new(200, r#"{"choices":[{"message":{"content":"ok"}}]}"#);
        let doc = graph(
            vec![
                node("n0", data("a.txt", DataFormat::Text)),
                node("n1", shell("count.sh", &[], 10)),
                node("n2", Some(NodeData::Agent(fake_agent()))),
                node("n3", shell("fail.sh", &[], 10)),
                node("n4", shell("count.sh", &[], 10)),
            ],
            &[("n0", "n1"), ("n1", "n2"), ("n0", "n3"), ("n3", "n4")],
        );
        let states = |seen: &Arc<Mutex<Vec<Progress>>>| -> Vec<(String, NodeState)> {
            seen.lock()
                .unwrap()
                .iter()
                .filter_map(|p| match p {
                    Progress::Node { id, state } => Some((id.clone(), *state)),
                    _ => None,
                })
                .collect()
        };
        let s = |id: &str, state: NodeState| (id.to_string(), state);
        use NodeState::*;

        let (control, seen) = recording_control();
        run_with(&doc, "n2", Dirs::under(&root), &http, &with_key, &control)
            .result
            .unwrap();
        assert_eq!(
            states(&seen),
            [
                s("n0", Running),
                s("n0", Done),
                s("n1", Running),
                s("n1", Done),
                s("n2", Running),
                s("n2", Done)
            ]
        );

        // Second time: the data node runs, the rest is reused and never starts.
        seen.lock().unwrap().clear();
        run_with(&doc, "n2", Dirs::under(&root), &http, &with_key, &control)
            .result
            .unwrap();
        assert_eq!(
            states(&seen),
            [
                s("n0", Running),
                s("n0", Done),
                s("n1", Reused),
                s("n2", Reused)
            ]
        );

        // A failing node is marked once; the node after it is never reached.
        seen.lock().unwrap().clear();
        let report = run_with(&doc, "n4", Dirs::under(&root), &http, &with_key, &control);
        assert!(report.result.is_err());
        assert_eq!(
            states(&seen),
            [
                s("n0", Running),
                s("n0", Done),
                s("n3", Running),
                s("n3", Failed)
            ]
        );

        // A cancel is not a failure.
        let cancelled = Control {
            cancel: Arc::new(AtomicBool::new(true)),
            ..control.clone()
        };
        seen.lock().unwrap().clear();
        run_with(&doc, "n2", Dirs::under(&root), &http, &with_key, &cancelled)
            .result
            .unwrap_err();
        assert!(states(&seen).is_empty(), "{:?}", states(&seen));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn persistent_cache_survives_a_restart_and_clears_its_files() {
        let root = scratch("disk-cache");
        fs::write(root.join("data/a.txt"), "v1").unwrap();
        let http = FakeHttp::new(200, r#"{"choices":[{"message":{"content":"ok"}}]}"#);
        let doc = graph(
            vec![
                node("n0", data("a.txt", DataFormat::Text)),
                node("n1", Some(NodeData::Agent(fake_agent()))),
            ],
            &[("n0", "n1")],
        );
        let dirs = Dirs::under(&root);
        let calls = || http.seen.lock().unwrap().len();
        let open = || Control {
            cache: Some(Arc::new(Mutex::new(Cache::persistent(dirs.cache.clone())))),
            ..Control::default()
        };

        let first = open();
        let report = run_with(&doc, "n1", dirs.clone(), &http, &with_key, &first);
        report.result.as_ref().unwrap();
        assert_eq!((report.reused.len(), calls()), (0, 1));
        let files: Vec<String> = fs::read_dir(&dirs.cache)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files.len(), 2, "{files:?}");
        assert!(
            files
                .iter()
                .any(|f| f.len() == 32 + 5 && f.ends_with(".json")),
            "{files:?}"
        );
        assert!(files.iter().any(|f| f.ends_with(".bin")), "{files:?}");
        assert_eq!(lock(first.cache.as_ref().unwrap()).len(), 1);

        // A new cache over the same directory, as after a restart, has the entry.
        let second = open();
        assert_eq!(lock(second.cache.as_ref().unwrap()).len(), 1);
        let report = run_with(&doc, "n1", dirs.clone(), &http, &with_key, &second);
        assert_eq!(report.result.unwrap().as_text(), Some("ok"));
        assert_eq!((report.reused, calls()), (vec!["n1".to_string()], 1));

        // Another node id with the same question hits too: entries are by content.
        let mut moved = doc.clone();
        moved.nodes[1].id = "n9".into();
        moved.edges[0].to = "n9".into();
        let report = run_with(&moved, "n9", dirs.clone(), &http, &with_key, &open());
        assert_eq!((report.reused, calls()), (vec!["n9".to_string()], 1));

        // A broken sidecar is a miss, not a crash.
        for file in files.iter().filter(|f| f.ends_with(".json")) {
            fs::write(dirs.cache.join(file), "{not json").unwrap();
        }
        let report = run_with(&doc, "n1", dirs.clone(), &http, &with_key, &open());
        assert_eq!((report.reused.len(), calls()), (0, 2));

        // Clear removes the files.
        let control = open();
        lock(control.cache.as_ref().unwrap()).clear();
        assert_eq!(fs::read_dir(&dirs.cache).unwrap().count(), 0);
        assert_eq!(lock(control.cache.as_ref().unwrap()).len(), 0);

        // Keys are stable values, not whatever the standard hasher does this build.
        assert_eq!(
            format!("{:032x}", key_of(&[b"agent", b"x"])),
            "c2c0d39782eaa908e9c9d98fcd63a797"
        );
        assert_ne!(key_of(&[b"ab", b"c"]), key_of(&[b"a", b"bc"]));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn oversized_cache_entries_are_misses() {
        let root = scratch("oversized-cache");
        let mut cache = Cache::persistent(root.join("cache"));
        let output = Output::new(b"ok".to_vec(), true, "Text");
        cache.write(1, &output).unwrap();
        let (_, bin) = cache.paths(1).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(bin)
            .unwrap()
            .set_len(MAX_OUTPUT_BYTES as u64 + 1)
            .unwrap();
        assert!(
            cache.fetch(1).is_none(),
            "an oversized cache file was accepted"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupt_cache_bytes_are_misses_and_history_can_repair_them() {
        let root = scratch("corrupt-cache");
        let mut cache = Cache::persistent(root.join("cache"));
        let output = Output::new(b"good".to_vec(), true, "Text");
        cache.write(1, &output).unwrap();
        let (meta, bin) = cache.paths(1).unwrap();
        fs::write(&bin, b"evil").unwrap();
        assert!(
            cache.fetch(1).is_none(),
            "a same-length corrupt payload was accepted"
        );
        assert!(cache.keep(1, &output));
        assert_eq!(cache.fetch(1).unwrap().bytes, b"good");
        let mut legacy: serde_json::Value =
            serde_json::from_slice(&fs::read(&meta).unwrap()).unwrap();
        legacy.as_object_mut().unwrap().remove("checksum");
        fs::write(&meta, serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert!(cache.fetch(1).is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cache_never_reads_writes_or_clears_a_symlinked_directory() {
        let root = scratch("linked-cache");
        let destination = root.join("elsewhere");
        fs::create_dir(&destination).unwrap();
        let preserved = destination.join("user.json");
        fs::write(&preserved, b"original").unwrap();
        std::os::unix::fs::symlink(&destination, root.join("cache")).unwrap();
        let mut cache = Cache::persistent(root.join("cache"));
        assert_eq!(cache.len(), 0);
        assert!(
            cache
                .write(1, &Output::new(b"new".to_vec(), true, "Text"))
                .is_err()
        );
        assert!(cache.fetch(1).is_none());
        cache.clear();
        assert_eq!(fs::read(preserved).unwrap(), b"original");
        assert_eq!(fs::read_dir(destination).unwrap().count(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cache_writes_never_follow_a_planted_temporary_symlink() {
        let root = scratch("cache-temp-symlink");
        let target = root.join("untouched");
        fs::write(&target, b"original").unwrap();
        let path = root.join("entry.bin");
        std::os::unix::fs::symlink(&target, path.with_extension("tmp")).unwrap();
        let _ = write_atomically(&path, b"cached output");
        assert_eq!(fs::read(&target).unwrap(), b"original");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancellation_after_a_whole_reply_prevents_saving_and_caching() {
        struct CancellingHttp(Arc<AtomicBool>);
        impl Http for CancellingHttp {
            fn post_json(&self, _: &providers::Request) -> Result<providers::Reply, String> {
                self.0.store(true, Ordering::Relaxed);
                Ok(providers::Reply {
                    status: 200,
                    body: r#"{"choices":[{"message":{"content":"cancelled output"}}]}"#.into(),
                })
            }
        }
        let root = scratch("cancel-whole-reply");
        let (control, _) = recording_control();
        let http = CancellingHttp(control.cancel.clone());
        let doc = graph(vec![node("n0", Some(NodeData::Agent(fake_agent())))], &[]);
        let report = run_with(&doc, "n0", Dirs::under(&root), &http, &with_key, &control);
        assert_eq!(report.result.unwrap_err(), CANCELLED);
        assert!(report.outputs.is_empty());
        assert_eq!(lock(control.cache.as_ref().unwrap()).len(), 0);
        assert_eq!(fs::read_dir(root.join("agents")).unwrap().count(), 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bundled_file_nodes_write_each_section_under_their_directory() {
        let root = scratch("bundle");
        fs::write(
            root.join("data/answer.md"),
            "Here are the files.\n\n=== src/add_test.rs ===\n```rust\n#[test]\nfn t() {}\n```\n\n=== notes/README.md ===\n# notes\n",
        )
        .unwrap();
        fs::write(root.join("data/escape.md"), "=== ../out.rs ===\nx\n").unwrap();
        fs::write(root.join("data/plain.md"), "no sections here\n").unwrap();
        fs::write(
            root.join("data/dupe.md"),
            "=== a.txt ===\n1\n=== a.txt ===\n2\n",
        )
        .unwrap();
        let http = FakeHttp::new(200, "{}");
        let bundle = |path: &str, mode: WriteMode| {
            Some(NodeData::File(FileConfig {
                path: path.into(),
                mode,
                unfence: true,
                bundle: true,
                review: false,
            }))
        };
        let doc = graph(
            vec![
                node("n0", data("answer.md", DataFormat::Markdown)),
                node("n1", bundle("tests/generated", WriteMode::Create)),
                node("n2", data("escape.md", DataFormat::Markdown)),
                node("n3", bundle("tests/generated", WriteMode::Overwrite)),
                node("n4", data("plain.md", DataFormat::Markdown)),
                node("n5", bundle("tests/generated", WriteMode::Overwrite)),
                node("n6", data("dupe.md", DataFormat::Markdown)),
                node("n7", bundle(".", WriteMode::Overwrite)),
                node("n8", bundle("scripts", WriteMode::Overwrite)),
            ],
            &[
                ("n0", "n1"),
                ("n2", "n3"),
                ("n4", "n5"),
                ("n6", "n7"),
                ("n0", "n8"),
            ],
        );
        let go = |id: &str| run(&doc, id, Dirs::under(&root), &http, &no_env);

        let report = go("n1");
        let out = report.result.expect("bundle");
        assert_eq!(
            fs::read_to_string(root.join("tests/generated/src/add_test.rs")).unwrap(),
            "#[test]\nfn t() {}\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("tests/generated/notes/README.md")).unwrap(),
            "# notes\n"
        );
        let text = out.as_text().unwrap();
        assert!(
            text.starts_with("Wrote 2 files under tests/generated/\n"),
            "{text}"
        );
        assert!(
            text.contains("tests/generated/src/add_test.rs (18 B) created"),
            "{text}"
        );
        assert_eq!(out.label, "Wrote 2 files · 26 B");
        assert_eq!(
            out.saved.as_deref(),
            Some(
                root.canonicalize()
                    .unwrap()
                    .join("tests/generated")
                    .as_path()
            )
        );
        // Create only: an existing file stops the whole write.
        let err = go("n1").result.unwrap_err();
        assert!(err.contains("exists; nothing was written"), "{err}");

        // Section paths can't leave the directory; nothing is written.
        let err = go("n3").result.unwrap_err();
        assert!(err.contains(".."), "{err}");
        assert!(!root.join("out.rs").exists());
        // No sections: say so.
        let err = go("n5").result.unwrap_err();
        assert!(err.contains("no `=== path ===` sections"), "{err}");
        // The same path twice is refused before anything is written.
        let err = go("n7").result.unwrap_err();
        assert!(err.contains("appears twice"), "{err}");
        assert!(!root.join("a.txt").exists());
        // The directory itself is checked against the app's directories.
        let err = go("n8").result.unwrap_err();
        assert!(err.contains("managed by the app"), "{err}");
        assert!(!root.join("scripts/src").exists());

        assert_eq!(
            split_sections("```\n=== a ===\nx\n===  b/c.txt ===\ny\nz\n```"),
            [
                ("a".to_string(), "x\n".to_string()),
                ("b/c.txt".to_string(), "y\nz\n".to_string())
            ]
        );
        assert!(split_sections("=== ===\nx").is_empty());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn cancel_kills_scripts_and_ends_the_run() {
        let root = scratch("cancel");
        fs::write(root.join("scripts/slow.sh"), "sleep 20\n").unwrap();
        let http = FakeHttp::new(200, "{}");
        let doc = graph(vec![node("n0", shell("slow.sh", &[], 60))], &[]);
        let (control, seen) = recording_control();
        let flag = control.cancel.clone();
        let setter = thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            flag.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        let report = run_with(&doc, "n0", Dirs::under(&root), &http, &no_env, &control);
        setter.join().unwrap();
        assert_eq!(report.result.unwrap_err(), CANCELLED);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "killed promptly"
        );
        let progress = seen.lock().unwrap();
        assert!(
            progress
                .iter()
                .any(|p| matches!(p, Progress::Log(l) if l.starts_with("$ sh "))),
            "{progress:?}"
        );

        // Already cancelled: nothing runs at all.
        let report = run_with(&doc, "n0", Dirs::under(&root), &http, &no_env, &control);
        assert_eq!(report.result.unwrap_err(), CANCELLED);
        assert!(report.log.is_empty(), "{:?}", report.log);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn agents_stream_text_into_progress_and_stop_on_cancel() {
        let root = scratch("stream");
        let http = FakeHttp::new(200, &sse(&["Hel", "lo", " there"]));
        let doc = graph(vec![node("n0", Some(NodeData::Agent(fake_agent())))], &[]);
        let (control, seen) = recording_control();
        let report = run_with(&doc, "n0", Dirs::under(&root), &http, &with_key, &control);
        assert_eq!(report.result.unwrap().as_text(), Some("Hello there"));
        assert_eq!(http.seen.lock().unwrap()[0].body["stream"], true);
        let texts: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .filter_map(|p| match p {
                Progress::Text { id, text } if id == "n0" => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["Hel", "lo", " there"]);
        assert!(
            !report.log.iter().any(|l| l.starts_with('⚠')),
            "{:?}",
            report.log
        );
        let logs = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|p| matches!(p, Progress::Log(_)))
            .count();
        assert_eq!(logs, report.log.len());

        // Cancelling while text streams in ends the run as cancelled, with nothing saved.
        let outputs = || {
            fs::read_dir(root.join("agents/n0/outputs"))
                .unwrap()
                .count()
        };
        let saved_before = outputs();
        let flag = Arc::new(AtomicBool::new(false));
        let stop = flag.clone();
        let control = Control {
            cache: None,
            fresh: false,
            cancel: flag,
            progress: Some(Arc::new(move |p| {
                if matches!(p, Progress::Text { .. }) {
                    stop.store(true, Ordering::Relaxed);
                }
            })),
            ..Control::default()
        };
        let report = run_with(&doc, "n0", Dirs::under(&root), &http, &with_key, &control);
        assert_eq!(report.result.unwrap_err(), CANCELLED);
        assert_eq!(outputs(), saved_before);

        // A server answering with another model than asked is called out in the log;
        // snapshot and tag suffixes are the same model.
        let other = FakeHttp::new(
            200,
            "data: {\"model\":\"loaded-instead\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n",
        );
        let report = run(&doc, "n0", Dirs::under(&root), &other, &with_key);
        assert_eq!(report.result.unwrap().as_text(), Some("hi"));
        assert!(
            report
                .log
                .iter()
                .any(|l| l == "⚠ answered by loaded-instead, not m: the server substituted a model"),
            "{:?}",
            report.log
        );
        let same = |answered: &str| substituted_model(&fake_agent(), Some(answered)).is_none();
        assert!(same("m"));
        assert!(same("m-2024-08-06"));
        assert!(same("m:latest"));
        assert!(!same("n"));
        assert!(substituted_model(&fake_agent(), None).is_none());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn file_nodes_write_wire_text_inside_the_root() {
        let root = scratch("file");
        fs::write(root.join("data/code.md"), "```rust\nfn t() {}\n```").unwrap();
        fs::write(root.join("data/img.png"), [0x89, b'P', b'N', b'G', 0, 0xff]).unwrap();
        fs::write(root.join("data/empty.txt"), "").unwrap();
        let http = FakeHttp::new(200, "{}");
        let file = |path: &str, mode: WriteMode, unfence: bool| {
            Some(NodeData::File(FileConfig {
                path: path.into(),
                mode,
                unfence,
                bundle: false,
                review: false,
            }))
        };
        let doc = graph(
            vec![
                node("n0", data("code.md", DataFormat::Markdown)),
                node("n1", file("tests/gen/t.rs", WriteMode::Create, true)),
                node("n2", file("tests/gen/t.rs", WriteMode::Overwrite, false)),
                node("n3", file("tests/gen/t.rs", WriteMode::Append, true)),
                node("n4", file("scripts/planted.sh", WriteMode::Overwrite, true)),
                node("n5", data("img.png", DataFormat::Image)),
                node("n6", file("out/copy.png", WriteMode::Create, true)),
                node("n7", file("both.txt", WriteMode::Create, true)),
                node("n8", data("empty.txt", DataFormat::Text)),
                node("n9", file("nothing.txt", WriteMode::Create, true)),
                node("n10", file("", WriteMode::Create, true)),
                node("n11", file("alone.txt", WriteMode::Create, true)),
            ],
            &[
                ("n0", "n1"),
                ("n0", "n2"),
                ("n0", "n3"),
                ("n0", "n4"),
                ("n5", "n6"),
                ("n0", "n7"),
                ("n5", "n7"),
                ("n8", "n9"),
                ("n0", "n10"),
            ],
        );
        let go = |id: &str| run(&doc, id, Dirs::under(&root), &http, &no_env);
        let real_root = root.canonicalize().unwrap();

        // Create: directories made on the way, fence stripped, newline added, text flows on.
        let out = go("n1").result.expect("create");
        let target = real_root.join("tests/gen/t.rs");
        assert_eq!(fs::read_to_string(&target).unwrap(), "fn t() {}\n");
        assert_eq!(out.as_text(), Some("fn t() {}\n"));
        assert_eq!(out.label, "Wrote tests/gen/t.rs · 10 B");
        assert_eq!(out.saved.as_deref(), Some(target.as_path()));
        let err = go("n1").result.unwrap_err();
        assert!(err.contains("exists"), "{err}");

        // Overwrite without unfencing keeps the text as it came.
        go("n2").result.expect("overwrite");
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "```rust\nfn t() {}\n```"
        );
        // Append adds to it.
        let report = go("n3");
        report.result.expect("append");
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "```rust\nfn t() {}\n```fn t() {}\n"
        );
        assert!(
            report.log.iter().any(|l| l.starts_with("appended to ")),
            "{:?}",
            report.log
        );

        // The app's directories are refused at validation and nothing is written.
        let err = go("n4").result.unwrap_err();
        assert!(err.contains("managed by the app"), "{err}");
        assert!(!root.join("scripts/planted.sh").exists());

        // A lone binary input is written byte for byte.
        let out = go("n6").result.expect("binary");
        assert!(!out.text);
        assert_eq!(
            fs::read(root.join("out/copy.png")).unwrap(),
            [0x89, b'P', b'N', b'G', 0, 0xff]
        );
        // Binary next to text can't be joined.
        let err = go("n7").result.unwrap_err();
        assert!(err.contains("binary output"), "{err}");
        assert!(!root.join("both.txt").exists());

        // Empty input, an empty path and no input at all are errors.
        let err = go("n9").result.unwrap_err();
        assert!(err.contains("nothing to write"), "{err}");
        let err = go("n10").result.unwrap_err();
        assert!(err.contains("not valid: path"), "{err}");
        let err = go("n11").result.unwrap_err();
        assert!(err.contains("nothing to write"), "{err}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn file_nodes_never_write_through_a_symlink_out_of_the_root() {
        let root = scratch("filelinks");
        fs::write(root.join("data/in.md"), "payload").unwrap();
        fs::create_dir_all(root.join("inside")).unwrap();
        // A directory outside the root, and symlinks into it: one directory deep, one to a
        // file that does not exist yet (so the "deepest existing ancestor" walk is what
        // has to catch it), and one that stays inside and must be allowed.
        let elsewhere = std::env::temp_dir().join(format!(
            "uapz-exec-outside-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&elsewhere).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&elsewhere, root.join("escape")).unwrap();
            std::os::unix::fs::symlink(elsewhere.join("new.txt"), root.join("escape-file.txt"))
                .unwrap();
            std::os::unix::fs::symlink(root.join("inside"), root.join("link-inside")).unwrap();
        }
        let http = FakeHttp::new(200, "{}");
        let file = |path: &str| {
            Some(NodeData::File(FileConfig {
                path: path.into(),
                mode: WriteMode::Overwrite,
                unfence: false,
                bundle: false,
                review: false,
            }))
        };
        let doc = graph(
            vec![
                node("n0", data("in.md", DataFormat::Markdown)),
                // Through a symlinked directory.
                node("n1", file("escape/planted.txt")),
                // Through a symlink to a file that is not there yet.
                node("n2", file("escape-file.txt")),
                // A symlink that resolves inside the root is a legitimate path.
                node("n3", file("link-inside/ok.txt")),
            ],
            &[("n0", "n1"), ("n0", "n2"), ("n0", "n3")],
        );
        let go = |id: &str| run(&doc, id, Dirs::under(&root), &http, &no_env);

        // A link into another directory is an escape.
        let err = go("n1").result.unwrap_err();
        assert!(err.contains("outside"), "{err}");
        // A link whose target is missing is refused too, and says so rather than leaking
        // a bare OS error; writing the target directly is what a user should do.
        let err = go("n2").result.unwrap_err();
        assert!(
            err.contains("symlink whose target does not exist"),
            "a dangling symlink is named as such: {err}"
        );
        assert!(
            !elsewhere.join("planted.txt").exists()
                && !elsewhere.join("new.txt").exists()
                && fs::read_dir(&elsewhere).unwrap().next().is_none(),
            "nothing was written outside the root"
        );

        go("n3").result.expect("a symlink inside the root is fine");
        assert_eq!(
            fs::read_to_string(root.join("inside/ok.txt")).unwrap(),
            "payload"
        );
        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&elsewhere).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn file_nodes_refuse_symlinks_into_protected_paths() {
        use std::os::unix::fs::symlink;

        let root = scratch("protected-links");
        fs::write(root.join("data/in.txt"), "payload").unwrap();
        fs::create_dir_all(root.join(".private")).unwrap();
        symlink(root.join("scripts"), root.join("alias")).unwrap();
        symlink(root.join(".private"), root.join("visible")).unwrap();
        fs::write(root.join("scripts/existing.sh"), "original").unwrap();
        symlink(root.join("scripts/existing.sh"), root.join("existing.txt")).unwrap();
        fs::create_dir_all(root.join("ordinary")).unwrap();
        symlink(root.join("ordinary"), root.join("prompts")).unwrap();
        let http = FakeHttp::new(200, "{}");
        for path in [
            "alias/new.sh",
            "visible/new.txt",
            "existing.txt",
            "ordinary/new.md",
        ] {
            for bundle in [false, true] {
                for mode in [WriteMode::Create, WriteMode::Overwrite, WriteMode::Append] {
                    fs::write(
                        root.join("data/in.txt"),
                        if bundle {
                            format!("=== {path} ===\npayload")
                        } else {
                            "payload".into()
                        },
                    )
                    .unwrap();
                    let doc = graph(
                        vec![
                            node("n0", data("in.txt", DataFormat::Text)),
                            node(
                                "n1",
                                Some(NodeData::File(FileConfig {
                                    path: if bundle { ".".into() } else { path.into() },
                                    mode,
                                    bundle,
                                    unfence: false,
                                    review: false,
                                })),
                            ),
                        ],
                        &[("n0", "n1")],
                    );
                    let report = run(&doc, "n1", Dirs::under(&root), &http, &no_env);
                    assert!(
                        report.result.is_err(),
                        "wrote through {path}, bundle={bundle}, mode={mode:?}"
                    );
                }
            }
        }
        assert!(!root.join("scripts/new.sh").exists());
        assert!(!root.join(".private/new.txt").exists());
        assert_eq!(
            fs::read_to_string(root.join("scripts/existing.sh")).unwrap(),
            "original"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn bundles_refuse_conflicting_resolved_destinations_before_writing() {
        let root = scratch("bundle-conflicts");
        fs::create_dir_all(root.join("out")).unwrap();
        std::os::unix::fs::symlink(root.join("out"), root.join("alias")).unwrap();
        let http = FakeHttp::new(200, "{}");
        let doc = graph(
            vec![
                node("n0", data("answer.txt", DataFormat::Text)),
                node(
                    "n1",
                    Some(NodeData::File(FileConfig {
                        path: ".".into(),
                        mode: WriteMode::Create,
                        bundle: true,
                        unfence: false,
                        review: false,
                    })),
                ),
            ],
            &[("n0", "n1")],
        );
        for paths in [
            ("out/file", "alias/file"),
            ("out/file", "out/file/child"),
            ("out/file/child", "out/file"),
        ] {
            fs::write(
                root.join("data/answer.txt"),
                format!("=== {} ===\nfirst\n=== {} ===\nsecond\n", paths.0, paths.1),
            )
            .unwrap();
            let report = run(&doc, "n1", Dirs::under(&root), &http, &no_env);
            assert!(report.result.unwrap_err().contains("conflicts"));
            assert!(fs::read_dir(root.join("out")).unwrap().next().is_none());
        }
        // Opening in Create mode enforces exclusivity even if the file appeared after
        // preflight. Append also remains append if it appeared during that interval.
        let target = root.join("out/file");
        fs::write(&target, b"original").unwrap();
        assert!(write_file_bytes(&target, b"replacement", WriteMode::Create).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"original");
        write_file_bytes(&target, b" appended", WriteMode::Append).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"original appended");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn process_timeout_covers_pipes_inherited_by_grandchildren() {
        let root = scratch("inherited-pipes");
        let start = Instant::now();
        let result = run_process(
            "sh",
            &["-c".into(), "sleep 2 & exit 0".into()],
            &root,
            &[],
            &[],
            Duration::from_millis(150),
            &AtomicBool::new(false),
        )
        .unwrap();
        let elapsed = start.elapsed();
        fs::remove_dir_all(root).unwrap();
        assert!(
            result.timed_out,
            "the parent exited but its descendant kept the pipes open for {elapsed:?}"
        );
        assert!(elapsed < Duration::from_secs(1), "timeout took {elapsed:?}");
    }

    #[test]
    fn cancellation_covers_pipes_inherited_by_grandchildren() {
        let root = scratch("cancel-inherited-pipes");
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            flag.store(true, Ordering::Relaxed);
        });
        let start = Instant::now();
        let result = run_process(
            "sh",
            &["-c".into(), "sleep 2 & exit 0".into()],
            &root,
            &[],
            &[],
            Duration::from_secs(10),
            &cancel,
        );
        let elapsed = start.elapsed();
        canceller.join().unwrap();
        assert!(matches!(result, Err(ref e) if e == CANCELLED));
        assert!(elapsed < Duration::from_secs(1), "cancel took {elapsed:?}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bundle_sections_cannot_escape_the_directory_or_plant_app_files() {
        let root = scratch("bundlepaths");
        fs::write(
            root.join("data/answer.md"),
            concat!(
                "=== ../../etc/evil ===\nno\n",
                "=== /etc/evil ===\nno\n",
                "=== .hidden ===\nno\n",
                "=== ok.txt ===\nyes\n",
            ),
        )
        .unwrap();
        let http = FakeHttp::new(200, "{}");
        let bundle = |path: &str| {
            Some(NodeData::File(FileConfig {
                path: path.into(),
                mode: WriteMode::Overwrite,
                unfence: true,
                bundle: true,
                review: false,
            }))
        };
        let doc = graph(
            vec![
                node("n0", data("answer.md", DataFormat::Markdown)),
                node("n1", bundle("out")),
            ],
            &[("n0", "n1")],
        );
        let err = run(&doc, "n1", Dirs::under(&root), &http, &no_env)
            .result
            .unwrap_err();
        assert!(err.contains(".."), "{err}");
        // Every section is checked before the first file is written.
        assert!(
            !root.join("out").exists(),
            "a refused bundle left files behind"
        );

        // Each refusal on its own, so one bad section can't mask another's message.
        for (body, needle) in [
            ("=== /etc/evil ===\nno\n", "absolute"),
            ("=== .hidden ===\nno\n", "hidden"),
            ("=== a/../../b ===\nno\n", ".."),
        ] {
            fs::write(root.join("data/one.md"), body).unwrap();
            let doc = graph(
                vec![
                    node("n0", data("one.md", DataFormat::Markdown)),
                    node("n1", bundle("out")),
                ],
                &[("n0", "n1")],
            );
            let err = run(&doc, "n1", Dirs::under(&root), &http, &no_env)
                .result
                .unwrap_err();
            assert!(err.contains(needle), "{body:?}: {err}");
        }

        // The path from the working directory is what decides whether an app directory is
        // involved, so a section under a bundle root may name one: `out/scripts/` is not
        // the app's `scripts/` and is written.
        fs::write(root.join("data/nested.md"), "=== scripts/x.sh ===\nno\n").unwrap();
        let doc_nested = graph(
            vec![
                node("n0", data("nested.md", DataFormat::Markdown)),
                node("n1", bundle("out")),
            ],
            &[("n0", "n1")],
        );
        run(&doc_nested, "n1", Dirs::under(&root), &http, &no_env)
            .result
            .expect("out/scripts is not the app's scripts/");
        assert_eq!(
            fs::read_to_string(root.join("out/scripts/x.sh")).unwrap(),
            "no\n"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn writable_sqlite_nodes_may_create_the_database() {
        require("sqlite3");
        let root = scratch("newdb");
        let http = FakeHttp::new(200, "{}");
        let db = |path: &str, query: &str, read_only: bool| {
            Some(NodeData::Database(DatabaseConfig {
                path: path.into(),
                query: query.into(),
                read_only,
                ..DatabaseConfig::default()
            }))
        };
        let doc = graph(
            vec![
                node("n0", db("fresh.sqlite", "CREATE TABLE t(a INTEGER)", false)),
                node(
                    "n1",
                    db("fresh.sqlite", "SELECT count(*) AS n FROM t", true),
                ),
                node("n2", db("missing/x.sqlite", "CREATE TABLE t(a)", false)),
                node("n3", db("absent.sqlite", "SELECT 1", true)),
            ],
            &[],
        );
        let go = |id: &str| run(&doc, id, Dirs::under(&root), &http, &no_env);
        let config = DatabaseConfig {
            path: "fresh.sqlite".into(),
            read_only: false,
            ..DatabaseConfig::default()
        };
        assert_eq!(
            describe_schema(&config, &Dirs::under(&root), &no_env).unwrap(),
            "(new database, no tables yet)"
        );
        go("n0").result.expect("creates the file");
        assert!(root.join("data/fresh.sqlite").is_file());
        let out = go("n1").result.expect("reads it back");
        assert_eq!(out.as_text().map(str::trim), Some(r#"[{"n":0}]"#));
        assert!(
            describe_schema(&config, &Dirs::under(&root), &no_env)
                .unwrap()
                .contains("CREATE TABLE t"),
        );
        let err = go("n2").result.unwrap_err();
        assert!(err.contains("does not exist"), "{err}");
        assert!(!root.join("data/missing").exists());
        let err = go("n3").result.unwrap_err();
        assert!(err.contains("absent.sqlite"), "{err}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[ignore = "needs a live model server; see the comment above"]
    fn live_agent_streams_its_answer_in_pieces() {
        let mut agent = live_agent("UAPZ_LIVE_MODEL", ModelType::Generative, OutputType::Text);
        agent.prompt = "Count from one to twenty in words, separated by commas.".into();
        let http = providers::UreqHttp::new(HTTP_TIMEOUT);
        let mut pieces: Vec<String> = Vec::new();
        let out = providers::generate_streaming(&http, &agent, None, "", &mut |text| {
            pieces.push(text.to_string());
            true
        })
        .expect("live stream");
        let text = String::from_utf8(out.bytes).unwrap();
        println!("{} pieces: {text}", pieces.len());
        assert!(pieces.len() >= 2, "{pieces:?}");
        assert_eq!(pieces.concat(), text);
        assert!(text.to_lowercase().contains("twenty"), "{text}");

        // Stopping after the first piece ends the request.
        let mut count = 0;
        let err = providers::generate_streaming(&http, &agent, None, "", &mut |_| {
            count += 1;
            false
        })
        .unwrap_err();
        assert_eq!(err, providers::STOPPED);
        assert_eq!(count, 1);
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

        // The same answer lands in the repository through a write-file node, and a second
        // run with a cache does not call the model again.
        let mut doc = doc;
        doc.nodes.push(node(
            "n2",
            Some(NodeData::File(FileConfig {
                path: "tests/generated.rs".into(),
                mode: WriteMode::Overwrite,
                unfence: true,
                bundle: false,
                review: false,
            })),
        ));
        doc.edges.push(EdgeDoc {
            output: None,
            from: "n1".into(),
            to: "n2".into(),
        });
        let (control, _) = recording_control();
        let http = providers::UreqHttp::new(HTTP_TIMEOUT);
        let first = run_with(
            &doc,
            "n2",
            Dirs::under(&root),
            &http,
            &process_env,
            &control,
        );
        first.result.expect("directory → agent → file");
        let written = fs::read_to_string(root.join("tests/generated.rs")).unwrap();
        assert!(written.contains("#[test]"), "{written}");
        assert!(!written.starts_with("```"), "{written}");
        let second = run_with(
            &doc,
            "n2",
            Dirs::under(&root),
            &http,
            &process_env,
            &control,
        );
        second.result.expect("second run");
        assert_eq!(second.reused, ["n1"], "{:?}", second.log);
        fs::remove_dir_all(&root).unwrap();
    }
}
