//! Thread-safe application boundary shared by desktop clients and tests.
//!
//! Library helpers retain their established workspace-relative paths. The desktop
//! host selects the workspace as its working directory once, before constructing
//! this service. Execution, cache and history always use the captured explicit root.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::{
    calls,
    doc::{self, GraphDoc},
    exec::{self, Cache, Control, Progress},
    history, library,
    node_data::{
        AgentConfig, Auth, DataFormat, DatabaseConfig, DbEngine, ModelType, NodeData, NodeType,
        OutputType, Provider, Runtime, WriteMode,
    },
    providers::{self, UreqHttp},
    review, storage, validate,
    workflow::{self, InputType},
};

const MAX_LIVE_TEXT: usize = 256 * 1024;
const MAX_LIVE_LOG: usize = 512 * 1024;
const MAX_FEED_EVENTS: usize = 2048;
const MAX_FEED_BYTES: usize = 2 * 1024 * 1024;
const MAX_LIVE_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
/// Model listings are short requests; the executor timeout is for generation.
const LISTING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

#[derive(Serialize)]
struct FeedEvent {
    seq: u64,
    #[serde(flatten)]
    body: FeedBody,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum FeedBody {
    Log {
        text: String,
        #[serde(skip_serializing_if = "is_false")]
        truncated: bool,
    },
    Node {
        id: String,
        state: &'static str,
    },
    Text {
        id: String,
        text: String,
        #[serde(skip_serializing_if = "is_false")]
        truncated: bool,
    },
    Output {
        id: String,
        result: Box<history::NodeRun>,
    },
}

struct StoredEvent {
    event: FeedEvent,
    bytes: usize,
}

struct LiveOutput {
    result: history::NodeRun,
    bytes: usize,
}

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A single workspace's cache and execution session. Closing or replacing a graph
/// in the frontend cannot discard the worker or its pending review.
#[derive(Clone)]
pub struct AppService {
    root: PathBuf,
    cache: Arc<Mutex<Cache>>,
    current: Arc<Mutex<Option<Arc<Mutex<Session>>>>>,
    next_id: Arc<AtomicU64>,
    _lifetime: Arc<ServiceLifetime>,
}

struct ServiceLifetime {
    current: Arc<Mutex<Option<Arc<Mutex<Session>>>>>,
}

impl Drop for ServiceLifetime {
    fn drop(&mut self) {
        if let Some(session) = lock(&self.current).as_ref() {
            lock(session).cancel();
        }
    }
}

struct Session {
    id: String,
    graph: GraphDoc,
    targets: Vec<String>,
    cancel: Arc<AtomicBool>,
    running: bool,
    marks: BTreeMap<String, &'static str>,
    log: Vec<String>,
    log_bytes: usize,
    stream: Option<(String, String)>,
    workflows: BTreeMap<String, GraphDoc>,
    review: Option<(String, review::Pending)>,
    review_seq: u64,
    run: Option<history::Run>,
    error: Option<String>,
    events: VecDeque<StoredEvent>,
    event_bytes: usize,
    next_seq: u64,
    events_dropped: u64,
    outputs: BTreeMap<String, LiveOutput>,
    output_order: VecDeque<String>,
    output_bytes: usize,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Session {
    fn event(&mut self, body: FeedBody) {
        let event = FeedEvent {
            seq: self.next_seq,
            body,
        };
        self.next_seq += 1;
        let bytes = serde_json::to_vec(&event).map_or(MAX_FEED_BYTES + 1, |bytes| bytes.len());
        if bytes > MAX_FEED_BYTES {
            // A single oversized item must not evict the useful retained feed.
            self.events_dropped += 1;
            return;
        }
        self.event_bytes += bytes;
        self.events.push_back(StoredEvent { event, bytes });
        self.prune_events();
    }

    fn prune_events(&mut self) {
        while self.events.len() > MAX_FEED_EVENTS || self.event_bytes > MAX_FEED_BYTES {
            let Some(event) = self.events.pop_front() else {
                break;
            };
            self.event_bytes -= event.bytes;
            self.events_dropped += 1;
        }
    }

    fn text_event(&mut self, id: &str, text: &str) {
        if let Some(last) = self.events.back_mut()
            && let FeedBody::Text {
                id: stream_id,
                text: stream,
                truncated,
            } = &mut last.event.body
            && stream_id == id
        {
            stream.push_str(text);
            *truncated |= stream.len() > MAX_LIVE_TEXT;
            *stream = tail(stream, MAX_LIVE_TEXT);
            self.event_bytes -= last.bytes;
            last.bytes =
                serde_json::to_vec(&last.event).map_or(MAX_FEED_BYTES + 1, |bytes| bytes.len());
            self.event_bytes += last.bytes;
            self.prune_events();
        } else {
            self.event(FeedBody::Text {
                id: id.into(),
                text: tail(text, MAX_LIVE_TEXT),
                truncated: text.len() > MAX_LIVE_TEXT,
            });
        }
    }

    fn completed_output(&mut self, id: String, mut result: Box<history::NodeRun>) {
        // Configuration remains in the captured graph and final run. Keeping it
        // out of live output events avoids repeating large prompts in every poll.
        result.data = None;
        let bytes =
            serde_json::to_vec(&result).map_or(MAX_LIVE_OUTPUT_BYTES + 1, |bytes| bytes.len());
        if let Some(old) = self.outputs.remove(&id) {
            self.output_bytes -= old.bytes;
            self.output_order.retain(|old| old != &id);
        }
        self.output_bytes += bytes;
        self.outputs.insert(
            id.clone(),
            LiveOutput {
                result: (*result).clone(),
                bytes,
            },
        );
        self.output_order.push_back(id.clone());
        while self.outputs.len() > MAX_FEED_EVENTS || self.output_bytes > MAX_LIVE_OUTPUT_BYTES {
            let Some(old) = self.output_order.pop_front() else {
                break;
            };
            if let Some(output) = self.outputs.remove(&old) {
                self.output_bytes -= output.bytes;
            }
        }
        // Completed output replaces streamed partial text, including earlier
        // chunks separated by logs. Replacement is not retention loss.
        self.events.retain(|event| {
            if matches!(&event.event.body, FeedBody::Text {id:stream_id,..} if stream_id == &id) {
                self.event_bytes -= event.bytes;
                false
            } else {
                true
            }
        });
        if self
            .stream
            .as_ref()
            .is_some_and(|(stream_id, _)| stream_id == &id)
        {
            self.stream = None;
        }
        self.event(FeedBody::Output { id, result });
    }

    fn cancel(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some((_, pending)) = self.review.take() {
            // Wake the receiver with a value before dropping the final sender. The
            // review waiter rechecks cancellation on receipt; disconnect alone is
            // a different (failed review) outcome in the domain API.
            pending.decide(review::Decision::Reject);
        }
    }

    fn progress(&mut self, progress: Progress) {
        match progress {
            Progress::Workflow { id, graph } => {
                self.workflows.insert(id, (*graph).clone());
            }
            Progress::Node { id, state } => {
                let state = match state {
                    exec::NodeState::Running => "running",
                    exec::NodeState::Done => "ran",
                    exec::NodeState::Reused => "reused",
                    exec::NodeState::Failed => "failed",
                };
                self.event(FeedBody::Node {
                    id: id.clone(),
                    state,
                });
                self.marks.insert(id, state);
            }
            Progress::Log(line) => {
                self.event(FeedBody::Log {
                    text: tail(&line, MAX_LIVE_TEXT),
                    truncated: line.len() > MAX_LIVE_TEXT,
                });
                let line = tail(&line, MAX_LIVE_TEXT);
                self.log_bytes += line.len();
                self.log.push(line);
                while self.log_bytes > MAX_LIVE_LOG && self.log.len() > 1 {
                    self.log_bytes -= self.log.remove(0).len();
                }
            }
            Progress::Text { id, text } => {
                self.text_event(&id, &text);
                let stream = self
                    .stream
                    .get_or_insert_with(|| (id.clone(), String::new()));
                if stream.0 != id {
                    *stream = (id, String::new());
                }
                stream.1.push_str(&text);
                stream.1 = tail(&stream.1, MAX_LIVE_TEXT);
            }
            Progress::Output { id, result } => self.completed_output(id, result),
        }
    }

    fn value(&self) -> Value {
        let pending = self.review.as_ref().map(|(id, pending)| json!({
            "id":id,"node":pending.request.node,
            "files":pending.request.files.iter().map(|file| json!({
                "path":file.path,"before_bytes":file.before_bytes,"after_bytes":file.after_bytes,"diff":file.diff
            })).collect::<Vec<_>>()
        }));
        json!({"run_id":self.id,"running":self.running,
            "cancelling":self.running && self.cancel.load(Ordering::Relaxed),
            "graph":self.graph,"targets":self.targets,"marks":self.marks,"log":self.log,
            "stream":self.stream.as_ref().map(|(id,text)| json!({"id":id,"text":text})),
            "workflows":self.workflows,"review":pending,"run":self.run,"error":self.error,
            "events":self.events.iter().map(|stored|&stored.event).collect::<Vec<_>>(),"events_dropped":self.events_dropped})
    }
}

impl AppService {
    pub fn new(root: PathBuf) -> Self {
        let root = root.canonicalize().unwrap_or(root);
        let current = Arc::new(Mutex::new(None));
        Self {
            cache: Arc::new(Mutex::new(Cache::persistent(root.join(library::CACHE_DIR)))),
            root,
            current: current.clone(),
            next_id: Arc::new(AtomicU64::new(1)),
            _lifetime: Arc::new(ServiceLifetime { current }),
        }
    }

