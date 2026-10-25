//! Thread-safe application boundary shared by desktop clients and tests.
//!
//! Library helpers retain their established workspace-relative paths. The desktop
//! host selects the workspace as its working directory once, before constructing
//! this service. Execution, cache and history always use the captured explicit root.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::{
    calls,
    doc::{self, GraphDoc},
    exec::{self, Cache, Control, Progress},
    history, library,
    node_data::{
        AgentConfig, DataFormat, DatabaseConfig, DbEngine, ModelType, NodeData, NodeType,
        OutputType, Provider, Runtime, WriteMode,
    },
    providers::UreqHttp,
    review, storage, validate,
    workflow::{self, InputType},
};

const MAX_LIVE_TEXT: usize = 256 * 1024;
const MAX_LIVE_LOG: usize = 512 * 1024;

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
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Session {
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
                self.marks.insert(
                    id,
                    match state {
                        exec::NodeState::Running => "running",
                        exec::NodeState::Done => "ran",
                        exec::NodeState::Reused => "reused",
                        exec::NodeState::Failed => "failed",
                    },
                );
            }
            Progress::Log(line) => {
                let line = tail(&line, MAX_LIVE_TEXT);
                self.log_bytes += line.len();
                self.log.push(line);
                while self.log_bytes > MAX_LIVE_LOG && self.log.len() > 1 {
                    self.log_bytes -= self.log.remove(0).len();
                }
            }
            Progress::Text { id, text } => {
                let stream = self
                    .stream
                    .get_or_insert_with(|| (id.clone(), String::new()));
                if stream.0 != id {
                    *stream = (id, String::new());
                }
                stream.1.push_str(&text);
                stream.1 = tail(&stream.1, MAX_LIVE_TEXT);
            }
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
            "workflows":self.workflows,"review":pending,"run":self.run,"error":self.error})
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
        let id = string(args, "run_id")?;
        let graph = string(args, "graph")?;
        safe_name(graph)?;
        let live = lock(&self.current)
            .as_ref()
            .and_then(|s| lock(s).run.clone())
            .filter(|r| r.id == id && r.graph == graph);
        let run = live
            .or_else(|| {
                history::list(&self.root, graph)
                    .into_iter()
                    .find(|r| r.id == id)
            })
            .ok_or("the run is no longer stored")?;
        let mut run = &run;
        if let Some(path) = args.get("call_path").and_then(Value::as_str) {
            for id in path.split('/').filter(|id| !id.is_empty()) {
                run = &run.calls.get(id).ok_or("this workflow did not run")?.run;
            }
        }
        let node = run
            .nodes
            .get(string(args, "node")?)
            .ok_or("this node has no result in the run")?;
        let node = if let Some(output) = args.get("output").and_then(Value::as_str) {
            node.named_outputs
                .get(output)
                .ok_or("this output is not available")?
        } else {
            node
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
        Ok(
            json!({"label":out.label,"saved":out.saved,"text":out.as_text(),
            "media_type":(!out.text).then(||media_type(&out.bytes)),
            "base64":if out.text {None} else {Some(base64::engine::general_purpose::STANDARD.encode(&out.bytes))},
            "complete":complete}),
        )
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

fn value(value: impl serde::Serialize) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|e| e.to_string())
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