    /// Wake a blocked file review and request termination of the active worker.
    pub fn cancel_active(&self) {
        if let Some(session) = lock(&self.current).as_ref() {
            let mut session = lock(session);
            session.cancel();
        }
    }

    /// Dispatch a desktop request. All mutation payloads are decoded and validated
    /// here; a renderer never obtains filesystem or process execution primitives.
    pub fn request(&self, method: &str, args: Value) -> Result<Value, String> {
        match method {
            "bootstrap" => Ok(self.bootstrap()),
            "load_graph" => value(doc::load_under(&self.root, string(&args, "name")?)?),
            "save_graph" => {
                let graph: GraphDoc = field(&args, "doc")?;
                structural(&graph)?;
                let (markdown, json) = doc::save(&graph)?;
                Ok(json!({"name":graph.name,"markdown":markdown,"json":json}))
            }
            "save_kind" => {
                let kind: doc::NodeKind = field(&args, "kind")?;
                if doc::hex_to_rgb(&kind.color).is_none() {
                    return Err("choose a #rrggbb color".into());
                }
                value(doc::save_kind(&kind)?)
            }
            "delete_kind" => {
                doc::delete_kind(string(&args, "name")?)?;
                Ok(Value::Null)
            }
            "save_preset" => {
                safe_name(string(&args, "name")?)?;
                let path = match string(&args, "type")? {
                    "agent" => {
                        library::save_agent(string(&args, "name")?, &field(&args, "config")?)?
                    }
                    "database" => {
                        library::save_database(string(&args, "name")?, &field(&args, "config")?)?
                    }
                    _ => return Err("unknown preset type".into()),
                };
                value(path)
            }
            "delete_preset" => {
                let name = string(&args, "name")?;
                safe_name(name)?;
                match string(&args, "type")? {
                    "agent" => library::delete_agent(name)?,
                    "database" => library::delete_database(name)?,
                    _ => return Err("unknown preset type".into()),
                };
                Ok(Value::Null)
            }
            "save_prompt" => {
                let name = string(&args, "name")?;
                safe_name(name)?;
                value(library::save_prompt(name, string(&args, "text")?)?)
            }
            "load_prompt" => {
                let name = string(&args, "name")?;
                safe_name(name)?;
                value(library::load_prompt(name)?)
            }
            "delete_prompt" => {
                let name = string(&args, "name")?;
                safe_name(name)?;
                library::delete_prompt(name)?;
                Ok(Value::Null)
            }
            "validate_node" => {
                let data: NodeData = field(&args, "data")?;
                let mut issues = issues(&data, None);
                if matches!(data, NodeData::Input(_))
                    && args
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .is_empty()
                {
                    issues.push(json!({"field":"name","message":"Name this Run input so it can appear in the Run form"}));
                }
                if let Some(name) = args
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
                    && let Err(error) = validate::node_name(name)
                {
                    issues.push(json!({"field":"name","message":error}));
                }
                Ok(Value::Array(issues))
            }
            "validate_graph" => {
                let graph: GraphDoc = field(&args, "doc")?;
                let targets = exec::sinks(&graph);
                let mut found = Vec::new();
                if let Err(error) = structural(&graph) {
                    found.push(json!({"field":"graph","message":error}));
                }
                for node in &graph.nodes {
                    if let Some(data) = &node.data {
                        found.extend(issues(data, Some(&node.id)));
                    }
                }
                let fields = match workflow::fields(&graph, &targets) {
                    Ok(fields) => fields,
                    Err(error) => {
                        found.push(json!({"field":"inputs","message":error}));
                        Vec::new()
                    }
                };
                let cancel = AtomicBool::new(false);
                let plans = calls::prepare(&graph, &targets, &self.root, &cancel);
                for (id, plan) in &plans {
                    if let Err(error) = plan {
                        found.push(json!({"node":id,"field":"workflow","message":error}));
                    }
                }
                for target in &targets {
                    if let Err(error) = exec::preflight(&graph, target, &cancel) {
                        found.push(json!({"node":target,"field":"graph","message":error}));
                    }
                }
                if let Err(error) = exec::preflight_wires(&graph, &targets, &plans) {
                    found.push(json!({"field":"wires","message":error}));
                }
                if let Err(error) = exec::preflight_publications(&graph, &plans) {
                    found.push(json!({"field":"outputs","message":error}));
                }
                found.extend(wiring_issues(&graph, &plans));
                Ok(json!({"issues":found,"fields":fields_value(&fields),"sinks":targets}))
            }
            "workflow_fields" => {
                let graph: GraphDoc = field(&args, "doc")?;
                let targets = interface_targets(&args, &graph)?;
                let cancel = AtomicBool::new(false);
                for target in &targets {
                    exec::preflight(&graph, target, &cancel)?;
                }
                let plans = calls::prepare(&graph, &targets, &self.root, &cancel);
                for (id, plan) in &plans {
                    if let Err(error) = plan {
                        return Err(format!("{id}: {error}"));
                    }
                }
                exec::preflight_wires(&graph, &targets, &plans)?;
                if args.get("targets").is_none_or(Value::is_null) && !graph.outputs.is_empty() {
                    exec::preflight_publications(&graph, &plans)?;
                }
                Ok(fields_value(&workflow::fields(&graph, &targets)?))
            }
            "prepare_run" => {
                let graph: GraphDoc = field(&args, "doc")?;
                structural(&graph)?;
                let targets = targets(&args, &graph)?;
                let fields = workflow::fields(&graph, &targets)?;
                let supplied: workflow::Values = optional(&args, "inputs")?.unwrap_or_default();
                // A form may contain intentionally empty required inputs. Preparation
                // describes it; execution resolves and checks every supplied value.
                let inputs = fields
                    .iter()
                    .map(|f| {
                        (
                            f.name.clone(),
                            supplied.get(&f.name).unwrap_or(&f.config.default).clone(),
                        )
                    })
                    .collect::<workflow::Values>();
                Ok(json!({"targets":targets,"fields":fields_value(&fields),"inputs":inputs}))
            }
            "start_run" => self.start(&args),
            "runtime_state" => Ok(lock(&self.current)
                .as_ref()
                .map_or(Value::Null, |s| lock(s).value())),
            "cancel_run" => {
                let session = self.session(string(&args, "run_id")?)?;
                let mut session = lock(&session);
                if !session.running {
                    return Err("this run has already finished".into());
                }
                session.cancel();
                Ok(Value::Null)
            }
            "review" => {
                let decision = match string(&args, "decision")? {
                    "apply" => review::Decision::Apply,
                    "reject" => review::Decision::Reject,
                    _ => return Err("unknown review decision".into()),
                };
                let session = self.session(string(&args, "run_id")?)?;
                let mut session = lock(&session);
                if !session.running || session.cancel.load(Ordering::Relaxed) {
                    return Err("this run is no longer awaiting review".into());
                }
                let (id, pending) = session
                    .review
                    .as_ref()
                    .ok_or("there is no pending review")?;
                if id != string(&args, "review_id")? {
                    return Err("this review is stale".into());
                }
                if !pending.decide(decision) {
                    return Err("this review has already been answered".into());
                }
                session.review = None;
                Ok(Value::Null)
            }
            "history" => {
                let graph = history_graph(string(&args, "graph")?)?;
                value(history::list(&self.root, &graph))
            }
            "output" => self.output(&args),
            "clear_history" => {
                let _current = self.idle()?;
                let graph = history_graph(string(&args, "graph")?)?;
                value(history::clear(&self.root, &graph))
            }
            "clear_cache" => {
                let _current = self.idle()?;
                let mut cache = lock(&self.cache);
                let count = cache.len();
                cache.clear();
                value(count)
            }
            "describe_schema" => {
                let config: DatabaseConfig = field(&args, "config")?;
                value(exec::describe_schema(
                    &config,
                    &exec::Dirs::under(&self.root),
                    &exec::process_env,
                )?)
            }
            "write_sql" => {
                let config: DatabaseConfig = field(&args, "config")?;
                let agent: AgentConfig = field(&args, "agent")?;
                value(exec::write_sql(
                    &agent,
                    &config,
                    string(&args, "question")?,
                    &exec::Dirs::under(&self.root),
                    &UreqHttp::new(exec::HTTP_TIMEOUT),
                    &exec::process_env,
                )?)
            }
            "list_models" => {
                let config: AgentConfig = field(&args, "config")?;
                let key = match &config.auth {
                    Auth::Env { var } => Some(
                        exec::process_env(var)
                            .filter(|value| !value.trim().is_empty())
                            .ok_or_else(|| format!("environment variable {var} is not set"))?,
                    ),
                    Auth::None => None,
                };
                value(providers::list_models(
                    &UreqHttp::new(LISTING_TIMEOUT),
                    &config,
                    key.as_deref(),
                )?)
            }
            "env_var_set" => {
                let name = string(&args, "name")?;
                validate::env_var(name)?;
                // Only presence is reported; the value never crosses this boundary.
                Ok(Value::Bool(
                    exec::process_env(name.trim()).is_some_and(|value| !value.trim().is_empty()),
                ))
            }
            "delete_graph" => {
                let _current = self.idle()?;
                let name = string(&args, "name")?;
                doc::delete(name)?;
                history::clear(&self.root, &history_graph(name)?);
                Ok(Value::Null)
            }
            "rename_graph" => {
                let _current = self.idle()?;
                let from = string(&args, "from")?;
                let to = doc::sanitize_name(string(&args, "to")?);
                doc::rename(from, &to)?;
                history::rename(&self.root, &history_graph(from)?, &history_graph(&to)?)?;
                Ok(json!({"name": to}))
            }
            "duplicate_graph" => {
                let from = string(&args, "from")?;
                let to = doc::sanitize_name(string(&args, "to")?);
                doc::duplicate(from, &to)?;
                Ok(json!({"name": to}))
            }
            "reveal" => {
                let path = validate::resolve_absolute_inside(&self.root, string(&args, "path")?)?;
                reveal(&path)?;
                Ok(Value::Null)
            }
            "install_examples" => {
                let (installed, skipped) = crate::examples::install(&self.root)?;
                Ok(json!({"installed": installed, "skipped": skipped}))
            }
            _ => Err(format!("unknown application request: {method}")),
        }
    }

    fn idle(&self) -> Result<MutexGuard<'_, Option<Arc<Mutex<Session>>>>, String> {
        let current = lock(&self.current);
        if current.as_ref().is_some_and(|s| lock(s).running) {
            Err("wait for the active run to finish".into())
        } else {
            Ok(current)
        }
    }

    fn output(&self, args: &Value) -> Result<Value, String> {
        use base64::Engine as _;
        let max_bytes: Option<usize> = args
            .get("max_bytes")
            .map(|_| field(args, "max_bytes"))
            .transpose()?;
        if max_bytes.is_some_and(|limit| limit == 0 || limit > exec::MAX_OUTPUT_BYTES) {
            return Err(format!(
                "max_bytes must be an integer between 1 and {}",
                exec::MAX_OUTPUT_BYTES
            ));
        }
        let id = string(args, "run_id")?;
        let graph = history_graph(string(args, "graph")?)?;
        let node = string(args, "node")?;
        validate::text("node", node, validate::MAX_PATH_LEN)?;
        let path: Option<String> = optional(args, "call_path")?;
        if let Some(path) = &path {
            validate::text("workflow path", path, validate::MAX_PATH_LEN)?;
        }
        let handle = path
            .filter(|path| !path.is_empty())
            .map_or_else(|| node.to_string(), |path| format!("{path}/{node}"));
        let parts: Vec<_> = handle.split('/').collect();
        if parts.len() > calls::MAX_DEPTH + 1
            || parts.iter().any(|part| !validate::is_node_id(part))
        {
            return Err("choose a node id and a valid workflow path".into());
        }
        let live = lock(&self.current).clone();
        let (node, live_run) = live
            .as_ref()
            .map(|session| {
                let session = lock(session);
                if history::graph_name(&session.graph.name) != graph {
                    return (None, None);
                }
                let matching =
                    session.id == id || session.run.as_ref().is_some_and(|run| run.id == id);
                if !matching {
                    return (None, None);
                }
                (
                    session
                        .outputs
                        .get(&handle)
                        .map(|output| output.result.clone()),
                    session.run.clone(),
                )
            })
            .unwrap_or_default();
        let node = match node {
            Some(node) => node,
            None => {
                let run = live_run
                    .or_else(|| {
                        history::list(&self.root, &graph)
                            .into_iter()
                            .find(|run| run.id == id)
                    })
                    .ok_or("the run or completed live output is no longer stored")?;
                let mut run = &run;
                for id in &parts[..parts.len() - 1] {
                    run = &run.calls.get(*id).ok_or("this workflow did not run")?.run;
                }
                run.nodes
                    .get(parts[parts.len() - 1])
                    .cloned()
                    .ok_or("this node has no result in the run")?
            }
        };
        let output: Option<String> = optional(args, "output")?;
        let node = if let Some(output) = &output {
            validate::output_name(output)?;
            node.named_outputs
                .get(output)
                .ok_or("this output is not available")?
        } else {
            &node
        };
        let cached = node
            .content_key()
            .and_then(|key| lock(&self.cache).fetch(key));
        let complete = cached.is_some() || node.complete;
        let out = cached.or_else(|| node.output(None)).ok_or_else(|| {
            node.error
                .clone()
                .unwrap_or_else(|| "this output is no longer stored".into())
        })?;
        let bytes = out.bytes.len();
        let limited = max_bytes.is_some_and(|limit| bytes > limit);
        let text = out
            .as_text()
            .map(|text| max_bytes.map_or(text, |limit| head(text, limit)));
        Ok(json!({"label":out.label,"saved":out.saved,"text":text,
            "media_type":(!out.text).then(||media_type(&out.bytes)),
            "base64":if out.text || limited {None} else {Some(base64::engine::general_purpose::STANDARD.encode(&out.bytes))},
            "complete":complete && !limited,"limited":limited,"bytes":bytes}))
    }

    fn session(&self, id: &str) -> Result<Arc<Mutex<Session>>, String> {
        lock(&self.current)
            .as_ref()
            .filter(|session| lock(session).id == id)
            .cloned()
            .ok_or_else(|| "this run is stale".into())
    }

    fn start(&self, args: &Value) -> Result<Value, String> {
        let graph: GraphDoc = field(args, "doc")?;
        structural(&graph)?;
        let targets = targets(args, &graph)?;
        let fields = workflow::fields(&graph, &targets)?;
        let supplied: workflow::Values = optional(args, "inputs")?.unwrap_or_default();
        let inputs = workflow::resolve(&fields, &supplied, &self.root.join(library::DATA_DIR))?;
        let fresh = optional(args, "fresh")?.unwrap_or(false);
        let mut current = lock(&self.current);
        if current.as_ref().is_some_and(|s| lock(s).running) {
            return Err("a run is already active".into());
        }
        let id = format!(
            "{}-{}",
            library::timestamp(),
            self.next_id.fetch_add(1, Ordering::Relaxed)
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let session = Arc::new(Mutex::new(Session {
            id: id.clone(),
            graph: graph.clone(),
            targets: targets.clone(),
            cancel: cancel.clone(),
            running: true,
            marks: members(&graph, &targets)
                .into_iter()
                .map(|id| (id, "pending"))
                .collect(),
            log: Vec::new(),
            log_bytes: 0,
            stream: None,
            workflows: BTreeMap::new(),
            review: None,
            review_seq: 0,
            run: None,
            error: None,
            events: VecDeque::new(),
            event_bytes: 0,
            next_seq: 1,
            events_dropped: 0,
            outputs: BTreeMap::new(),
            output_order: VecDeque::new(),
            output_bytes: 0,
        }));
        let progress_session = session.clone();
        let review_session = session.clone();
        let review_cancel = cancel.clone();
        let control = Control {
            inputs,
            cache: Some(self.cache.clone()),
            fresh,
            cancel,
            progress: Some(Arc::new(move |p| lock(&progress_session).progress(p))),
            review: Some(Arc::new(move |request| {
                let result = review::wait(request, &review_cancel, |pending| {
                    let mut state = lock(&review_session);
                    state.review_seq += 1;
                    let id = format!("{}-review-{}", state.id, state.review_seq);
                    state.review = Some((id, pending));
                });
                lock(&review_session).review = None;
                result
            })),
        };
        *current = Some(session.clone());
        let root = self.root.clone();
        let spawn = std::thread::Builder::new()
            .name(format!("uapz-run-{id}"))
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let http = UreqHttp::new(exec::HTTP_TIMEOUT);
                    let report = exec::run_all_with(
                        &graph,
                        &targets,
                        exec::Dirs::under(&root),
                        &http,
                        &exec::process_env,
                        &control,
                    );
                    let mut run =
                        history::record(&graph, &targets, &report, control.cache.as_deref());
                    let save_error = history::save(&root, &run).err();
                    if let Some(error) = &save_error {
                        run.log
                            .push(format!("The run record was not saved: {error}"));
                    }
                    let mut state = lock(&session);
                    state
                        .marks
                        .retain(|_, state| *state != "pending" && *state != "running");
                    state.log = run
                        .log
                        .iter()
                        .rev()
                        .scan(0usize, |bytes, line| {
                            if *bytes >= MAX_LIVE_LOG {
                                None
                            } else {
                                let line = tail(line, MAX_LIVE_TEXT.min(MAX_LIVE_LOG - *bytes));
                                *bytes += line.len();
                                Some(line)
                            }
                        })
                        .collect();
                    state.log.reverse();
                    state.log_bytes = state.log.iter().map(String::len).sum();
                    state.workflows = run.workflows.clone();
                    state.run = Some(run);
                    state.error = save_error;
                }));
                let mut state = lock(&session);
                state.running = false;
                state.review = None;
                state.stream = None;
                if result.is_err() {
                    state.error = Some("the execution worker stopped unexpectedly".into());
                }
            });
        if let Err(error) = spawn {
            *current = None;
            return Err(format!("start execution worker: {error}"));
        }
        Ok(json!({"run_id":id}))
    }

    fn bootstrap(&self) -> Value {
        let agents = library::list_agents()
            .into_iter()
            .map(|p| json!({"name":p.name,"config":p.config}))
            .collect::<Vec<_>>();
        let databases = library::list_databases()
            .into_iter()
            .map(|p| json!({"name":p.name,"config":p.config}))
            .collect::<Vec<_>>();
        json!({"workspace":self.root,"graphs":doc::list(),"kinds":doc::list_kinds(),"agents":agents,"databases":databases,
            "prompts":library::list_prompts(),"scripts":library::list_scripts(),"data":library::list_data(),"catalog":{
                "nodes":NodeType::ALL.map(|t|json!({"type":t,"label":t.label(),"color":t.color(),"data":t.default_data()})),
                "providers":Provider::ALL.map(|v|json!({"value":v,"label":v.label(),"endpoint":v.default_endpoint(),"model":v.default_model(),"env_var":v.default_env_var()})),
                "model_types":ModelType::ALL.map(|v|json!({"value":v,"label":v.label()})),
                "outputs":OutputType::ALL.map(|v|json!({"value":v,"label":v.label()})),
                "engines":DbEngine::ALL.map(|v|json!({"value":v,"label":v.label()})),
                "runtimes":Runtime::ALL.map(|v|json!({"value":v,"label":v.label()})),
                "formats":DataFormat::ALL.map(|v|json!({"value":v,"label":v.label()})),
                "write_modes":WriteMode::ALL.map(|v|json!({"value":v,"label":v.label()})),
                "input_types":InputType::ALL.map(|v|json!({"value":v,"label":v.label()}))}})
    }
}

/// Show a workspace file in the platform's file manager. Callers resolve the path
/// inside the workspace first, so nothing the renderer names can leave it.
fn reveal(path: &std::path::Path) -> Result<(), String> {
    use std::process::{Command, Stdio};
    if !path.exists() {
        return Err(format!("{} no longer exists", path.display()));
    }
    // Paths travel as OS strings, never formatted into one: a non-UTF-8 name survives, and
    // on Windows a name carrying `,` stays part of `/select,` rather than adding switches.
    let (program, args): (&str, Vec<std::ffi::OsString>) = if cfg!(target_os = "macos") {
        ("open", vec!["-R".into(), path.as_os_str().to_owned()])
    } else if cfg!(target_os = "windows") {
        let mut select = std::ffi::OsString::from("/select,");
        select.push(path.as_os_str());
        ("explorer", vec![select])
    } else {
        (
            "xdg-open",
            vec![path.parent().unwrap_or(path).as_os_str().to_owned()],
        )
    };
    // `open -R` and `explorer /select,` report a bad destination only after launch, so a
    // stale path has to be caught before spawning.
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not open the file manager: {e}"))?;
    Ok(())
}

fn value(value: impl serde::Serialize) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|e| e.to_string())
}
fn is_false(value: &bool) -> bool {
    !value
}
fn field<T: DeserializeOwned>(args: &Value, name: &str) -> Result<T, String> {
    serde_json::from_value(
        args.get(name)
            .cloned()
            .ok_or_else(|| format!("missing {name}"))?,
    )
    .map_err(|e| format!("{name}: {e}"))
}
fn optional<T: DeserializeOwned>(args: &Value, name: &str) -> Result<Option<T>, String> {
    args.get(name)
        .filter(|v| !v.is_null())
        .map(|v| serde_json::from_value(v.clone()).map_err(|e| format!("{name}: {e}")))
        .transpose()
}
fn string<'a>(args: &'a Value, name: &str) -> Result<&'a str, String> {
    args.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string {name}"))
}
fn safe_name(name: &str) -> Result<(), String> {
    if name.is_empty() || validate::file_stem(name) != name {
        Err("choose a safe, nonempty file name".into())
    } else {
        Ok(())
    }
}
fn history_graph(name: &str) -> Result<String, String> {
    validate::text("graph", name, validate::MAX_PATH_LEN)?;
    if name.contains(['/', '\\']) || name.chars().any(char::is_control) {
        return Err("choose a graph name without path separators or control characters".into());
    }
    Ok(history::graph_name(name))
}

fn media_type(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "image/gif"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else if bytes.starts_with(b"BM") {
        "image/bmp"
    } else if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        "image/tiff"
    } else if bytes.starts_with(b"%PDF-") {
        "application/pdf"
    } else if std::str::from_utf8(&bytes[..bytes.len().min(512)])
        .ok()
        .is_some_and(|prefix| {
            let prefix = prefix.trim_start_matches('\u{feff}').trim_start();
            prefix.starts_with("<svg") || prefix.starts_with("<?xml") && prefix.contains("<svg")
        })
    {
        "image/svg+xml"
    } else {
        "application/octet-stream"
    }
}
fn tail(text: &str, max: usize) -> String {
    let mut start = text.len().saturating_sub(max);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
}
fn head(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
fn fields_value(fields: &[workflow::Field]) -> Value {
    json!(
        fields
            .iter()
            .map(|f| json!({"node":f.node,"name":f.name,"config":f.config}))
            .collect::<Vec<_>>()
    )
}
fn issues(data: &NodeData, node: Option<&str>) -> Vec<Value> {
    data.validate()
        .into_iter()
        .map(|i| {
            let mut issue = json!({"field":i.field,"message":i.message});
            if let Some(node) = node {
                issue["node"] = json!(node);
            }
            issue
        })
        .collect()
}
fn targets(args: &Value, graph: &GraphDoc) -> Result<Vec<String>, String> {
    let targets: Vec<String> = optional(args, "targets")?.unwrap_or_else(|| exec::sinks(graph));
    if targets.is_empty() {
        return Err("nothing to run: no node has content".into());
    }
    if let Some(id) = targets
        .iter()
        .find(|id| !graph.nodes.iter().any(|n| &n.id == *id))
    {
        return Err(format!("unknown target: {id}"));
    }
    Ok(targets)
}

fn interface_targets(args: &Value, graph: &GraphDoc) -> Result<Vec<String>, String> {
    if args
        .get("targets")
        .is_some_and(|targets| !targets.is_null())
        || graph.outputs.is_empty()
    {
        return targets(args, graph);
    }
    if graph.outputs.len() > workflow::MAX_FIELDS {
        return Err("a graph can publish at most 64 outputs".into());
    }
    let mut selected = Vec::new();
    for (name, handle) in &graph.outputs {
        validate::output_name(name).map_err(|error| format!("published output {name}: {error}"))?;
        if !validate::is_output_handle(handle) {
            return Err(format!(
                "published output {name}: choose a node id or named output handle"
            ));
        }
        let base = handle.split('.').next().unwrap_or_default();
        if !graph.nodes.iter().any(|node| node.id == base) {
            return Err(format!(
                "published output {name}: {handle} is not in the graph"
            ));
        }
        selected.push(base.to_string());
    }
    selected.sort();
    selected.dedup();
    Ok(selected)
}

/// Form diagnostics that depend on direct wires or node names, without resolving
/// required run values or reading input files and provider outputs.
fn wiring_issues(graph: &GraphDoc, plans: &calls::Plans) -> Vec<Value> {
    let mut issues = Vec::new();
    let mut names: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for node in &graph.nodes {
        if let Some(name) = node
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            names.entry(name).or_default().push(&node.id);
        }
    }
    for (name, ids) in names.iter().filter(|(_, ids)| ids.len() > 1) {
        for id in ids {
            issues.push(json!({"node":id,"field":"name","message":format!("{} share the name {name}; a placeholder needs one of each", ids.join(" and "))}));
        }
    }
    let mut handles: Vec<_> = graph.edges.iter().map(doc::EdgeDoc::handle).collect();
    handles.sort();
    handles.dedup();
    let binary = exec::binary_output_handles(graph, plans, &handles);
    for node in &graph.nodes {
        let mut upstream: Vec<_> = graph
            .edges
            .iter()
            .filter(|edge| edge.to == node.id)
            .map(doc::EdgeDoc::handle)
            .collect();
        upstream.sort();
        upstream.dedup();
        if matches!(node.data, Some(NodeData::Input(_))) {
            if node.name.as_deref().unwrap_or_default().trim().is_empty() {
                issues.push(json!({"node":node.id,"field":"name","message":"Name this Run input so it can appear in the Run form"}));
            }
            if !upstream.is_empty() {
                issues.push(json!({"node":node.id,"field":"inputs","message":"A Run input is a source; remove its incoming wires"}));
            }
        }
        if let Some(NodeData::Agent(agent)) = &node.data {
            let aliases: Vec<_> = upstream
                .iter()
                .filter_map(|handle| {
                    let (id, port) = handle
                        .split_once('.')
                        .map_or((handle.as_str(), None), |(id, port)| (id, Some(port)));
                    let source = graph.nodes.iter().find(|source| source.id == id)?;
                    let name = source.name.as_deref()?.trim();
                    validate::node_name(name).ok()?;
                    Some(port.map_or_else(|| name.to_string(), |port| format!("{name}.{port}")))
                })
                .collect();
            for (field, template) in [("prompt", &agent.prompt), ("message", &agent.message)] {
                for hole in validate::placeholders(template) {
                    let count = aliases.iter().filter(|alias| **alias == hole).count();
                    let message = if count > 1 {
                        Some(format!(
                            "{{{{{hole}}}}} is ambiguous; more than one input has this name"
                        ))
                    } else if count == 0 && !upstream.contains(&hole) {
                        Some(format!(
                            "{{{{{hole}}}}} names no input; {}",
                            if upstream.is_empty() {
                                "nothing is wired into this node".into()
                            } else {
                                format!("the inputs are {}", upstream.join(", "))
                            }
                        ))
                    } else {
                        None
                    };
                    if let Some(message) = message {
                        issues.push(json!({"node":node.id,"field":field,"message":message}));
                    }
                }
            }
        }
        let requires_text = matches!(
            node.data,
            Some(NodeData::Agent(_) | NodeData::Database(_) | NodeData::Script(_))
        ) || matches!(&node.data, Some(NodeData::File(config)) if config.bundle || upstream.len() != 1);
        if requires_text {
            for handle in upstream.iter().filter(|handle| binary.contains(*handle)) {
                issues.push(json!({"node":node.id,"field":"inputs","message":format!("{handle} produces binary output, which {} cannot take as text",node.id)}));
            }
        }
    }
    issues
}
fn members(graph: &GraphDoc, targets: &[String]) -> HashSet<String> {
    let mut members = HashSet::new();
    let mut pending = targets.to_vec();
    while let Some(id) = pending.pop() {
        if members.insert(id.clone()) {
            pending.extend(
                graph
                    .edges
                    .iter()
                    .filter(|e| e.to == id)
                    .map(|e| e.from.clone()),
            );
        }
    }
    members
}
fn structural(graph: &GraphDoc) -> Result<(), String> {
    storage::check_size(
        serde_json::to_vec(graph).map_err(|e| e.to_string())?.len(),
        storage::MAX_DOCUMENT_BYTES,
    )?;
    if graph.version != doc::VERSION {
        return Err(format!("unsupported graph version {}", graph.version));
    }
    if !graph.view.pan_x.is_finite() || !graph.view.pan_y.is_finite() {
        return Err("view position must be finite".into());
    }
    if !graph.view.zoom.is_finite() || !(0.15..=2.5).contains(&graph.view.zoom) {
        return Err("view zoom must be between 0.15 and 2.5".into());
    }
    if graph.outputs.len() > workflow::MAX_FIELDS {
        return Err("a graph can publish at most 64 outputs".into());
    }
    let mut ids = HashSet::new();
    for node in &graph.nodes {
        if !validate::is_node_id(&node.id) || !ids.insert(node.id.as_str()) {
            return Err(format!("invalid or duplicate node id: {}", node.id));
        }
        if !node.x.is_finite() || !node.y.is_finite() {
            return Err("node positions must be finite".into());
        }
        if let Some(name) = node.name.as_deref().filter(|name| !name.is_empty()) {
            validate::node_name(name)?;
        }
        if let Some(color) = &node.color
            && doc::hex_to_rgb(color).is_none()
        {
            return Err(format!("{}: invalid node color", node.id));
        }
    }
    for edge in &graph.edges {
        if !ids.contains(edge.from.as_str()) || !ids.contains(edge.to.as_str()) {
            return Err("a wire references a missing node".into());
        }
        if let Some(name) = &edge.output {
            validate::output_name(name)?;
        }
    }
    for (name, handle) in &graph.outputs {
        validate::output_name(name)?;
        if !validate::is_output_handle(handle)
            || !ids.contains(handle.split('.').next().unwrap_or_default())
        {
            return Err(format!("{name}: select an existing output node"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{EdgeDoc, NodeDoc};
    use crate::node_data::{FileConfig, ScriptConfig};
    use std::time::{Duration, Instant};

    struct Workspace {
        root: PathBuf,
        service: AppService,
    }
    impl Workspace {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "uapz-app-{}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(root.join("data")).unwrap();
            std::fs::create_dir_all(root.join("scripts")).unwrap();
            Self {
                service: AppService::new(root.clone()),
                root,
            }
        }
        fn state(&self) -> Value {
            self.service.request("runtime_state", json!({})).unwrap()
        }
        fn until(&self, condition: impl Fn(&Value) -> bool) -> Value {
            let started = Instant::now();
            loop {
                let state = self.state();
                if condition(&state) {
                    return state;
                }
                assert!(
                    started.elapsed() < Duration::from_secs(10),
                    "worker timed out: {state}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
    impl Drop for Workspace {
        fn drop(&mut self) {
            self.service.cancel_active();
            self.until(|s| s.is_null() || s["running"] == false);
            std::fs::remove_dir_all(&self.root).unwrap();
        }
    }
    fn node(id: &str, name: Option<&str>, data: NodeData) -> NodeDoc {
        NodeDoc {
            id: id.into(),
            x: 0.,
            y: 0.,
            kind: None,
            color: None,
            name: name.map(str::to_string),
            data: Some(data),
        }
    }
    fn input_doc() -> GraphDoc {
        GraphDoc {
            version: doc::VERSION,
            name: "service-input".into(),
            nodes: vec![node(
                "n0",
                Some("question"),
                NodeData::Input(workflow::InputConfig {
                    default: "hello".into(),
                    ..Default::default()
                }),
            )],
            ..Default::default()
        }
    }
    fn review_doc() -> GraphDoc {
        GraphDoc {
            name: "service-review".into(),
            nodes: vec![
                node(
                    "n0",
                    Some("question"),
                    NodeData::Input(workflow::InputConfig {
                        default: "hello".into(),
                        ..Default::default()
                    }),
                ),
                node(
                    "n1",
                    None,
                    NodeData::File(FileConfig {
                        path: "review.txt".into(),
                        review: true,
                        ..Default::default()
                    }),
                ),
            ],
            edges: vec![EdgeDoc {
                from: "n0".into(),
                to: "n1".into(),
                output: None,
            }],
            version: doc::VERSION,
            ..Default::default()
        }
    }

    #[test]
    fn asynchronous_run_keeps_snapshot_history_and_cache_hydrated_output() {
        let workspace = Workspace::new();
        let graph = input_doc();
        let started = workspace
            .service
            .request("start_run", json!({"doc":graph}))
            .unwrap();
        let state = workspace.until(|s| s["running"] == false);
        assert_eq!(state["run_id"], started["run_id"]);
        assert_eq!(state["graph"], json!(graph));
        assert_eq!(state["run"]["outcome"]["kind"], "ok");
        assert_eq!(state["marks"]["n0"], "ran");
        let runs = workspace
            .service
            .request("history", json!({"graph":"service-input"}))
            .unwrap();
        assert_eq!(runs.as_array().unwrap().len(), 1);
        assert_eq!(runs[0], state["run"]);
        let output = workspace
            .service
            .request(
                "output",
                json!({"graph":"service-input","run_id":state["run"]["id"],"node":"n0"}),
            )
            .unwrap();
        assert_eq!(output["text"], "hello");
        assert_eq!(
            workspace
                .service
                .request("clear_history", json!({"graph":"service-input"}))
                .unwrap(),
            1
        );
        assert_eq!(
            workspace
                .service
                .request("history", json!({"graph":"service-input"}))
                .unwrap(),
            json!([])
        );
    }

    #[test]
    fn pending_review_blocks_second_run_and_rejects_stale_decisions() {
        let workspace = Workspace::new();
        let graph = review_doc();
        let started = workspace
            .service
            .request("start_run", json!({"doc":graph}))
            .unwrap();
        let state = workspace.until(|s| s["review"].is_object());
        assert!(!workspace.root.join("review.txt").exists());
        assert!(
            workspace
                .service
                .request("start_run", json!({"doc":input_doc()}))
                .unwrap_err()
                .contains("already active")
        );
        assert!(workspace.service.request("clear_cache", json!({})).is_err());
        assert!(
            workspace
                .service
                .request(
                    "review",
                    json!({"run_id":"stale","review_id":state["review"]["id"],"decision":"apply"})
                )
                .is_err()
        );
        assert!(
            workspace
                .service
                .request(
                    "review",
                    json!({"run_id":started["run_id"],"review_id":"stale","decision":"apply"})
                )
                .is_err()
        );
        let args = json!({"run_id":started["run_id"],"review_id":state["review"]["id"],"decision":"apply"});
        workspace.service.request("review", args.clone()).unwrap();
        assert!(workspace.service.request("review", args).is_err());
        let state = workspace.until(|s| s["running"] == false);
        assert_eq!(
            std::fs::read_to_string(workspace.root.join("review.txt")).unwrap(),
            "hello\n"
        );
        assert!(state["review"].is_null());
        assert_eq!(state["run"]["outcome"]["kind"], "ok");
    }

    #[test]
    fn cancellation_wakes_review_and_retains_cancelled_history() {
        let workspace = Workspace::new();
        workspace
            .service
            .request("start_run", json!({"doc":review_doc()}))
            .unwrap();
        let state = workspace.until(|s| s["review"].is_object());
        workspace
            .service
            .request("cancel_run", json!({"run_id":state["run_id"]}))
            .unwrap();
        let state = workspace.until(|s| s["running"] == false);
        assert!(!workspace.root.join("review.txt").exists());
        assert_eq!(state["run"]["outcome"]["kind"], "cancelled");
        assert!(state["review"].is_null());
        let runs = history::list(&workspace.root, "service-review");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].outcome, history::Outcome::Cancelled);
        assert!(
            workspace
                .service
                .request("cancel_run", json!({"run_id":state["run_id"]}))
                .is_err()
        );
    }

    #[test]
    fn final_service_owner_cancels_pending_worker_but_a_clone_does_not() {
        let workspace = Workspace::new();
        let service = AppService::new(workspace.root.clone());
        let clone = service.clone();
        service
            .request("start_run", json!({"doc":review_doc()}))
            .unwrap();
        let started = Instant::now();
        let session = loop {
            let session = lock(&service.current).clone().unwrap();
            if lock(&session).review.is_some() {
                break session;
            }
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(10));
        };
        drop(clone);
        assert!(!lock(&session).cancel.load(Ordering::Relaxed));
        drop(service);
        while lock(&session).running {
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            lock(&session).run.as_ref().unwrap().outcome,
            history::Outcome::Cancelled
        );
        assert!(!workspace.root.join("review.txt").exists());
    }

    #[test]
    fn rejected_review_keeps_files_and_persists_failure() {
        let workspace = Workspace::new();
        workspace
            .service
            .request("start_run", json!({"doc":review_doc()}))
            .unwrap();
        let state = workspace.until(|s| s["review"].is_object());
        workspace.service.request("review",json!({"run_id":state["run_id"],"review_id":state["review"]["id"],"decision":"reject"})).unwrap();
        let state = workspace.until(|s| s["running"] == false);
        assert!(!workspace.root.join("review.txt").exists());
        assert_eq!(state["run"]["outcome"]["kind"], "failed");
        assert!(
            state["run"]["outcome"]["error"]
                .as_str()
                .unwrap()
                .contains("rejected")
        );
    }

    #[test]
    fn malformed_mutations_and_unknown_inputs_have_no_effects() {
        let workspace = Workspace::new();
        let mut graph = input_doc();
        graph.nodes.push(graph.nodes[0].clone());
        assert!(
            workspace
                .service
                .request("save_graph", json!({"doc":graph}))
                .unwrap_err()
                .contains("duplicate")
        );
        assert!(
            workspace
                .service
                .request(
                    "start_run",
                    json!({"doc":input_doc(),"inputs":{"unknown":"value"}})
                )
                .is_err()
        );
        assert!(
            workspace
                .service
                .request("save_prompt", json!({"name":"../escape","text":"no"}))
                .is_err()
        );
        assert!(
            workspace
                .service
                .request(
                    "save_preset",
                    json!({"type":"script","name":"name","config":{}})
                )
                .is_err()
        );
        assert!(workspace.state().is_null());
        assert!(history::list(&workspace.root, "service-input").is_empty());
        let result = workspace
            .service
            .request(
                "validate_node",
                json!({"data":NodeData::Script(ScriptConfig::default())}),
            )
            .unwrap();
        assert!(!result.as_array().unwrap().is_empty());
    }

    #[test]
    fn graph_validation_reports_wiring_placeholders_names_and_binary_inputs_without_run_values() {
        let workspace = Workspace::new();
        let mut graph = input_doc();
        graph.nodes[0].name = Some("shared".into());
        graph.nodes[0].data = Some(NodeData::Input(workflow::InputConfig::default()));
        graph.nodes.push(node(
            "n1",
            Some("shared"),
            NodeData::Data(crate::node_data::DataConfig {
                path: "logo.png".into(),
                format: DataFormat::Image,
            }),
        ));
        graph.nodes.push(node(
            "n2",
            None,
            NodeData::Agent(AgentConfig {
                prompt: "Use {{shared}} and {{missing}}".into(),
                ..Default::default()
            }),
        ));
        graph.edges = vec![
            EdgeDoc {
                from: "n0".into(),
                to: "n2".into(),
                output: None,
            },
            EdgeDoc {
                from: "n1".into(),
                to: "n2".into(),
                output: None,
            },
        ];
        let result = workspace
            .service
            .request("validate_graph", json!({"doc":graph}))
            .unwrap();
        let issues = result["issues"].as_array().unwrap();
        assert!(
            issues
                .iter()
                .any(|issue| issue["field"] == "name" && issue["node"] == "n0")
        );
        assert!(
            issues
                .iter()
                .any(|issue| issue["message"].as_str().unwrap().contains("ambiguous"))
        );
        assert!(
            issues
                .iter()
                .any(|issue| issue["message"].as_str().unwrap().contains("{{missing}}"))
        );
        assert!(
            issues
                .iter()
                .any(|issue| issue["message"].as_str().unwrap().contains("binary output"))
        );
        assert!(!issues.iter().any(|issue| {
            issue["message"]
                .as_str()
                .unwrap()
                .contains("value is required")
        }));
        assert!(workspace.state().is_null());
    }

    #[test]
    fn published_interface_ignores_unrelated_inputs_but_checks_actual_named_ports() {
        let workspace = Workspace::new();
        let mut graph = input_doc();
        graph.nodes.push(node(
            "n1",
            None,
            NodeData::Input(workflow::InputConfig::default()),
        ));
        graph.outputs.insert("answer".into(), "n0".into());
        let fields = workspace
            .service
            .request("workflow_fields", json!({"doc":graph}))
            .unwrap();
        assert_eq!(fields.as_array().unwrap().len(), 1);
        assert_eq!(fields[0]["node"], "n0");
        graph.outputs.insert("invalid".into(), "n0.missing".into());
        assert!(
            workspace
                .service
                .request("workflow_fields", json!({"doc":graph}))
                .unwrap_err()
                .contains("no named outputs")
        );
        assert!(
            workspace
                .service
                .request("workflow_fields", json!({"doc":graph,"targets":["n0"]}))
                .is_ok()
        );
        assert!(
            workspace
                .service
                .request(
                    "workflow_fields",
                    json!({"doc":graph,"targets":["n0.missing"]})
                )
                .is_err()
        );
        let result = workspace
            .service
            .request("validate_graph", json!({"doc":graph}))
            .unwrap();
        assert!(
            result["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| issue["field"] == "outputs")
        );
    }

    #[test]
    fn view_and_history_names_are_validated_without_aliasing_paths() {
        let workspace = Workspace::new();
        let mut graph = input_doc();
        for zoom in [0., -1., f32::NAN, f32::INFINITY, 2.6] {
            graph.view.zoom = zoom;
            assert!(structural(&graph).is_err());
        }
        graph.view.zoom = 1.;
        graph.view.pan_x = f32::INFINITY;
        assert!(structural(&graph).is_err());
        for name in ["../service-input", "sub/graph", "sub\\graph", "name\n"] {
            assert!(
                workspace
                    .service
                    .request("history", json!({"graph":name}))
                    .is_err()
            );
            assert!(
                workspace
                    .service
                    .request("clear_history", json!({"graph":name}))
                    .is_err()
            );
        }
        assert_eq!(
            history_graph("Draft: question?").unwrap(),
            history::graph_name("Draft: question?")
        );
        assert_eq!(history_graph("").unwrap(), "untitled");
    }

    #[test]
    fn reveal_takes_the_absolute_path_a_run_reported_and_refuses_to_leave_the_workspace() {
        let workspace = Workspace::new();
        let saved = workspace.root.join("data/note.txt");
        std::fs::write(&saved, b"x").unwrap();
        let reported = saved.canonicalize().unwrap();
        // The renderer echoes the absolute path a run reported; it resolves back inside.
        assert_eq!(
            validate::resolve_absolute_inside(&workspace.root, &reported.to_string_lossy())
                .unwrap(),
            reported
        );
        // Absolute only widens what may be named. A path outside the workspace is refused
        // by the containment proof, and a `..` segment by the path rules, before any
        // process starts, so the same proof backs both the validator and the command.
        let outside = std::env::temp_dir().join(format!("uapz-reveal-out-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        let escape = outside.to_string_lossy().to_string();
        let err = workspace
            .service
            .request("reveal", json!({"path":escape}))
            .unwrap_err();
        assert!(err.contains("outside the workspace"), "{err}");
        let err = workspace
            .service
            .request("reveal", json!({"path":"data/../data/note.txt"}))
            .unwrap_err();
        assert!(err.contains("`..`"), "{err}");
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn binary_outputs_use_the_captured_workspace_and_preserve_media_type() {
        let workspace = Workspace::new();
        let bytes = b"\xff\xd8\xffsample JPEG";
        std::fs::write(workspace.root.join("data/image.jpg"), bytes).unwrap();
        let graph = GraphDoc {
            version: doc::VERSION,
            name: "binary".into(),
            nodes: vec![node(
                "n0",
                None,
                NodeData::Data(crate::node_data::DataConfig {
                    path: "image.jpg".into(),
                    format: DataFormat::Image,
                }),
            )],
            ..Default::default()
        };
        workspace
            .service
            .request("start_run", json!({"doc":graph}))
            .unwrap();
        let state = workspace.until(|state| state["running"] == false);
        let args = json!({"graph":"binary","run_id":state["run"]["id"],"node":"n0"});
        let output = workspace.service.request("output", args.clone()).unwrap();
        assert_eq!(output["media_type"], "image/jpeg");
        assert!(output["text"].is_null());
        assert_eq!(output["complete"], true);
        use base64::Engine as _;
        assert_eq!(
            output["base64"],
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );
        workspace.service.request("clear_cache", json!({})).unwrap();
        assert!(workspace.service.request("output", args).is_err());
    }

    #[test]
    fn optional_output_byte_limits_preserve_utf8_boundaries_and_default_full_text() {
        let workspace = Workspace::new();
        let text = format!("A😀Bé{}", "😀".repeat(history::PREVIEW_CHARS));
        let mut graph = input_doc();
        graph.nodes[0].data = Some(NodeData::Input(workflow::InputConfig {
            default: text.clone(),
            ..Default::default()
        }));
        let started = workspace
            .service
            .request("start_run", json!({"doc":graph}))
            .unwrap();
        workspace.until(|state| state["running"] == false);
        let args = json!({"graph":"service-input","run_id":started["run_id"],"node":"n0"});
        let output = workspace.service.request("output", args.clone()).unwrap();
        assert_eq!(output["text"], text);
        assert_eq!(output["complete"], true);
        assert_eq!(output["limited"], false);
        assert_eq!(output["bytes"], text.len());
        for (limit, expected) in [
            (1, "A"),
            (2, "A"),
            (4, "A"),
            (5, "A😀"),
            (6, "A😀B"),
            (7, "A😀B"),
            (8, "A😀Bé"),
        ] {
            let mut args = args.clone();
            args["max_bytes"] = json!(limit);
            let output = workspace.service.request("output", args).unwrap();
            assert_eq!(output["text"], expected);
            assert_eq!(output["complete"], false);
            assert_eq!(output["limited"], true);
            assert_eq!(output["bytes"], text.len());
            assert!(output["text"].as_str().unwrap().len() <= limit);
        }
        for limit in [text.len(), text.len() + 1, exec::MAX_OUTPUT_BYTES] {
            let mut args = args.clone();
            args["max_bytes"] = json!(limit);
            let output = workspace.service.request("output", args).unwrap();
            assert_eq!(output["text"], text);
            assert_eq!(output["complete"], true);
            assert_eq!(output["limited"], false);
        }
        for invalid in [
            json!(0),
            json!(-1),
            json!(1.5),
            json!("256"),
            json!(true),
            json!([]),
            json!({}),
            Value::Null,
            json!(exec::MAX_OUTPUT_BYTES + 1),
        ] {
            let mut args = args.clone();
            args["max_bytes"] = invalid;
            assert!(
                workspace
                    .service
                    .request("output", args)
                    .unwrap_err()
                    .contains("max_bytes")
            );
        }
    }

    #[test]
    fn optional_binary_byte_limits_omit_oversized_payloads_and_allow_exact_boundaries() {
        use base64::Engine as _;
        let workspace = Workspace::new();
        let bytes = b"\x89PNG\r\n\x1a\nimage payload";
        std::fs::write(workspace.root.join("data/limited.png"), bytes).unwrap();
        let graph = GraphDoc {
            name: "binary-limit".into(),
            version: doc::VERSION,
            nodes: vec![node(
                "n0",
                None,
                NodeData::Data(crate::node_data::DataConfig {
                    path: "limited.png".into(),
                    format: DataFormat::Image,
                }),
            )],
            ..Default::default()
        };
        workspace
            .service
            .request("start_run", json!({"doc":graph}))
            .unwrap();
        let state = workspace.until(|state| state["running"] == false);
        let args = json!({"graph":"binary-limit","run_id":state["run"]["id"],"node":"n0"});
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        for limit in [None, Some(bytes.len()), Some(bytes.len() + 1)] {
            let mut args = args.clone();
            if let Some(limit) = limit {
                args["max_bytes"] = json!(limit);
            }
            let output = workspace.service.request("output", args).unwrap();
            assert_eq!(output["base64"], encoded);
            assert_eq!(output["media_type"], "image/png");
            assert_eq!(output["bytes"], bytes.len());
            assert_eq!(output["limited"], false);
            assert_eq!(output["complete"], true);
        }
        for limit in [1, bytes.len() - 1] {
            let mut args = args.clone();
            args["max_bytes"] = json!(limit);
            let output = workspace.service.request("output", args).unwrap();
            assert!(output["base64"].is_null());
            assert!(output["text"].is_null());
            assert_eq!(output["media_type"], "image/png");
            assert_eq!(output["bytes"], bytes.len());
            assert_eq!(output["limited"], true);
            assert_eq!(output["complete"], false);
        }
    }

    #[test]
    fn feed_preserves_order_coalesces_text_and_replaces_completed_streams() {
        let workspace = Workspace::new();
        workspace
            .service
            .request("start_run", json!({"doc":input_doc()}))
            .unwrap();
        workspace.until(|state| state["running"] == false);
        let current = lock(&workspace.service.current).clone().unwrap();
        let mut session = lock(&current);
        session.events.clear();
        session.event_bytes = 0;
        session.events_dropped = 0;
        session.next_seq = 1;
        session.progress(Progress::Node {
            id: "n0".into(),
            state: exec::NodeState::Running,
        });
        session.progress(Progress::Log("starting".into()));
        session.progress(Progress::Text {
            id: "n0".into(),
            text: "one".into(),
        });
        session.progress(Progress::Text {
            id: "n0".into(),
            text: " two".into(),
        });
        session.progress(Progress::Log("between chunks".into()));
        session.progress(Progress::Text {
            id: "n0".into(),
            text: " three".into(),
        });
        let events = session.value()["events"].as_array().unwrap().clone();
        assert_eq!(
            events
                .iter()
                .map(|event| event["seq"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            [1, 2, 3, 4, 5]
        );
        assert_eq!(events[2]["text"], "one two");
        assert_eq!(events[4]["text"], " three");
        let result = session.run.as_ref().unwrap().nodes["n0"].clone();
        session.progress(Progress::Node {
            id: "n0".into(),
            state: exec::NodeState::Done,
        });
        session.progress(Progress::Output {
            id: "n0".into(),
            result: Box::new(result),
        });
        let state = session.value();
        let events = state["events"].as_array().unwrap();
        assert!(!events.iter().any(|event| event["kind"] == "text"));
        assert_eq!(events.last().unwrap()["seq"], 7);
        assert_eq!(events.last().unwrap()["kind"], "output");
        assert!(events.last().unwrap()["result"].get("data").is_none());
        assert_eq!(state["events_dropped"], 0);
        assert!(state["stream"].is_null());
    }

    #[test]
    fn feed_retention_is_bounded_and_flags_utf8_truncation_and_real_evictions() {
        let workspace = Workspace::new();
        workspace
            .service
            .request("start_run", json!({"doc":input_doc()}))
            .unwrap();
        workspace.until(|state| state["running"] == false);
        let current = lock(&workspace.service.current).clone().unwrap();
        let mut session = lock(&current);
        session.events.clear();
        session.event_bytes = 0;
        session.events_dropped = 0;
        session.next_seq = 1;
        for _ in 0..MAX_FEED_EVENTS + 7 {
            session.progress(Progress::Log("entry".into()));
        }
        assert_eq!(session.events.len(), MAX_FEED_EVENTS);
        assert_eq!(session.events_dropped, 7);
        assert_eq!(session.events.front().unwrap().event.seq, 8);
        session.progress(Progress::Text {
            id: "n0".into(),
            text: "😀".repeat(MAX_LIVE_TEXT),
        });
        let seq = session.events.back().unwrap().event.seq;
        session.progress(Progress::Text {
            id: "n0".into(),
            text: "end".into(),
        });
        let event = serde_json::to_value(&session.events.back().unwrap().event).unwrap();
        assert_eq!(event["seq"], seq);
        assert_eq!(event["truncated"], true);
        assert!(event["text"].as_str().unwrap().len() <= MAX_LIVE_TEXT);
        assert!(event["text"].as_str().unwrap().ends_with("end"));
        for _ in 0..20 {
            session.progress(Progress::Log("😀".repeat(MAX_LIVE_TEXT)));
        }
        assert!(session.event_bytes <= MAX_FEED_BYTES);
        assert!(session.events_dropped > 7);
        assert!(
            session
                .events
                .iter()
                .zip(session.events.iter().skip(1))
                .all(|(a, b)| a.event.seq < b.event.seq)
        );
        assert_eq!(
            session.value()["events"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()["truncated"],
            true
        );
        let mut result = session.run.as_ref().unwrap().nodes["n0"].clone();
        result.preview = "😀".repeat(history::PREVIEW_CHARS);
        result.named_outputs = (0..64)
            .map(|i| (format!("port{i}"), result.clone()))
            .collect();
        for i in 0..8 {
            session.progress(Progress::Output {
                id: format!("n{i}"),
                result: Box::new(result.clone()),
            });
        }
        assert!(session.event_bytes <= MAX_FEED_BYTES);
        assert!(session.output_bytes <= MAX_LIVE_OUTPUT_BYTES);
        assert!(session.outputs.len() < 8);
        let retained = session.events.len();
        let retained_bytes = session.event_bytes;
        let dropped = session.events_dropped;
        session.event(FeedBody::Log {
            text: "x".repeat(MAX_FEED_BYTES + 1),
            truncated: false,
        });
        assert_eq!(session.events.len(), retained);
        assert_eq!(session.event_bytes, retained_bytes);
        assert_eq!(session.events_dropped, dropped + 1);
    }

    #[test]
    fn nonstreamed_and_reused_outputs_appear_in_feed_in_execution_order() {
        let workspace = Workspace::new();
        std::fs::write(
            workspace.root.join("scripts/feed.sh"),
            "printf 'script output'\n",
        )
        .unwrap();
        let graph = GraphDoc {
            name: "feed-script".into(),
            version: doc::VERSION,
            nodes: vec![node(
                "n0",
                None,
                NodeData::Script(ScriptConfig {
                    runtime: Runtime::Shell,
                    path: "feed.sh".into(),
                    args: Vec::new(),
                    timeout_secs: 5,
                }),
            )],
            ..Default::default()
        };
        for expected in ["ran", "reused"] {
            let started = workspace
                .service
                .request("start_run", json!({"doc":graph}))
                .unwrap();
            let state = workspace.until(|state| state["running"] == false);
            let events = state["events"].as_array().unwrap();
            let output = events
                .iter()
                .position(|event| event["kind"] == "output")
                .unwrap();
            let done = events
                .iter()
                .position(|event| event["kind"] == "node" && event["state"] == expected)
                .unwrap();
            assert!(done < output);
            assert_eq!(events[output]["result"]["state"], expected);
            assert_eq!(events[output]["result"]["preview"], "script output");
            assert!(!events.iter().any(|event| event["kind"] == "text"));
            let full = workspace
                .service
                .request(
                    "output",
                    json!({"graph":"feed-script","run_id":started["run_id"],"node":"n0"}),
                )
                .unwrap();
            assert_eq!(full["text"], "script output");
            assert_eq!(state["run"]["nodes"]["n0"]["state"], expected);
        }
    }

    #[test]
    fn nested_named_outputs_are_available_live_before_a_review_finishes() {
        let workspace = Workspace::new();
        let long = "live output ".repeat(history::PREVIEW_CHARS);
        let leaf = GraphDoc {
            name: "leaf".into(),
            version: doc::VERSION,
            nodes: vec![node(
                "n0",
                Some("value"),
                NodeData::Input(workflow::InputConfig {
                    default: long.clone(),
                    ..Default::default()
                }),
            )],
            outputs: BTreeMap::from([("text".into(), "n0".into())]),
            ..Default::default()
        };
        let middle = GraphDoc {
            name: "middle".into(),
            version: doc::VERSION,
            nodes: vec![node(
                "n0",
                None,
                NodeData::Workflow(calls::Config {
                    graph: "leaf".into(),
                    ..Default::default()
                }),
            )],
            outputs: BTreeMap::from([("forward".into(), "n0.text".into())]),
            ..Default::default()
        };
        std::fs::create_dir(workspace.root.join("graphs")).unwrap();
        for graph in [&leaf, &middle] {
            std::fs::write(
                workspace
                    .root
                    .join("graphs")
                    .join(format!("{}.json", graph.name)),
                serde_json::to_vec(graph).unwrap(),
            )
            .unwrap();
        }
        let graph = GraphDoc {
            name: "feed-workflow".into(),
            version: doc::VERSION,
            nodes: vec![
                node(
                    "n0",
                    None,
                    NodeData::Workflow(calls::Config {
                        graph: "middle".into(),
                        ..Default::default()
                    }),
                ),
                node(
                    "n1",
                    None,
                    NodeData::File(FileConfig {
                        path: "feed.txt".into(),
                        review: true,
                        ..Default::default()
                    }),
                ),
            ],
            edges: vec![EdgeDoc {
                from: "n0".into(),
                to: "n1".into(),
                output: Some("forward".into()),
            }],
            ..Default::default()
        };
        let started = workspace
            .service
            .request("start_run", json!({"doc":graph}))
            .unwrap();
        let state = workspace.until(|state| state["review"].is_object());
        let events = state["events"].as_array().unwrap();
        let outputs = events
            .iter()
            .filter(|event| event["kind"] == "output")
            .collect::<Vec<_>>();
        assert_eq!(
            outputs
                .iter()
                .map(|event| event["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["n0/n0/n0", "n0/n0", "n0"]
        );
        assert_eq!(
            outputs[1]["result"]["named_outputs"]["text"]["complete"],
            false
        );
        assert_eq!(
            outputs[2]["result"]["named_outputs"]["forward"]["complete"],
            false
        );
        for location in [
            json!({"node":"n0","call_path":"n0/n0"}),
            json!({"node":"n0/n0/n0"}),
            json!({"node":"n0","output":"forward"}),
            json!({"node":"n0","call_path":"n0","output":"text"}),
        ] {
            let mut args = location;
            args["graph"] = json!("feed-workflow");
            args["run_id"] = started["run_id"].clone();
            let output = workspace.service.request("output", args).unwrap();
            assert_eq!(output["text"], long);
            assert_eq!(output["complete"], true);
        }
        assert!(!workspace.root.join("feed.txt").exists());
        workspace.service.request("review",json!({"run_id":started["run_id"],"review_id":state["review"]["id"],"decision":"apply"})).unwrap();
        let done = workspace.until(|state| state["running"] == false);
        assert!(
            done["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|event| event["kind"] == "output" && event["id"] == "n1")
        );
        assert_eq!(
            done["run"]["calls"]["n0"]["run"]["calls"]["n0"]["run"]["nodes"]["n0"]["preview"],
            long.chars()
                .take(history::PREVIEW_CHARS)
                .collect::<String>()
        );
        assert_eq!(history::list(&workspace.root, "feed-workflow").len(), 1);
    }

    #[test]
    fn streaming_bounds_preserve_utf8_and_latest_logs() {
        let workspace = Workspace::new();
        workspace
            .service
            .request("start_run", json!({"doc":input_doc()}))
            .unwrap();
        workspace.until(|s| s["running"] == false);
        let session = lock(&workspace.service.current).clone().unwrap();
        let mut session = lock(&session);
        for _ in 0..8 {
            session.progress(Progress::Text {
                id: "n0".into(),
                text: "😀".repeat(MAX_LIVE_TEXT / 4),
            });
            session.progress(Progress::Log("x".repeat(MAX_LIVE_TEXT)));
        }
        assert!(session.stream.as_ref().unwrap().1.len() <= MAX_LIVE_TEXT);
        assert!(session.log_bytes <= MAX_LIVE_LOG);
        assert!(session.log.len() <= 2);
    }
}
