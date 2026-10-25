//! The inspector: the panel to the right of the canvas that shows the selected node's
//! content as a form and writes every change straight back into the node.
//!
//! A blank node offers the content types; a typed node shows its type's fields, live
//! validation from [`NodeData::validate`], and a row to save the config into the library
//! (agents and databases) so it shows up in the sidebar for reuse. Agent prompts can be
//! saved to and loaded from `prompts/` on their own.
//!
//! Runs start here too. The executor works on a background thread under a
//! [`Control`]: the inspector owns the output [`Cache`] that lives across runs, the cancel
//! flag behind the Cancel button, and a progress sink the executor writes log lines and
//! streamed agent text into. A foreground task polls that sink a few times a second and
//! redraws, so the log and the text appear while the run is still going.
//!
//! Every node that took part in a run keeps its result here, so selecting a node in the
//! middle of a graph shows what it produced (or why it failed) in the last run that
//! included it, with a note when the node has been edited since.
//!
//! Every finished run is also written to the run history ([`history`]), listed in the
//! History tab. Opening one brings its outputs, dots and log back without running
//! anything, and the selected node's previous output is shown under its current one. Run
//! all starts every end node of the graph in one pass.
//!
//! The Name field at the top of the form names the node for the canvas and for the
//! `{{name}}` placeholders of agents wired after it; an agent's Message field is the user
//! message template, and its Inputs list says where each input ends up.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::form::{field, v_form};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme, Disableable, IndexPath, Sizable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::doc::GraphDoc;
use crate::exec::{self, Cache, Control, NodeState, Progress, Report};
use crate::graph::{Graph, GraphEvent, Mark, NodeId, data_color};
use crate::history;
use crate::library;
use crate::node_data::{
    AgentConfig, Auth, DataConfig, DataFormat, DatabaseConfig, DbEngine, DirectoryConfig,
    FileConfig, ModelType, NodeData, NodeType, OutputType, Provider, Runtime, ScriptConfig,
    WRITABLE_DATABASE_LABEL, WriteMode,
};
use crate::providers::{self, UreqHttp};
use crate::validate::{self, Issue};
use crate::{
    calls, review,
    workflow::{self, InputConfig, InputType},
};

/// Initial inspector width; the divider next to it resizes it.
pub const INSPECTOR_W: f32 = 370.0;
/// How often the panel pulls progress from a running job.
const PROGRESS_POLL: Duration = Duration::from_millis(80);
/// Characters of a text preview, streamed or final.
const PREVIEW_CHARS: usize = history::PREVIEW_CHARS;
/// Past runs listed in the panel; the rest stay on disk.
const RUNS_SHOWN: usize = 12;
const PUBLISHED_OUTPUTS: &str = "Published outputs";
const NODE_RESULT: &str = "Node result";

/// What the executor's thread writes and the panel reads while a run is going.
#[derive(Default)]
struct Live {
    workflows: HashMap<String, Arc<GraphDoc>>,
    review: Option<review::Pending>,
    log: Vec<String>,
    /// The node whose text is streaming in, and the text so far.
    stream: Option<(String, String)>,
    /// Node state changes, in order.
    states: Vec<(String, NodeState)>,
    /// Set once by the thread when the run is over, with the record it wrote.
    report: Option<(Report, history::Run)>,
}

impl Live {
    fn take_in(&mut self, progress: Progress) {
        match progress {
            Progress::Workflow { id, graph } => {
                self.workflows.insert(id, graph);
            }
            Progress::Log(line) => self.log.push(line),
            Progress::Text { id, text } => match &mut self.stream {
                Some((current, so_far)) if *current == id => so_far.push_str(&text),
                _ => self.stream = Some((id, text)),
            },
            Progress::Node { id, state } => self.states.push((id, state)),
        }
    }
}

/// A run in flight: its cancel flag and the state shared with its thread.
struct Job {
    cancel: Arc<AtomicBool>,
    live: Arc<Mutex<Live>>,
    /// The node Run was pressed on, or every end node for Run all.
    targets: Vec<NodeId>,
    /// The document the run works on, to remember each node's content at run time.
    doc: GraphDoc,
}

/// What a node produced the last time a run included it.
struct NodeResult {
    named_outputs: BTreeMap<String, exec::Output>,
    result: Result<exec::Output, String>,
    /// The node's content when it ran, to tell whether it has changed since.
    data: Option<NodeData>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

pub enum InspectorEvent {
    OpenWorkflow {
        node: NodeId,
        graph: GraphDoc,
        run: Option<Box<history::Run>>,
    },
    ExecutionChanged,
    /// A preset or prompt was written or removed; the sidebar should re-list.
    LibraryChanged,
    Status {
        text: String,
        error: bool,
    },
}

impl EventEmitter<InspectorEvent> for Inspector {}

type StrSelect = SelectState<Vec<SharedString>>;

struct RunField {
    field: workflow::Field,
    value: Entity<TextareaState>,
    choices: Option<Entity<StrSelect>>,
}

struct RunForm {
    doc: GraphDoc,
    targets: Vec<NodeId>,
    fresh: bool,
    fields: Vec<RunField>,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

struct CallField {
    field: workflow::Field,
    source: Entity<StrSelect>,
    value: Entity<TextareaState>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InspectorTab {
    Configure,
    Results,
    History,
}

pub struct ChildExecution {
    pub graph: GraphDoc,
    pub workflows: BTreeMap<String, GraphDoc>,
    pub running: bool,
    pub run: Option<history::Run>,
    pub marks: HashMap<NodeId, Mark>,
    pub log: Vec<String>,
    pub stream: Option<(String, String)>,
}

pub struct Inspector {
    tab: InspectorTab,
    preview: bool,
    preview_live: bool,
    preview_snapshot: bool,
    preview_run: Option<history::Run>,
    published: Entity<InputState>,
    publish_source: Entity<StrSelect>,
    publish_issue: Option<String>,
    show_advanced: bool,
    show_log: bool,
    workflow_graph: Entity<StrSelect>,
    workflow_output: Entity<StrSelect>,
    workflow_fields: Vec<CallField>,
    workflow_problem: Option<String>,
    workflow_subscriptions: Vec<Subscription>,
    workflow_details: bool,
    child_states: BTreeMap<String, NodeState>,
    scroll: ScrollHandle,
    run_form: Option<RunForm>,
    run_values: workflow::Values,
    pending_review: Option<review::Pending>,
    review_file: usize,
    review_page: usize,
    graph: Entity<Graph>,
    /// The node being edited, mirrored from the graph's selection.
    node: Option<NodeId>,
    /// Its content type, `None` for a blank node.
    kind: Option<NodeType>,
    issues: Vec<Issue>,
    /// Set while the form is being filled from a node so changes aren't written back.
    loading: bool,
    /// Saved outputs of the agent preset this node was made from.
    output_count: usize,
    /// The run in flight, if any.
    job: Option<Job>,
    /// Log lines and streamed text of the run in flight, mirrored for rendering.
    live_log: Vec<String>,
    live_stream: Option<(String, String)>,
    /// How many node state changes of the run in flight were applied to the canvas.
    live_states: usize,
    /// The canvas dots: one run's worth, the latest.
    marks: HashMap<NodeId, Mark>,
    /// The run shown: its report and the nodes it was started from.
    report: Option<(Vec<NodeId>, Report)>,
    /// What every node produced the last time a run included it.
    results: HashMap<NodeId, NodeResult>,
    /// Node outputs kept across runs, so a downstream run reuses upstream agents.
    cache: Arc<Mutex<Cache>>,
    /// Past runs of the current graph, newest first.
    history: Vec<history::Run>,
    /// The record whose outputs and dots are showing, when the run is one from the list.
    shown_run: Option<String>,
    /// The graph's name as saved, which its runs are filed under.
    graph_name: String,
    /// The selected node's own name.
    node_name: Entity<InputState>,
    preset_name: Entity<InputState>,
    // run input definition
    input_kind: Entity<StrSelect>,
    input_default: Entity<TextareaState>,
    input_required: bool,
    // agent
    provider: Entity<StrSelect>,
    endpoint: Entity<InputState>,
    model_type: Entity<StrSelect>,
    model: Entity<InputState>,
    auth_env: Entity<InputState>,
    prompt: Entity<TextareaState>,
    message: Entity<TextareaState>,
    prompt_name: Entity<InputState>,
    context_size: Entity<InputState>,
    temperature: Entity<InputState>,
    output: Entity<StrSelect>,
    // database
    engine: Entity<StrSelect>,
    db_path: Entity<InputState>,
    url_env: Entity<InputState>,
    input: Entity<InputState>,
    input_table: Entity<InputState>,
    query: Entity<TextareaState>,
    read_only: bool,
    /// Saved agents offered to write the query, the pick, the request and progress.
    sql_agents: Vec<library::Preset<AgentConfig>>,
    sql_agent: Entity<StrSelect>,
    sql_request: Entity<TextareaState>,
    writing_sql: bool,
    // script
    runtime: Entity<StrSelect>,
    script_path: Entity<InputState>,
    args: Entity<InputState>,
    timeout: Entity<InputState>,
    // data
    data_path: Entity<InputState>,
    format: Entity<StrSelect>,
    // directory
    dir_path: Entity<InputState>,
    dir_include: Entity<InputState>,
    dir_exclude: Entity<InputState>,
    dir_gitignore: bool,
    dir_contents: bool,
    dir_max_files: Entity<InputState>,
    dir_max_bytes: Entity<InputState>,
    // write file
    file_path: Entity<InputState>,
    file_mode: Entity<StrSelect>,
    file_unfence: bool,
    file_bundle: bool,
    file_review: bool,
    _subscriptions: Vec<Subscription>,
}

/// A dropdown over every value of an enum, shown by label.
fn enum_select<T: Copy>(
    all: &[T],
    label: fn(T) -> &'static str,
    window: &mut Window,
    cx: &mut Context<Inspector>,
) -> Entity<StrSelect> {
    let items: Vec<SharedString> = all.iter().map(|t| SharedString::from(label(*t))).collect();
    cx.new(|cx| SelectState::new(items, Some(IndexPath::new(0)), window, cx))
}

/// The enum value a dropdown currently shows.
fn chosen<T: Copy>(
    select: &Entity<StrSelect>,
    all: &[T],
    label: fn(T) -> &'static str,
    cx: &App,
) -> Option<T> {
    let value = select.read(cx).selected_value()?.clone();
    all.iter().copied().find(|t| label(*t) == value.as_ref())
}

fn choose<T: Copy + PartialEq>(
    select: &Entity<StrSelect>,
    all: &[T],
    value: T,
    window: &mut Window,
    cx: &mut App,
) {
    let index = all.iter().position(|t| *t == value).unwrap_or(0);
    select.update(cx, |select, cx| {
        select.set_selected_index(Some(IndexPath::new(index)), window, cx)
    });
}

/// The editor id behind a document id such as `n3`.
fn node_id(doc_id: &str) -> Option<NodeId> {
    doc_id.strip_prefix('n')?.parse().ok()
}

fn set_text(state: &Entity<InputState>, value: String, window: &mut Window, cx: &mut App) {
    state.update(cx, |state, cx| state.set_value(value, window, cx));
}

fn set_textarea(state: &Entity<TextareaState>, value: String, window: &mut Window, cx: &mut App) {
    state.update(cx, |state, cx| state.set_value(value, window, cx));
}

fn set_picker(
    state: &Entity<StrSelect>,
    items: Vec<String>,
    selected: Option<usize>,
    window: &mut Window,
    cx: &mut App,
) {
    state.update(cx, |state, cx| {
        state.set_items(
            items.into_iter().map(SharedString::from).collect(),
            window,
            cx,
        );
        state.set_selected_index(selected.map(IndexPath::new), window, cx);
    });
}

/// What the endpoint field suggests for a provider.
fn endpoint_placeholder(provider: Provider) -> String {
    match provider.default_endpoint() {
        "" => "http://localhost:8000/v1".to_string(),
        url => url.to_string(),
    }
}

fn text_of(state: &Entity<InputState>, cx: &App) -> String {
    state.read(cx).value().trim().to_string()
}

/// Parse a numeric field, keeping the previous value and recording an issue on failure.
fn parse_field<T: FromStr>(
    state: &Entity<InputState>,
    prev: T,
    field: &'static str,
    issues: &mut Vec<Issue>,
    cx: &App,
) -> T {
    let text = text_of(state, cx);
    if text.is_empty() {
        issues.push(Issue::new(field, format!("{field} is empty")));
        return prev;
    }
    match text.parse() {
        Ok(value) => value,
        Err(_) => {
            issues.push(Issue::new(field, format!("{field} must be a number")));
            prev
        }
    }
}

impl Inspector {
    pub fn new(graph: Entity<Graph>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let text = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let area = |placeholder: &'static str,
                    rows: usize,
                    window: &mut Window,
                    cx: &mut Context<Self>| {
            cx.new(|cx| {
                TextareaState::new(window, cx)
                    .placeholder(placeholder)
                    .rows(rows)
            })
        };

        let node_name = text("name, for {{name}} in agent prompts", window, cx);
        let published = text("summary, details", window, cx);
        let publish_source = cx.new(|cx| {
            SelectState::new(
                vec![SharedString::from(NODE_RESULT)],
                Some(IndexPath::new(0)),
                window,
                cx,
            )
        });
        let preset_name = text("preset name", window, cx);
        let input_kind = enum_select(&InputType::ALL, InputType::label, window, cx);
        let input_default = area("Optional default for the Run form", 3, window, cx);
        let workflow_graph =
            cx.new(|cx| SelectState::new(Vec::<SharedString>::new(), None, window, cx));
        let workflow_output =
            cx.new(|cx| SelectState::new(Vec::<SharedString>::new(), None, window, cx));
        let provider = enum_select(&Provider::ALL, Provider::label, window, cx);
        let endpoint = text(Provider::Ollama.default_endpoint(), window, cx);
        let model_type = enum_select(&ModelType::ALL, ModelType::label, window, cx);
        let model = text("model id", window, cx);
        let auth_env = text("ANTHROPIC_API_KEY", window, cx);
        let prompt = area("You are…", 6, window, cx);
        let message = area(
            "Empty sends the inputs. {{name}} puts an input here.",
            3,
            window,
            cx,
        );
        let prompt_name = text("prompt name", window, cx);
        let context_size = text("8192", window, cx);
        let temperature = text("0.7", window, cx);
        let output = enum_select(&OutputType::ALL, OutputType::label, window, cx);
        let engine = enum_select(&DbEngine::ALL, DbEngine::label, window, cx);
        let db_path = text("file.sqlite (inside data/)", window, cx);
        let url_env = text("DATABASE_URL", window, cx);
        let input = text("optional: table.csv (inside data/)", window, cx);
        let input_table = text("input", window, cx);
        let query = area("SELECT …", 5, window, cx);
        let sql_agent = cx.new(|cx| SelectState::new(Vec::<SharedString>::new(), None, window, cx));
        let sql_request = area("What should the query do?", 3, window, cx);
        let runtime = enum_select(&Runtime::ALL, Runtime::label, window, cx);
        let script_path = text("etl.py (inside scripts/)", window, cx);
        let args = text(
            r#"arguments: quote "words that belong together""#,
            window,
            cx,
        );
        let timeout = text("60", window, cx);
        let data_path = text("sales.csv (inside data/)", window, cx);
        let format = enum_select(&DataFormat::ALL, DataFormat::label, window, cx);
        let dir_path = text(". (the working directory)", window, cx);
        let dir_include = text("src/**/*.rs *.md (empty takes every file)", window, cx);
        let dir_exclude = text("patterns to skip", window, cx);
        let dir_max_files = text("200", window, cx);
        let dir_max_bytes = text("262144", window, cx);
        let file_path = text(
            "tests/generated.rs (inside the working directory)",
            window,
            cx,
        );
        let file_mode = enum_select(&WriteMode::ALL, WriteMode::label, window, cx);

        let mut subscriptions =
            vec![
                cx.subscribe_in(&graph, window, |this, _, event: &GraphEvent, window, cx| {
                    match event {
                        GraphEvent::SelectionChanged => this.reload(window, cx),
                        GraphEvent::Replaced => this.forget_results(cx),
                        GraphEvent::OpenWorkflow(id) => this.open_workflow(*id, cx),
                        GraphEvent::Changed => cx.notify(),
                    }
                }),
            ];

        // Plain fields: every edit is written back to the node.
        for state in [
            &model,
            &endpoint,
            &auth_env,
            &context_size,
            &temperature,
            &db_path,
            &url_env,
            &input,
            &input_table,
            &args,
            &timeout,
            &dir_path,
            &dir_include,
            &dir_exclude,
            &dir_max_files,
            &dir_max_bytes,
            &file_path,
        ] {
            subscriptions.push(cx.subscribe_in(
                state,
                window,
                |this, _, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.sync(cx);
                    }
                },
            ));
        }
        for state in [&prompt, &message, &query, &input_default] {
            subscriptions.push(cx.subscribe_in(
                state,
                window,
                |this, _, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.sync(cx);
                    }
                },
            ));
        }
        // The name is not part of the node's content: it goes to the graph directly.
        subscriptions.push(cx.subscribe_in(
            &publish_source,
            window,
            |this, _, _: &SelectEvent<Vec<SharedString>>, window, cx| {
                if !this.loading {
                    this.load_published_names(window, cx);
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &published,
            window,
            |this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.publish_outputs(cx);
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &node_name,
            window,
            |this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.on_name_change(cx);
                }
            },
        ));
        // Paths pick a matching runtime or format as they change.
        subscriptions.push(cx.subscribe_in(
            &script_path,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.on_script_path_change(window, cx);
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &data_path,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.on_data_path_change(window, cx);
                }
            },
        ));
        // Dropdowns.
        for state in [&output, &engine, &runtime, &format, &file_mode, &input_kind] {
            subscriptions.push(cx.subscribe_in(
                state,
                window,
                |this, _, _: &SelectEvent<Vec<SharedString>>, _, cx| this.sync(cx),
            ));
        }
        subscriptions.push(cx.subscribe_in(
            &workflow_graph,
            window,
            |this, _, _: &SelectEvent<Vec<SharedString>>, window, cx| {
                this.change_workflow(true, window, cx)
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &workflow_output,
            window,
            |this, _, _: &SelectEvent<Vec<SharedString>>, window, cx| {
                this.change_workflow(false, window, cx)
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &provider,
            window,
            |this, _, _: &SelectEvent<Vec<SharedString>>, window, cx| {
                this.on_provider_change(window, cx)
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &model_type,
            window,
            |this, _, _: &SelectEvent<Vec<SharedString>>, window, cx| {
                this.on_model_type_change(window, cx)
            },
        ));
        // Enter in the name fields saves.
        subscriptions.push(cx.subscribe_in(
            &preset_name,
            window,
            |this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.save_preset(cx);
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &prompt_name,
            window,
            |this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.save_prompt(cx);
                }
            },
        ));

        let mut inspector = Self {
            tab: InspectorTab::Configure,
            preview: false,
            preview_live: false,
            preview_snapshot: false,
            preview_run: None,
            published,
            publish_source,
            publish_issue: None,
            show_advanced: false,
            show_log: false,
            workflow_graph,
            workflow_output,
            workflow_fields: Vec::new(),
            workflow_problem: None,
            workflow_subscriptions: Vec::new(),
            workflow_details: false,
            child_states: BTreeMap::new(),
            scroll: ScrollHandle::new(),
            run_form: None,
            run_values: workflow::Values::new(),
            pending_review: None,
            review_file: 0,
            review_page: 0,
            graph,
            node: None,
            kind: None,
            issues: Vec::new(),
            loading: false,
            output_count: 0,
            job: None,
            live_log: Vec::new(),
            live_stream: None,
            live_states: 0,
            marks: HashMap::new(),
            report: None,
            results: HashMap::new(),
            cache: Arc::new(Mutex::new(Cache::persistent(exec::Dirs::from_cwd().cache))),
            history: Vec::new(),
            shown_run: None,
            graph_name: String::new(),
            node_name,
            preset_name,
            input_kind,
            input_default,
            input_required: true,
            provider,
            endpoint,
            model_type,
            model,
            auth_env,
            prompt,
            message,
            prompt_name,
            context_size,
            temperature,
            output,
            engine,
            db_path,
            url_env,
            input,
            input_table,
            query,
            read_only: true,
            sql_agents: Vec::new(),
            sql_agent,
            sql_request,
            writing_sql: false,
            runtime,
            script_path,
            args,
            timeout,
            data_path,
            format,
            dir_path,
            dir_include,
            dir_exclude,
            dir_gitignore: true,
            dir_contents: true,
            dir_max_files,
            dir_max_bytes,
            file_path,
            file_mode,
            file_unfence: true,
            file_bundle: false,
            file_review: false,
            _subscriptions: subscriptions,
        };
        inspector.reload(window, cx);
        inspector.set_graph_name("untitled", cx);
        inspector
    }

    fn status(&mut self, text: String, error: bool, cx: &mut Context<Self>) {
        cx.emit(InspectorEvent::Status { text, error });
    }

    /// A different graph is loaded: node ids mean other nodes now.
    fn forget_results(&mut self, cx: &mut Context<Self>) {
        self.preview_run = None;
        self.preview_live = false;
        self.child_states.clear();
        if let Some(job) = self.job.take() {
            job.cancel.store(true, Ordering::Relaxed);
        }
        self.run_form = None;
        self.run_values.clear();
        self.pending_review = None;
        self.live_log.clear();
        self.live_stream = None;
        self.results.clear();
        self.report = None;
        self.marks.clear();
        self.shown_run = None;
        cx.notify();
    }

    /// The graph was loaded, saved or started afresh under `name`: its runs are filed
    /// under that name, so list them.
    pub fn set_graph_name(&mut self, name: &str, cx: &mut Context<Self>) {
        let name = history::graph_name(name);
        if name == self.graph_name {
            return;
        }
        self.graph_name = name;
        self.history = history::list(Path::new("."), &self.graph_name);
        self.run_values = self
            .history
            .first()
            .map(|run| run.inputs.clone())
            .unwrap_or_default();
        self.shown_run = None;
        cx.notify();
    }

    pub fn running(&self) -> bool {
        self.job.is_some() || self.run_form.is_some()
    }

    pub fn needs_review(&self) -> bool {
        self.pending_review.is_some()
    }

    pub fn set_preview(&mut self, run: Option<history::Run>, cx: &mut Context<Self>) {
        self.preview = true;
        self.preview_live = false;
        self.preview_snapshot = run.is_some();
        self.graph
            .update(cx, |graph, cx| graph.set_read_only(true, cx));
        self.results.clear();
        self.marks.clear();
        if let Some(run) = &run {
            let report = run.report(Some(&self.cache));
            self.remember(
                &|id| run.nodes.get(id).and_then(|n| n.data.clone()),
                &report,
                cx,
            );
            self.report = Some((
                run.targets.iter().filter_map(|id| node_id(id)).collect(),
                report,
            ));
            self.tab = InspectorTab::Results;
        } else {
            self.report = None;
            self.tab = InspectorTab::Configure;
        }
        self.preview_run = run;
        cx.notify();
    }

    pub fn set_live_preview(&mut self, execution: ChildExecution, cx: &mut Context<Self>) {
        self.graph.update(cx, |graph, cx| {
            graph.set_workflow_snapshots(&execution.workflows, cx)
        });
        if let Some(run) = execution.run {
            if self.preview_run.as_ref().map(|r| &r.id) != Some(&run.id) {
                self.set_preview(Some(run), cx);
            }
            return;
        }
        self.preview = true;
        if !self.preview_snapshot {
            self.tab = InspectorTab::Results;
        }
        self.preview_snapshot = true;
        self.preview_live = execution.running;
        self.live_log = execution.log;
        self.live_stream = execution.stream;
        self.graph
            .update(cx, |graph, cx| graph.set_marks(execution.marks, cx));
        cx.notify();
    }

    pub fn child_execution(&self, path: &str) -> Option<ChildExecution> {
        if let Some(job) = &self.job {
            let live = lock(&job.live);
            let graph = live.workflows.get(path)?.as_ref().clone();
            let prefix = format!("{path}/");
            let marks = live
                .states
                .iter()
                .filter_map(|(id, state)| {
                    let id = node_id(id.strip_prefix(&prefix)?)?;
                    Some((
                        id,
                        match state {
                            NodeState::Running => Mark::Running,
                            NodeState::Done => Mark::Ran,
                            NodeState::Reused => Mark::Reused,
                            NodeState::Failed => Mark::Failed,
                        },
                    ))
                })
                .collect();
            return Some(ChildExecution {
                graph,
                running: true,
                workflows: live
                    .workflows
                    .iter()
                    .filter_map(|(id, graph)| {
                        let id = id.strip_prefix(&prefix)?;
                        (!id.contains('/')).then(|| (id.to_string(), graph.as_ref().clone()))
                    })
                    .collect(),
                run: None,
                marks,
                log: live
                    .log
                    .iter()
                    .filter_map(|line| line.strip_prefix(&prefix).map(str::to_string))
                    .collect(),
                stream: live.stream.as_ref().and_then(|(id, text)| {
                    Some((id.strip_prefix(&prefix)?.to_string(), text.clone()))
                }),
            });
        }
        let (_, report) = self.report.as_ref()?;
        let mut calls = &report.calls;
        let mut found = None;
        for id in path.split('/') {
            found = calls.get(id);
            let Some(call) = found else { break };
            calls = &call.run.calls;
        }
        let graph = report
            .workflows
            .get(path)
            .cloned()
            .or_else(|| found.map(|call| call.graph.clone()))?;
        let prefix = format!("{path}/");
        let mut workflows: BTreeMap<_, _> = report
            .workflows
            .iter()
            .filter_map(|(id, graph)| {
                let id = id.strip_prefix(&prefix)?;
                (!id.contains('/')).then(|| (id.to_string(), graph.clone()))
            })
            .collect();
        if let Some(call) = found {
            workflows.extend(
                call.run
                    .calls
                    .iter()
                    .map(|(id, call)| (id.clone(), call.graph.clone())),
            );
        }
        Some(ChildExecution {
            graph,
            workflows,
            running: false,
            run: found.map(|call| call.run.clone()),
            marks: HashMap::new(),
            log: found.map(|call| call.run.log.clone()).unwrap_or_default(),
            stream: None,
        })
    }

    pub fn open_workflow(&mut self, id: NodeId, cx: &mut Context<Self>) {
        let key = format!("n{id}");
        if let Some(execution) = self.child_execution(&key) {
            cx.emit(InspectorEvent::OpenWorkflow {
                node: id,
                graph: execution.graph,
                run: execution.run.map(Box::new),
            });
            return;
        }
        if self.job.is_some() {
            self.status("This child has not started yet. Its run snapshot will be available when it starts.".into(), false, cx);
            return;
        }
        if self.preview_snapshot {
            // The workspace resolves this path from the root run's fixed snapshots.
            cx.emit(InspectorEvent::OpenWorkflow {
                node: id,
                graph: GraphDoc::default(),
                run: None,
            });
            return;
        }
        if self.shown_run.is_some() {
            self.status(
                "No child snapshot was recorded for this node in the displayed run.".into(),
                false,
                cx,
            );
            return;
        }
        let Some(NodeData::Workflow(config)) = self.graph.read(cx).node_data(id) else {
            return;
        };
        match crate::doc::load_under(Path::new("."), &config.graph) {
            Ok(graph) => cx.emit(InspectorEvent::OpenWorkflow {
                node: id,
                graph,
                run: None,
            }),
            Err(error) => self.status(error, true, cx),
        }
    }

    fn publish_outputs(&mut self, cx: &mut Context<Self>) {
        if self.loading || self.preview {
            return;
        }
        let Some(id) = self.node else {
            return;
        };
        let names: Vec<String> = self
            .published
            .read(cx)
            .value()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        let port = self
            .publish_source
            .read(cx)
            .selected_value()
            .filter(|name| name.as_ref() != NODE_RESULT)
            .map(ToString::to_string);
        self.publish_issue = self
            .graph
            .update(cx, |graph, cx| match port.as_deref() {
                Some(port) => graph.publish_output_port(id, Some(port), &names, cx),
                None => graph.publish_outputs(id, &names, cx),
            })
            .err();
        cx.notify();
    }

    fn load_published_names(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.node else {
            return;
        };
        let port = self
            .publish_source
            .read(cx)
            .selected_value()
            .filter(|name| name.as_ref() != NODE_RESULT)
            .map(ToString::to_string);
        let names = self
            .graph
            .read(cx)
            .published_port_names(id, port.as_deref());
        let loading = self.loading;
        self.loading = true;
        set_text(&self.published, names.join(", "), window, cx);
        self.publish_issue = None;
        self.loading = loading;
        cx.notify();
    }

    fn push_marks(&self, cx: &mut Context<Self>) {
        let marks = self.marks.clone();
        self.graph
            .update(cx, |graph, cx| graph.set_marks(marks, cx));
    }

    /// The target and everything wired into it, directly or not.
    fn run_members(doc: &GraphDoc, target: NodeId) -> Vec<NodeId> {
        let mut members = vec![target];
        let mut i = 0;
        while i < members.len() {
            let current = format!("n{}", members[i]);
            for edge in doc.edges.iter().filter(|e| e.to == current) {
                if let Some(from) = node_id(&edge.from)
                    && !members.contains(&from)
                {
                    members.push(from);
                }
            }
            i += 1;
        }
        members
    }

    // ----- graph <-> form -----

    /// Fill the form from whatever the graph has selected.
    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.loading = true;
        let selected = {
            let graph = self.graph.read(cx);
            graph.selected().map(|v| {
                (
                    v.id,
                    v.preset.clone(),
                    v.name.map(str::to_string),
                    v.data.cloned(),
                )
            })
        };
        match selected {
            None => {
                self.node = None;
                self.kind = None;
                self.issues.clear();
                self.output_count = 0;
            }
            Some((id, preset, name, data)) => {
                self.node = Some(id);
                let ports = self.graph.read(cx).output_ports(id);
                let mut sources = if ports.is_empty() {
                    vec![NODE_RESULT.to_string()]
                } else {
                    ports
                };
                let doc = self.graph.read(cx).snapshot(&self.graph_name);
                if doc
                    .outputs
                    .values()
                    .any(|target| target == &format!("n{id}"))
                    && !sources.iter().any(|source| source == NODE_RESULT)
                {
                    sources.push(NODE_RESULT.to_string());
                }
                let prefix = format!("n{id}.");
                for target in doc.outputs.values() {
                    if let Some(port) = target.strip_prefix(&prefix)
                        && !sources.iter().any(|source| source == port)
                    {
                        sources.push(port.to_string());
                    }
                }
                set_picker(&self.publish_source, sources, Some(0), window, cx);
                self.load_published_names(window, cx);
                set_text(&self.node_name, name.unwrap_or_default(), window, cx);
                set_text(
                    &self.preset_name,
                    preset.as_ref().map(|p| p.to_string()).unwrap_or_default(),
                    window,
                    cx,
                );
                self.kind = data.as_ref().map(NodeData::node_type);
                if self.kind == Some(NodeType::Database) {
                    self.refresh_sql_agents(window, cx);
                }
                self.output_count = match (&preset, &self.kind) {
                    (Some(name), Some(NodeType::Agent)) => library::agent_output_count(name),
                    _ => 0,
                };
                match data {
                    None => self.issues.clear(),
                    Some(data) => {
                        self.fill(&data, window, cx);
                        self.issues = data.validate();
                    }
                }
            }
        }
        self.loading = false;
        cx.notify();
    }

    fn fill(&mut self, data: &NodeData, window: &mut Window, cx: &mut Context<Self>) {
        match data {
            NodeData::Workflow(config) => self.fill_workflow(config, window, cx),
            NodeData::Input(input) => {
                choose(&self.input_kind, &InputType::ALL, input.kind, window, cx);
                set_textarea(&self.input_default, input.default.clone(), window, cx);
                self.input_required = input.required;
            }
            NodeData::Agent(a) => {
                choose(&self.provider, &Provider::ALL, a.provider, window, cx);
                set_text(&self.endpoint, a.endpoint.clone(), window, cx);
                self.endpoint.update(cx, |state, cx| {
                    state.set_placeholder(endpoint_placeholder(a.provider), window, cx)
                });
                choose(&self.model_type, &ModelType::ALL, a.model_type, window, cx);
                set_text(&self.model, a.model.clone(), window, cx);
                set_text(&self.auth_env, a.auth.env_var().to_string(), window, cx);
                set_textarea(&self.prompt, a.prompt.clone(), window, cx);
                set_textarea(&self.message, a.message.clone(), window, cx);
                set_text(&self.context_size, a.context_size.to_string(), window, cx);
                set_text(&self.temperature, a.temperature.to_string(), window, cx);
                choose(&self.output, &OutputType::ALL, a.output, window, cx);
            }
            NodeData::Database(d) => {
                choose(&self.engine, &DbEngine::ALL, d.engine, window, cx);
                set_text(&self.db_path, d.path.clone(), window, cx);
                set_text(&self.url_env, d.url_env.clone(), window, cx);
                set_text(&self.input, d.input.clone(), window, cx);
                set_text(&self.input_table, d.input_table.clone(), window, cx);
                set_textarea(&self.query, d.query.clone(), window, cx);
                self.read_only = d.read_only;
            }
            NodeData::Script(s) => {
                choose(&self.runtime, &Runtime::ALL, s.runtime, window, cx);
                set_text(&self.script_path, s.path.clone(), window, cx);
                set_text(&self.args, validate::join_args(&s.args), window, cx);
                set_text(&self.timeout, s.timeout_secs.to_string(), window, cx);
            }
            NodeData::Data(d) => {
                set_text(&self.data_path, d.path.clone(), window, cx);
                choose(&self.format, &DataFormat::ALL, d.format, window, cx);
            }
            NodeData::Directory(d) => {
                set_text(&self.dir_path, d.path.clone(), window, cx);
                set_text(&self.dir_include, d.include.join(" "), window, cx);
                set_text(&self.dir_exclude, d.exclude.join(" "), window, cx);
                self.dir_gitignore = d.gitignore;
                self.dir_contents = d.contents;
                set_text(&self.dir_max_files, d.max_files.to_string(), window, cx);
                set_text(&self.dir_max_bytes, d.max_bytes.to_string(), window, cx);
            }
            NodeData::File(f) => {
                set_text(&self.file_path, f.path.clone(), window, cx);
                choose(&self.file_mode, &WriteMode::ALL, f.mode, window, cx);
                self.file_unfence = f.unfence;
                self.file_bundle = f.bundle;
                self.file_review = f.review;
            }
        }
    }

    /// Write the form into the selected node and re-validate.
    fn sync(&mut self, cx: &mut Context<Self>) {
        if self.loading || self.preview {
            return;
        }
        let (Some(id), Some(kind)) = (self.node, self.kind) else {
            return;
        };
        let prev = self.graph.read(cx).node_data(id).cloned();
        let (data, mut issues) = self.collect(kind, prev, cx);
        issues.extend(data.validate());
        self.issues = issues;
        self.graph
            .update(cx, |graph, cx| graph.set_node_data(id, Some(data), cx));
        cx.notify();
    }

    /// Build the node's content from the form. Numbers that don't parse keep their previous
    /// value and are reported as issues.
    fn collect(&self, kind: NodeType, prev: Option<NodeData>, cx: &App) -> (NodeData, Vec<Issue>) {
        let mut issues = Vec::new();
        let data = match kind {
            NodeType::Workflow => {
                let inputs = self
                    .workflow_fields
                    .iter()
                    .filter_map(|field| {
                        let source = field.source.read(cx).selected_value()?;
                        let binding = match source.as_ref() {
                            "Use default" => return None,
                            "Value" => calls::Binding::Value {
                                value: field.value.read(cx).value().to_string(),
                            },
                            label => calls::Binding::Output {
                                node: label.split(" · ").next().unwrap_or(label).to_string(),
                            },
                        };
                        Some((field.field.name.clone(), binding))
                    })
                    .collect();
                // Preserve mappings when a saved graph is temporarily missing/unreadable.
                let inputs = if self.workflow_problem.is_some() {
                    match prev {
                        Some(NodeData::Workflow(config)) => config.inputs,
                        _ => inputs,
                    }
                } else {
                    inputs
                };
                NodeData::Workflow(calls::Config {
                    graph: self
                        .workflow_graph
                        .read(cx)
                        .selected_value()
                        .map(ToString::to_string)
                        .unwrap_or_default(),
                    output: self
                        .workflow_output
                        .read(cx)
                        .selected_value()
                        .map(|s| {
                            if s.starts_with(PUBLISHED_OUTPUTS) {
                                String::new()
                            } else {
                                s.split(" · ").next().unwrap_or_default().to_string()
                            }
                        })
                        .unwrap_or_default(),
                    inputs,
                })
            }
            NodeType::Input => NodeData::Input(InputConfig {
                kind: chosen(&self.input_kind, &InputType::ALL, InputType::label, cx)
                    .unwrap_or_default(),
                default: self.input_default.read(cx).value().to_string(),
                required: self.input_required,
            }),
            NodeType::Agent => {
                let prev = match prev {
                    Some(NodeData::Agent(a)) => a,
                    _ => AgentConfig::default(),
                };
                let env = text_of(&self.auth_env, cx);
                NodeData::Agent(AgentConfig {
                    provider: chosen(&self.provider, &Provider::ALL, Provider::label, cx)
                        .unwrap_or(prev.provider),
                    endpoint: text_of(&self.endpoint, cx),
                    model_type: chosen(&self.model_type, &ModelType::ALL, ModelType::label, cx)
                        .unwrap_or(prev.model_type),
                    model: text_of(&self.model, cx),
                    auth: if env.is_empty() {
                        Auth::None
                    } else {
                        Auth::Env { var: env }
                    },
                    prompt: self.prompt.read(cx).value().to_string(),
                    message: self.message.read(cx).value().to_string(),
                    context_size: parse_field(
                        &self.context_size,
                        prev.context_size,
                        "context size",
                        &mut issues,
                        cx,
                    ),
                    temperature: parse_field(
                        &self.temperature,
                        prev.temperature,
                        "temperature",
                        &mut issues,
                        cx,
                    ),
                    output: chosen(&self.output, &OutputType::ALL, OutputType::label, cx)
                        .unwrap_or(prev.output),
                })
            }
            NodeType::Database => {
                let prev = match prev {
                    Some(NodeData::Database(d)) => d,
                    _ => DatabaseConfig::default(),
                };
                NodeData::Database(DatabaseConfig {
                    engine: chosen(&self.engine, &DbEngine::ALL, DbEngine::label, cx)
                        .unwrap_or(prev.engine),
                    path: text_of(&self.db_path, cx),
                    url_env: text_of(&self.url_env, cx),
                    input: text_of(&self.input, cx),
                    input_table: text_of(&self.input_table, cx),
                    query: self.query.read(cx).value().to_string(),
                    read_only: self.read_only,
                })
            }
            NodeType::Script => {
                let prev = match prev {
                    Some(NodeData::Script(s)) => s,
                    _ => ScriptConfig::default(),
                };
                NodeData::Script(ScriptConfig {
                    runtime: chosen(&self.runtime, &Runtime::ALL, Runtime::label, cx)
                        .unwrap_or(prev.runtime),
                    path: text_of(&self.script_path, cx),
                    args: match validate::split_args(&text_of(&self.args, cx)) {
                        Ok(args) => args,
                        Err(e) => {
                            issues.push(Issue::new("arguments", e));
                            prev.args
                        }
                    },
                    timeout_secs: parse_field(
                        &self.timeout,
                        prev.timeout_secs,
                        "timeout",
                        &mut issues,
                        cx,
                    ),
                })
            }
            NodeType::Data => {
                let prev = match prev {
                    Some(NodeData::Data(d)) => d,
                    _ => DataConfig::default(),
                };
                NodeData::Data(DataConfig {
                    path: text_of(&self.data_path, cx),
                    format: chosen(&self.format, &DataFormat::ALL, DataFormat::label, cx)
                        .unwrap_or(prev.format),
                })
            }
            NodeType::Directory => {
                let prev = match prev {
                    Some(NodeData::Directory(d)) => d,
                    _ => DirectoryConfig::default(),
                };
                let patterns = |state: &Entity<InputState>| -> Vec<String> {
                    text_of(state, cx)
                        .split_whitespace()
                        .map(str::to_string)
                        .collect()
                };
                NodeData::Directory(DirectoryConfig {
                    path: text_of(&self.dir_path, cx),
                    include: patterns(&self.dir_include),
                    exclude: patterns(&self.dir_exclude),
                    gitignore: self.dir_gitignore,
                    contents: self.dir_contents,
                    max_files: parse_field(
                        &self.dir_max_files,
                        prev.max_files,
                        "max files",
                        &mut issues,
                        cx,
                    ),
                    max_bytes: parse_field(
                        &self.dir_max_bytes,
                        prev.max_bytes,
                        "max bytes",
                        &mut issues,
                        cx,
                    ),
                })
            }
            NodeType::File => {
                let prev = match prev {
                    Some(NodeData::File(f)) => f,
                    _ => FileConfig::default(),
                };
                NodeData::File(FileConfig {
                    path: text_of(&self.file_path, cx),
                    mode: chosen(&self.file_mode, &WriteMode::ALL, WriteMode::label, cx)
                        .unwrap_or(prev.mode),
                    unfence: self.file_unfence,
                    bundle: self.file_bundle,
                    review: self.file_review,
                })
            }
        };
        (data, issues)
    }

    fn fill_workflow(
        &mut self,
        config: &calls::Config,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let loading = self.loading;
        self.loading = true;
        self.workflow_fields.clear();
        self.workflow_subscriptions.clear();
        self.workflow_problem = None;
        let mut names = crate::doc::list();
        if !config.graph.is_empty() && !names.contains(&config.graph) {
            names.push(config.graph.clone());
        }
        let selected = names.iter().position(|n| n == &config.graph);
        set_picker(&self.workflow_graph, names, selected, window, cx);
        set_picker(
            &self.workflow_output,
            if config.output.is_empty() {
                Vec::new()
            } else {
                vec![format!("{} · saved output", config.output)]
            },
            (!config.output.is_empty()).then_some(0),
            window,
            cx,
        );
        let loaded = crate::doc::load_under(Path::new("."), &config.graph);
        let result = loaded.and_then(|doc| {
            let mut outputs = vec![format!("{PUBLISHED_OUTPUTS} · {}", doc.outputs.len())];
            outputs.extend(
                doc.nodes
                    .iter()
                    .filter(|n| n.data.is_some())
                    .map(|n| {
                        format!(
                            "{} · {}",
                            n.id,
                            n.name.as_deref().unwrap_or_else(|| n
                                .data
                                .as_ref()
                                .map(NodeData::label)
                                .unwrap_or("node"))
                        )
                    })
                    .collect::<Vec<_>>(),
            );
            let mut selected = if config.output.is_empty() {
                Some(0)
            } else {
                doc.nodes
                    .iter()
                    .filter(|n| n.data.is_some())
                    .position(|n| n.id == config.output)
                    .map(|i| i + 1)
            };
            if selected.is_none() && !config.output.is_empty() {
                selected = Some(outputs.len());
                outputs.push(format!("{} · missing node", config.output));
            }
            set_picker(&self.workflow_output, outputs, selected, window, cx);
            if config.output.is_empty() {
                if doc.outputs.is_empty() {
                    return Err(
                        "Publish named outputs in the child graph, or select one node to return."
                            .into(),
                    );
                }
                let targets = doc
                    .outputs
                    .values()
                    .map(|handle| handle.split('.').next().unwrap_or(handle).to_string())
                    .collect::<Vec<_>>();
                return workflow::fields(&doc, &targets);
            }
            let node = doc
                .nodes
                .iter()
                .find(|n| n.id == config.output)
                .ok_or("choose an output node")?;
            if node.data.is_none() {
                return Err("the output node is blank".into());
            }
            workflow::fields(&doc, std::slice::from_ref(&config.output))
        });
        match result {
            Err(error) => {
                if config.graph.is_empty() {
                    set_picker(&self.workflow_output, Vec::new(), None, window, cx);
                }
                self.workflow_problem = Some(error);
            }
            Ok(fields) => {
                let graph = self.graph.read(cx);
                let upstream = self
                    .node
                    .map(|id| graph.input_handles(id))
                    .unwrap_or_default();
                let wire_labels: Vec<_> = upstream
                    .iter()
                    .map(|handle| format!("{handle} · {}", graph.input_label(handle)))
                    .collect();
                for field in fields {
                    let binding = config.inputs.get(&field.name);
                    let mut sources = vec!["Use default".to_string(), "Value".to_string()];
                    sources.extend(wire_labels.clone());
                    let index = match binding {
                        None => 0,
                        Some(calls::Binding::Value { .. }) => 1,
                        Some(calls::Binding::Output { node }) => sources
                            .iter()
                            .position(|s| s.split(" · ").next() == Some(node))
                            .unwrap_or_else(|| {
                                sources.push(format!("{node} · wire missing"));
                                sources.len() - 1
                            }),
                    };
                    let value = match binding {
                        Some(calls::Binding::Value { value }) => value.clone(),
                        _ => field.config.default.clone(),
                    };
                    let source = cx.new(|cx| {
                        SelectState::new(
                            sources
                                .into_iter()
                                .map(SharedString::from)
                                .collect::<Vec<_>>(),
                            Some(IndexPath::new(index)),
                            window,
                            cx,
                        )
                    });
                    let value =
                        cx.new(|cx| TextareaState::new(window, cx).rows(3).default_value(value));
                    self.workflow_subscriptions.push(cx.subscribe_in(
                        &source,
                        window,
                        |this, _, _: &SelectEvent<Vec<SharedString>>, _, cx| this.sync(cx),
                    ));
                    self.workflow_subscriptions.push(cx.subscribe_in(
                        &value,
                        window,
                        |this, _, event: &InputEvent, _, cx| {
                            if matches!(event, InputEvent::Change) {
                                this.sync(cx);
                            }
                        },
                    ));
                    self.workflow_fields.push(CallField {
                        field,
                        source,
                        value,
                    });
                }
            }
        }
        self.loading = loading;
    }

    fn change_workflow(
        &mut self,
        changed_graph: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.loading || self.preview || self.kind != Some(NodeType::Workflow) {
            return;
        }
        let Some(id) = self.node else {
            return;
        };
        let Some(NodeData::Workflow(mut config)) = self.graph.read(cx).node_data(id).cloned()
        else {
            return;
        };
        if changed_graph {
            let name = self
                .workflow_graph
                .read(cx)
                .selected_value()
                .map(ToString::to_string)
                .unwrap_or_default();
            if config.graph == name {
                return;
            }
            config.graph = name;
            config.inputs.clear();
            config.output = crate::doc::load_under(Path::new("."), &config.graph)
                .ok()
                .and_then(|doc| {
                    if doc.outputs.is_empty() {
                        exec::sinks(&doc).into_iter().next()
                    } else {
                        Some(String::new())
                    }
                })
                .unwrap_or_default();
        } else {
            let output = self
                .workflow_output
                .read(cx)
                .selected_value()
                .map(|s| {
                    if s.starts_with(PUBLISHED_OUTPUTS) {
                        String::new()
                    } else {
                        s.split(" · ").next().unwrap_or_default().to_string()
                    }
                })
                .unwrap_or_default();
            if config.output == output {
                return;
            }
            config.output = output;
        }
        self.graph.update(cx, |graph, cx| {
            graph.set_node_data(id, Some(NodeData::Workflow(config.clone())), cx)
        });
        self.reload(window, cx);
        self.sync(cx);
    }

    fn render_workflow(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        v_form()
            .small()
            .child(
                field()
                    .label("Saved workflow")
                    .child(Select::new(&self.workflow_graph).small()),
            )
            .child(
                field()
                    .label("Outputs")
                    .description("Published outputs create labeled ports. A selected node keeps a single output.")
                    .child(Select::new(&self.workflow_output).small()),
            )
            .child(
                field().child(
                    h_flex().gap_2().flex_wrap()
                    .child(Button::new("inspect-workflow").outline().small().label("Open child graph")
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| { if let Some(id) = this.node { this.open_workflow(id, cx); } })))
                    .child(
                    Button::new("refresh-workflows")
                        .ghost()
                        .small()
                        .label("Refresh")
                        .on_click(
                            cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.graph.update(cx, |graph, cx| graph.refresh_workflow_ports(cx));
                                this.reload(window, cx);
                            }),
                        ),
                    )
                ),
            )
            .children(self.workflow_fields.iter().map(|item| {
                let literal = item
                    .source
                    .read(cx)
                    .selected_value()
                    .is_some_and(|s| s == "Value");
                let default = item
                    .source
                    .read(cx)
                    .selected_value()
                    .is_some_and(|s| s == "Use default");
                let description = format!(
                    "{} · {}",
                    item.field.config.kind.label(),
                    if item.field.config.required {
                        "required"
                    } else {
                        "optional"
                    }
                );
                field()
                    .label(item.field.name.clone())
                    .description(description)
                    .child(
                        v_flex()
                            .gap_1()
                            .child(Select::new(&item.source).small())
                            .when(literal, |d| d.child(Textarea::new(&item.value).small()))
                            .when(default, |d| {
                                d.child(div().text_xs().child(
                                    if item.field.config.default.is_empty() {
                                        "No default value".to_string()
                                    } else {
                                        format!(
                                            "Default: {}",
                                            item.field
                                                .config
                                                .default
                                                .chars()
                                                .take(200)
                                                .collect::<String>()
                                        )
                                    },
                                ))
                            }),
                    )
            }))
    }

    fn render_workflow_details(&self, id: NodeId, cx: &mut Context<Self>) -> AnyElement {
        let prefix = format!("n{id}/");
        let mut lines = Vec::new();
        if self.workflow_details {
            if self.job.is_some() {
                for (node, state) in &self.child_states {
                    if node.starts_with(&prefix) {
                        lines.push(format!("{node}: {state:?}"));
                    }
                }
                lines.extend(
                    self.live_log
                        .iter()
                        .filter(|line| line.starts_with(&prefix))
                        .cloned(),
                );
            } else if let Some(call) = self
                .report
                .as_ref()
                .and_then(|(_, report)| report.calls.get(&format!("n{id}")))
            {
                fn describe(prefix: &str, call: &history::WorkflowRun, lines: &mut Vec<String>) {
                    lines.push(format!(
                        "{prefix}{} · {:?}",
                        call.graph.name, call.run.outcome
                    ));
                    for (id, node) in &call.run.nodes {
                        lines.push(format!(
                            "{prefix}{id}: {:?} · {}",
                            node.state,
                            node.error
                                .as_ref()
                                .or(node.label.as_ref())
                                .map(String::as_str)
                                .unwrap_or("no output")
                        ));
                        if !node.preview.is_empty() {
                            lines.push(node.preview.chars().take(500).collect());
                        }
                    }
                    for (id, nested) in &call.run.calls {
                        describe(&format!("{prefix}{id}/"), nested, lines);
                    }
                }
                describe(&prefix, call, &mut lines);
                lines.extend(call.run.log.iter().map(|line| format!("{prefix}{line}")));
            }
            if lines.is_empty() {
                lines.push("No child execution to show yet.".into());
            }
        }
        v_flex()
            .gap_1()
            .child(
                Button::new("workflow-details")
                    .outline()
                    .small()
                    .label(if self.workflow_details {
                        "Hide child execution"
                    } else {
                        "Show child execution"
                    })
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.workflow_details = !this.workflow_details;
                        cx.notify();
                    })),
            )
            .children(lines.into_iter().map(|line| div().text_xs().child(line)))
            .into_any_element()
    }

    // ----- field reactions -----

    /// The name lives on the node itself, not in its content, so it is written to the
    /// graph as typed; what is wrong with it shows under the form.
    fn on_name_change(&mut self, cx: &mut Context<Self>) {
        if self.loading || self.preview {
            return;
        }
        let Some(id) = self.node else { return };
        let name = text_of(&self.node_name, cx);
        self.graph
            .update(cx, |graph, cx| graph.set_node_name(id, Some(name), cx));
        cx.notify();
    }

    /// Problems that depend on the rest of the graph, found at render time so a wire
    /// drawn or removed shows at once: the node's name, and an agent's placeholders
    /// against what is wired into it.
    fn wiring_issues(&self, id: NodeId, data: Option<&NodeData>, cx: &App) -> Vec<Issue> {
        let graph = self.graph.read(cx);
        let mut issues = Vec::new();
        if let Some(NodeData::Workflow(config)) = data {
            if let Some(problem) = &self.workflow_problem {
                issues.push(Issue::new("workflow", problem.clone()));
            } else {
                let fields: Vec<_> = self
                    .workflow_fields
                    .iter()
                    .map(|f| f.field.clone())
                    .collect();
                let upstream = graph.input_handles(id);
                let upstream: Vec<_> = upstream.iter().map(String::as_str).collect();
                if let Err(error) =
                    config.validate_bindings(&fields, &upstream, &exec::Dirs::from_cwd().data)
                {
                    issues.push(Issue::new("inputs", error));
                }
            }
        }
        if matches!(data, Some(NodeData::Input(_))) {
            if graph.node_name(id).is_none() {
                issues.push(Issue::new(
                    "name",
                    "Name this Run input so it can appear in the Run form",
                ));
            }
            if !graph.inputs(id).is_empty() {
                issues.push(Issue::new(
                    "inputs",
                    "A Run input is a source; remove its incoming wires",
                ));
            }
        }
        if let Some(name) = graph.node_name(id) {
            if let Err(e) = validate::node_name(name) {
                issues.push(Issue::new("name", e));
            } else if let Some((other, _)) = graph
                .named_nodes()
                .iter()
                .find(|(other, n)| *other != id && n == name)
            {
                issues.push(Issue::new(
                    "name",
                    format!("n{other} is also named {name}; a placeholder needs one of each"),
                ));
            }
        }
        if let Some(NodeData::Agent(agent)) = data {
            let inputs = graph.input_handles(id);
            let mut handles: Vec<String> = Vec::new();
            for input in &inputs {
                handles.push(input.clone());
                if let Some(alias) = graph.input_alias(input) {
                    handles.push(alias);
                }
            }
            for (field, template) in [("prompt", &agent.prompt), ("message", &agent.message)] {
                for hole in validate::placeholders(template) {
                    if handles.contains(&hole) {
                        continue;
                    }
                    let where_ = if inputs.is_empty() {
                        "nothing is wired into this node".to_string()
                    } else {
                        format!("the inputs are {}", handles.join(", "))
                    };
                    issues.push(Issue::new(
                        field,
                        format!("{{{{{hole}}}}} names no input; {where_}"),
                    ));
                }
            }
        }
        issues
    }

    /// Switching provider fills in its usual model and key variable when those are empty
    /// or still the previous provider's defaults.
    fn on_provider_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        let Some(provider) = chosen(&self.provider, &Provider::ALL, Provider::label, cx) else {
            return;
        };
        self.endpoint.update(cx, |state, cx| {
            state.set_placeholder(endpoint_placeholder(provider), window, cx)
        });
        let model = text_of(&self.model, cx);
        if model.is_empty() || Provider::ALL.iter().any(|p| p.default_model() == model) {
            set_text(
                &self.model,
                provider.default_model().to_string(),
                window,
                cx,
            );
        }
        let env = text_of(&self.auth_env, cx);
        if env.is_empty() || Provider::ALL.iter().any(|p| p.default_env_var() == env) {
            set_text(
                &self.auth_env,
                provider.default_env_var().to_string(),
                window,
                cx,
            );
        }
        self.sync(cx);
    }

    /// An embedding model can only emit embeddings, and so on: snap the output to match.
    fn on_model_type_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        let Some(model_type) = chosen(&self.model_type, &ModelType::ALL, ModelType::label, cx)
        else {
            return;
        };
        let output = chosen(&self.output, &OutputType::ALL, OutputType::label, cx);
        if !output.is_some_and(|o| model_type.outputs().contains(&o))
            && let Some(first) = model_type.outputs().first()
        {
            choose(&self.output, &OutputType::ALL, *first, window, cx);
        }
        self.sync(cx);
    }

    fn on_script_path_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        if let Some(runtime) = validate::extension(&text_of(&self.script_path, cx))
            .as_deref()
            .and_then(Runtime::for_extension)
        {
            choose(&self.runtime, &Runtime::ALL, runtime, window, cx);
        }
        self.sync(cx);
    }

    fn on_data_path_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        if let Some(ext) = validate::extension(&text_of(&self.data_path, cx)) {
            let format = DataFormat::for_extension(&ext);
            choose(&self.format, &DataFormat::ALL, format, window, cx);
        }
        self.sync(cx);
    }

    fn set_read_only(&mut self, read_only: bool, cx: &mut Context<Self>) {
        self.read_only = read_only;
        self.sync(cx);
    }

    fn set_dir_gitignore(&mut self, on: bool, cx: &mut Context<Self>) {
        self.dir_gitignore = on;
        self.sync(cx);
    }

    fn set_dir_contents(&mut self, on: bool, cx: &mut Context<Self>) {
        self.dir_contents = on;
        self.sync(cx);
    }

    fn set_file_unfence(&mut self, on: bool, cx: &mut Context<Self>) {
        self.file_unfence = on;
        self.sync(cx);
    }

    fn set_file_bundle(&mut self, on: bool, cx: &mut Context<Self>) {
        self.file_bundle = on;
        self.sync(cx);
    }

    // ----- actions -----

    /// Give a blank node content, or replace a typed node's content.
    fn set_content(&mut self, data: NodeData, window: &mut Window, cx: &mut Context<Self>) {
        if self.preview {
            return;
        }
        let Some(id) = self.node else { return };
        self.graph.update(cx, |graph, cx| {
            graph.set_node_data(id, Some(data), cx);
            graph.set_node_label(id, None, cx);
        });
        self.reload(window, cx);
    }

    fn clear_content(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.preview {
            return;
        }
        let Some(id) = self.node else { return };
        self.graph.update(cx, |graph, cx| {
            graph.set_node_data(id, None, cx);
            graph.set_node_label(id, None, cx);
        });
        self.reload(window, cx);
    }

    /// Save the node's content under the preset name, so it appears in the sidebar.
    fn save_preset(&mut self, cx: &mut Context<Self>) {
        if self.preview {
            return;
        }
        let Some(id) = self.node else { return };
        let Some(data) = self.graph.read(cx).node_data(id).cloned() else {
            return;
        };
        let issues = data.validate();
        if !issues.is_empty() {
            let n = issues.len();
            self.status(
                format!(
                    "Fix {n} issue{} before saving",
                    if n == 1 { "" } else { "s" }
                ),
                true,
                cx,
            );
            return;
        }
        let raw = text_of(&self.preset_name, cx);
        if raw.is_empty() {
            self.status("Name the preset first".into(), true, cx);
            return;
        }
        let name = validate::file_stem(&raw);
        let result = match &data {
            NodeData::Agent(a) => library::save_agent(&name, a),
            NodeData::Database(d) => library::save_database(&name, d),
            NodeData::Input(_)
            | NodeData::Workflow(_)
            | NodeData::Script(_)
            | NodeData::Data(_)
            | NodeData::Directory(_)
            | NodeData::File(_) => {
                self.status(
                    format!(
                        "Scripts, data, directories and files are paths: put files in {}/ or {}/ and they appear in the sidebar",
                        library::SCRIPTS_DIR,
                        library::DATA_DIR
                    ),
                    true,
                    cx,
                );
                return;
            }
        };
        match result {
            Ok(path) => {
                self.graph.update(cx, |graph, cx| {
                    graph.set_node_label(id, Some(name.clone()), cx)
                });
                self.status(format!("Saved {}", path.display()), false, cx);
                cx.emit(InspectorEvent::LibraryChanged);
            }
            Err(text) => self.status(text, true, cx),
        }
    }

    fn save_prompt(&mut self, cx: &mut Context<Self>) {
        if self.preview || self.kind != Some(NodeType::Agent) {
            return;
        }
        let raw = text_of(&self.prompt_name, cx);
        if raw.is_empty() {
            self.status("Name the prompt first".into(), true, cx);
            return;
        }
        let text = self.prompt.read(cx).value().to_string();
        if text.trim().is_empty() {
            self.status("The prompt is empty".into(), true, cx);
            return;
        }
        match library::save_prompt(&raw, &text) {
            Ok(path) => {
                self.status(format!("Saved {}", path.display()), false, cx);
                cx.emit(InspectorEvent::LibraryChanged);
            }
            Err(text) => self.status(text, true, cx),
        }
    }

    /// Load a saved prompt into the selected agent node.
    pub fn apply_prompt(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.preview {
            return;
        }
        if self.node.is_none() || self.kind != Some(NodeType::Agent) {
            self.status(
                "Select an agent node to load a prompt into".into(),
                true,
                cx,
            );
            return;
        }
        match library::load_prompt(name) {
            Ok(text) => {
                set_textarea(&self.prompt, text, window, cx);
                set_text(&self.prompt_name, name.to_string(), window, cx);
                self.sync(cx);
                self.status(format!("Loaded prompt {name}"), false, cx);
            }
            Err(text) => self.status(text, true, cx),
        }
    }

    /// Point the selected node at a file, if it is a script or data node of the matching
    /// type. Returns whether that happened; the caller adds a new node otherwise.
    pub fn use_file(
        &mut self,
        ty: NodeType,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.preview {
            return false;
        }
        if self.node.is_none() || self.kind != Some(ty) {
            return false;
        }
        match ty {
            NodeType::Script => {
                set_text(&self.script_path, path.to_string(), window, cx);
                self.on_script_path_change(window, cx);
            }
            NodeType::Data => {
                set_text(&self.data_path, path.to_string(), window, cx);
                self.on_data_path_change(window, cx);
            }
            NodeType::Input
            | NodeType::Workflow
            | NodeType::Agent
            | NodeType::Database
            | NodeType::Directory
            | NodeType::File => {
                return false;
            }
        }
        true
    }

    /// Run the selected node and everything wired into it, off the UI thread. `fresh`
    /// ignores cached outputs. A run can start while a cancelled one is still winding
    /// down; the old one's progress is then ignored.
    fn run_selected(&mut self, fresh: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.node {
            self.start_run(vec![id], fresh, window, cx);
        }
    }

    /// Run every end node of the graph, which runs the whole graph in one pass.
    pub fn run_all(&mut self, fresh: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.preview {
            return;
        }
        let doc = self.graph.read(cx).snapshot(&self.graph_name);
        let targets: Vec<NodeId> = exec::sinks(&doc)
            .iter()
            .filter_map(|id| node_id(id))
            .collect();
        if targets.is_empty() {
            self.status("Nothing to run: no node has content".into(), true, cx);
            return;
        }
        self.start_run(targets, fresh, window, cx);
    }

    fn start_run(
        &mut self,
        targets: Vec<NodeId>,
        fresh: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.preview || self.running() {
            return;
        }
        let doc = self.graph.read(cx).snapshot(&self.graph_name);
        let ids: Vec<_> = targets.iter().map(|id| format!("n{id}")).collect();
        let fields = match workflow::fields(&doc, &ids) {
            Ok(fields) => fields,
            Err(error) => {
                self.status(error, true, cx);
                return;
            }
        };
        if fields.is_empty() {
            self.launch_run(doc, targets, fresh, workflow::Values::new(), cx);
            return;
        }
        let mut editors = Vec::new();
        let mut subscriptions = Vec::new();
        for field in fields {
            let initial = self
                .run_values
                .get(&field.name)
                .unwrap_or(&field.config.default)
                .clone();
            let rows = if matches!(field.config.kind, InputType::Text | InputType::Json) {
                3
            } else {
                1
            };
            let value = cx.new(|cx| TextareaState::new(window, cx).rows(rows));
            set_textarea(&value, initial.clone(), window, cx);
            let choices = match field.config.kind {
                InputType::File | InputType::Boolean => {
                    let mut options = vec![String::new()];
                    if field.config.kind == InputType::File {
                        options.extend(library::list_data());
                    } else {
                        options.extend(["true".into(), "false".into()]);
                    }
                    if !options.contains(&initial) {
                        options.push(initial.clone());
                    }
                    let selected = options
                        .iter()
                        .position(|s| s == &initial)
                        .map(IndexPath::new);
                    let items: Vec<SharedString> = options
                        .iter()
                        .map(|s| {
                            if s.is_empty() {
                                "—".into()
                            } else {
                                s.clone().into()
                            }
                        })
                        .collect();
                    let select = cx.new(|cx| SelectState::new(items, selected, window, cx));
                    let editor = value.clone();
                    subscriptions.push(cx.subscribe_in(
                        &select,
                        window,
                        move |this, state, _: &SelectEvent<Vec<SharedString>>, window, cx| {
                            if let Some(text) = state
                                .read(cx)
                                .selected_index(cx)
                                .and_then(|index| options.get(index.row).cloned())
                            {
                                set_textarea(&editor, text, window, cx);
                                if let Some(form) = &mut this.run_form {
                                    form.error = None;
                                }
                                cx.notify();
                            }
                        },
                    ));
                    Some(select)
                }
                _ => None,
            };
            editors.push(RunField {
                field,
                value,
                choices,
            });
        }
        self.run_form = Some(RunForm {
            doc,
            targets,
            fresh,
            fields: editors,
            error: None,
            _subscriptions: subscriptions,
        });
        self.scroll.set_offset(point(px(0.0), px(0.0)));
        self.status(
            "Fill in the run inputs, then press Start run".into(),
            false,
            cx,
        );
        cx.notify();
    }

    fn submit_run_form(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &self.run_form else {
            return;
        };
        let supplied = form
            .fields
            .iter()
            .map(|f| (f.field.name.clone(), f.value.read(cx).value().to_string()))
            .collect();
        let fields: Vec<_> = form.fields.iter().map(|f| f.field.clone()).collect();
        match workflow::resolve(&fields, &supplied, &exec::Dirs::from_cwd().data) {
            Ok(inputs) => {
                let form = self.run_form.take().expect("the run form is open");
                self.run_values = inputs.clone();
                self.launch_run(form.doc, form.targets, form.fresh, inputs, cx);
            }
            Err(error) => {
                if let Some(form) = &mut self.run_form {
                    form.error = Some(error);
                }
                cx.notify();
            }
        }
    }

    /// Start `targets` on a background thread, in order, each after everything wired into
    /// it, and poll its progress until the report and the run record arrive.
    fn launch_run(
        &mut self,
        doc: GraphDoc,
        targets: Vec<NodeId>,
        fresh: bool,
        inputs: workflow::Values,
        cx: &mut Context<Self>,
    ) {
        if self.preview {
            return;
        }
        self.tab = InspectorTab::Results;
        if self
            .job
            .as_ref()
            .is_some_and(|j| !j.cancel.load(Ordering::Relaxed))
            || targets.is_empty()
        {
            return;
        }
        let ids: Vec<String> = targets.iter().map(|t| format!("n{t}")).collect();
        let cancel = Arc::new(AtomicBool::new(false));
        let live = Arc::new(Mutex::new(Live::default()));
        let sink = live.clone();
        let review_sink = live.clone();
        let review_cancel = cancel.clone();
        let control = Control {
            inputs,
            review: Some(Arc::new(move |request| {
                let result = review::wait(request, &review_cancel, |pending| {
                    lock(&review_sink).review = Some(pending)
                });
                lock(&review_sink).review = None;
                result
            })),
            cache: Some(self.cache.clone()),
            fresh,
            cancel: cancel.clone(),
            progress: Some(Arc::new(move |progress| lock(&sink).take_in(progress))),
        };
        self.job = Some(Job {
            cancel,
            live: live.clone(),
            targets: targets.clone(),
            doc: doc.clone(),
        });
        self.live_log.clear();
        self.child_states.clear();
        self.live_stream = None;
        self.pending_review = None;
        self.live_states = 0;
        self.report = None;
        // `shown_run` stays: the results on show are still that run's until the report
        // arrives, and Previous compares against the run before it.
        // The dots now describe this run: every member hollow until it moves.
        self.marks = targets
            .iter()
            .flat_map(|target| Self::run_members(&doc, *target))
            .map(|member| (member, Mark::Pending))
            .collect();
        self.push_marks(cx);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let worker = live.clone();
            cx.background_spawn(async move {
                let http = UreqHttp::new(exec::HTTP_TIMEOUT);
                let dirs = exec::Dirs::from_cwd();
                let mut report = match ids.as_slice() {
                    [only] => exec::run_with(&doc, only, dirs, &http, &exec::process_env, &control),
                    many => {
                        exec::run_all_with(&doc, many, dirs, &http, &exec::process_env, &control)
                    }
                };
                let run = history::record(&doc, &ids, &report, control.cache.as_deref());
                if let Err(e) = history::save(Path::new("."), &run) {
                    report
                        .log
                        .push(format!("⚠ the run record was not saved: {e}"));
                }
                lock(&worker).report = Some((report, run));
            })
            .detach();
            loop {
                cx.background_executor().timer(PROGRESS_POLL).await;
                let finished = this
                    .update(cx, |this, cx| this.pull_progress(&live, cx))
                    .unwrap_or(true);
                if finished {
                    break;
                }
            }
        })
        .detach();
    }

    /// Copy what the running thread reported into the panel; true once the run is over or
    /// this is no longer the current job.
    fn pull_progress(&mut self, live: &Arc<Mutex<Live>>, cx: &mut Context<Self>) -> bool {
        if !self
            .job
            .as_ref()
            .is_some_and(|j| Arc::ptr_eq(&j.live, live))
        {
            return true;
        }
        let (report, moved) = {
            let mut live = lock(live);
            let changed_review = match (&self.pending_review, &live.review) {
                (Some(a), Some(b)) => !Arc::ptr_eq(&a.request, &b.request),
                (None, None) => false,
                _ => true,
            };
            if changed_review {
                self.review_file = 0;
                self.review_page = 0;
                if live.review.is_some() {
                    self.scroll.set_offset(point(px(0.0), px(0.0)));
                }
            }
            self.pending_review = live.review.clone();
            if live.log.len() > self.live_log.len() {
                self.live_log
                    .extend(live.log[self.live_log.len()..].iter().cloned());
            }
            if live.stream.is_some() {
                self.live_stream = live.stream.clone();
            }
            let mut moved = false;
            for (id, state) in &live.states[self.live_states..] {
                if id.contains('/') {
                    self.child_states.insert(id.clone(), *state);
                }
                if let Some(node) = node_id(id) {
                    let mark = match state {
                        NodeState::Running => Mark::Running,
                        NodeState::Done => Mark::Ran,
                        NodeState::Reused => Mark::Reused,
                        NodeState::Failed => Mark::Failed,
                    };
                    self.marks.insert(node, mark);
                    moved = true;
                }
            }
            self.live_states = live.states.len();
            (live.report.take(), moved)
        };
        if moved {
            self.push_marks(cx);
        }
        cx.notify();
        let Some((report, run)) = report else {
            cx.emit(InspectorEvent::ExecutionChanged);
            return false;
        };
        let Some(job) = self.job.take() else {
            return true;
        };
        self.live_log.clear();
        self.live_stream = None;
        self.pending_review = None;
        let data_of = |id: &str| {
            job.doc
                .nodes
                .iter()
                .find(|n| n.id == id)
                .and_then(|n| n.data.clone())
        };
        self.remember(&data_of, &report, cx);
        let all = job.targets.len() > 1;
        let what = if all { "Run all" } else { "Run" };
        let cached = match report.reused.len() {
            0 => String::new(),
            1 => " (1 node from the cache)".to_string(),
            n => format!(" ({n} nodes from the cache)"),
        };
        let (text, error) = match &report.result {
            Ok(_) if all => {
                let ran = report.outputs.len().saturating_sub(report.reused.len());
                (
                    format!(
                        "Run all finished in {:.1}s: {ran} node{} ran{cached}",
                        report.elapsed.as_secs_f32(),
                        if ran == 1 { "" } else { "s" }
                    ),
                    false,
                )
            }
            Ok(output) => {
                let saved = output
                    .saved
                    .as_ref()
                    .map(|p| format!(" → {}", p.display()))
                    .unwrap_or_default();
                (
                    format!(
                        "Run finished in {:.1}s: {}{saved}{cached}",
                        report.elapsed.as_secs_f32(),
                        output.label
                    ),
                    false,
                )
            }
            Err(e) if e == exec::CANCELLED => (format!("{what} cancelled"), false),
            Err(e) => (format!("{what} failed: {e}"), true),
        };
        if let Ok(output) = &report.result
            && output.saved.is_some()
        {
            self.output_count += 1;
        }
        self.report = Some((job.targets, report));
        self.shown_run = Some(run.id.clone());
        self.history.insert(0, run);
        self.history.truncate(history::MAX_RUNS);
        self.status(text, error, cx);
        cx.emit(InspectorEvent::ExecutionChanged);
        cx.notify();
        true
    }

    /// Keep every node's outcome from a run: the outputs that were made, and the error on
    /// the node that failed. Nodes the run never reached keep their earlier result. The
    /// canvas dots are settled from the report: a node that never got going loses its
    /// hollow dot. `data_of` is each node's content when it ran.
    fn remember(
        &mut self,
        data_of: &dyn Fn(&str) -> Option<NodeData>,
        report: &Report,
        cx: &mut Context<Self>,
    ) {
        for (id, output) in &report.outputs {
            if let Some(node) = node_id(id) {
                self.results.insert(
                    node,
                    NodeResult {
                        named_outputs: report.named_outputs.get(id).cloned().unwrap_or_default(),
                        result: Ok(output.clone()),
                        data: data_of(id),
                    },
                );
                let mark = if report.reused.contains(id) {
                    Mark::Reused
                } else {
                    Mark::Ran
                };
                self.marks.insert(node, mark);
            }
        }
        if let (Some(id), Err(e)) = (&report.failed, &report.result)
            && let Some(node) = node_id(id)
        {
            self.results.insert(
                node,
                NodeResult {
                    named_outputs: BTreeMap::new(),
                    result: Err(e.clone()),
                    data: data_of(id),
                },
            );
            if e != exec::CANCELLED {
                self.marks.insert(node, Mark::Failed);
            }
        }
        self.marks
            .retain(|_, mark| !matches!(mark, Mark::Pending | Mark::Running));
        self.push_marks(cx);
    }

    /// Ask the running job to stop. Scripts and database tools are killed at once; a
    /// streaming agent stops at its next piece of text.
    fn cancel_run(&mut self, cx: &mut Context<Self>) {
        if let Some(job) = &self.job {
            job.cancel.store(true, Ordering::Relaxed);
            self.status("Cancelling…".into(), false, cx);
            cx.notify();
        }
    }

    /// Show a past run as if it had just finished: its outputs (from the cache, or the
    /// previews its record kept), its dots and its log, without running anything.
    fn open_run(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.running() {
            return;
        }
        let Some(run) = self.history.iter().find(|r| r.id == id).cloned() else {
            return;
        };
        let report = run.report(Some(&self.cache));
        self.run_values = run.inputs.clone();
        let targets: Vec<NodeId> = run.targets.iter().filter_map(|t| node_id(t)).collect();
        self.results.clear();
        self.marks.clear();
        let data_of = |id: &str| run.nodes.get(id).and_then(|n| n.data.clone());
        self.remember(&data_of, &report, cx);
        self.report = Some((targets, report));
        self.shown_run = Some(run.id.clone());
        self.status(
            format!("Showing the run from {} UTC; nothing was run", run.time()),
            false,
            cx,
        );
        cx.notify();
    }

    fn clear_runs(&mut self, cx: &mut Context<Self>) {
        let n = history::clear(Path::new("."), &self.graph_name);
        self.history.clear();
        self.shown_run = None;
        self.status(
            format!(
                "Deleted {n} run record{} of {}",
                if n == 1 { "" } else { "s" },
                self.graph_name
            ),
            false,
            cx,
        );
        cx.notify();
    }

    /// The latest past run, other than the one showing, that this node took part in.
    fn previous_run(&self, node: NodeId) -> Option<(&history::Run, &history::NodeRun)> {
        let key = format!("n{node}");
        self.history
            .iter()
            .filter(|run| self.shown_run.as_deref() != Some(run.id.as_str()))
            .find_map(|run| run.nodes.get(&key).map(|n| (run, n)))
    }

    fn clear_cache(&mut self, cx: &mut Context<Self>) {
        let n = {
            let mut cache = lock(&self.cache);
            let n = cache.len();
            cache.clear();
            n
        };
        self.status(
            format!(
                "Dropped {n} cached output{}; the next run recomputes everything",
                if n == 1 { "" } else { "s" }
            ),
            false,
            cx,
        );
        cx.notify();
    }

    /// Re-list the saved generative agents for the SQL helper, keeping the pick when that
    /// agent still exists.
    fn refresh_sql_agents(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let chosen = self.sql_agent.read(cx).selected_value().cloned();
        self.sql_agents = library::list_agents()
            .into_iter()
            .filter(|p| p.config.model_type == ModelType::Generative)
            .collect();
        let items: Vec<SharedString> = self
            .sql_agents
            .iter()
            .map(|p| SharedString::from(p.name.clone()))
            .collect();
        let index = chosen
            .and_then(|name| items.iter().position(|i| *i == name))
            .or_else(|| (!items.is_empty()).then_some(0));
        self.sql_agent.update(cx, |select, cx| {
            select.set_items(items, window, cx);
            select.set_selected_index(index.map(IndexPath::new), window, cx);
        });
    }

    /// Ask the chosen agent to write the query from the schema and the request, then put
    /// its answer into the query field, where the usual checks run on it.
    fn write_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.writing_sql {
            return;
        }
        let Some(id) = self.node else { return };
        let Some(NodeData::Database(config)) = self.graph.read(cx).node_data(id).cloned() else {
            return;
        };
        self.refresh_sql_agents(window, cx);
        let agent = self
            .sql_agent
            .read(cx)
            .selected_index(cx)
            .and_then(|i| self.sql_agents.get(i.row).cloned());
        let Some(agent) = agent else {
            self.status("Save an agent first, then pick it here".into(), true, cx);
            return;
        };
        let request = self.sql_request.read(cx).value().to_string();
        if request.trim().is_empty() {
            self.status("Describe what the query should do first".into(), true, cx);
            return;
        }
        self.writing_sql = true;
        self.status(
            format!("Asking {} to write the query…", agent.name),
            false,
            cx,
        );
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let http = UreqHttp::new(exec::HTTP_TIMEOUT);
                    exec::write_sql(
                        &agent.config,
                        &config,
                        &request,
                        &exec::Dirs::from_cwd(),
                        &http,
                        &exec::process_env,
                    )
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.writing_sql = false;
                match result {
                    Ok(sql) => {
                        set_textarea(&this.query, sql, window, cx);
                        this.sync(cx);
                        match this.issues.iter().find(|i| i.field == "query") {
                            Some(issue) => this.status(
                                format!("The agent's query was refused: {}", issue.message),
                                true,
                                cx,
                            ),
                            None => this.status(
                                "Query written; read it before you run it".into(),
                                false,
                                cx,
                            ),
                        }
                    }
                    Err(e) => this.status(format!("Writing the query failed: {e}"), true, cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ----- rendering -----

    fn render_agent(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let muted = cx.theme().muted_foreground;
        let provider =
            chosen(&self.provider, &Provider::ALL, Provider::label, cx).unwrap_or(Provider::Ollama);
        let is_local = provider.is_local();
        v_form()
            .small()
            .child(
                field()
                    .label("Provider")
                    .child(Select::new(&self.provider).small().w_full()),
            )
            .child(
                field()
                    .label("Endpoint")
                    .description(
                        "Base URL of the API. Empty uses the provider's default; plain http only for localhost.",
                    )
                    .child(Input::new(&self.endpoint).small()),
            )
            .child(
                field()
                    .label("Model type")
                    .child(Select::new(&self.model_type).small().w_full()),
            )
            .child(
                field()
                    .label("Model")
                    .child(Input::new(&self.model).small()),
            )
            .child(
                field()
                    .label("API key environment variable")
                    .description(
                        "Only the variable's name is saved. The key itself stays in your shell.",
                    )
                    .visible(!is_local)
                    .child(Input::new(&self.auth_env).small()),
            )
            .child(
                field().label("Prompt").child(
                    v_flex()
                        .gap_1()
                        .child(Textarea::new(&self.prompt).small())
                        .child(
                            h_flex()
                                .gap_2()
                                .child(Input::new(&self.prompt_name).small())
                                .child(
                                    Button::new("save-prompt")
                                        .outline()
                                        .small()
                                        .label("Save prompt")
                                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                            this.save_prompt(cx)
                                        })),
                                ),
                        )
                        .child(div().text_xs().text_color(muted).child(format!(
                            "Saved prompts live in {}/ and load from the sidebar. {{{{name}}}} stands for the output of the input named so.",
                            library::PROMPTS_DIR
                        ))),
                ),
            )
            .child(
                field()
                    .label("Message")
                    .description(
                        "The user message. Empty sends the inputs joined with blank lines; otherwise {{name}} puts an input here and the inputs nothing names follow.",
                    )
                    .child(Textarea::new(&self.message).small()),
            )
            .child(
                field()
                    .label("Context size (tokens)")
                    .child(Input::new(&self.context_size).small()),
            )
            .child(
                field()
                    .label("Temperature")
                    .when(!providers::sends_temperature(provider), |f| {
                        f.description(
                            "Not sent to Anthropic: API version 2026-10-21 has no sampling parameters.",
                        )
                    })
                    .child(Input::new(&self.temperature).small()),
            )
            .child(
                field()
                    .label("Output")
                    .child(Select::new(&self.output).small().w_full()),
            )
    }

    fn render_database(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let file_based = chosen(&self.engine, &DbEngine::ALL, DbEngine::label, cx)
            .is_none_or(DbEngine::is_file_based);
        let read_only = self.read_only;
        let muted = cx.theme().muted_foreground;
        let danger = cx.theme().danger;
        let has_agents = !self.sql_agents.is_empty();
        v_form()
            .small()
            .child(
                field()
                    .label("Engine")
                    .child(Select::new(&self.engine).small().w_full()),
            )
            .child(
                field()
                    .label("Database file")
                    .description(format!("Relative to {}/.", library::DATA_DIR))
                    .visible(file_based)
                    .child(Input::new(&self.db_path).small()),
            )
            .child(
                field()
                    .label("Connection URL environment variable")
                    .description("The URL, password included, stays in your shell.")
                    .visible(!file_based)
                    .child(Input::new(&self.url_env).small()),
            )
            .child(
                field()
                    .label("Input data")
                    .description(format!(
                        "Optional file in {}/ loaded into the table below before the query.",
                        library::DATA_DIR
                    ))
                    .child(Input::new(&self.input).small()),
            )
            .child(
                field()
                    .label("Input table")
                    .child(Input::new(&self.input_table).small()),
            )
            .child(
                field()
                    .label("Query")
                    .description(
                        "One statement. Leave it empty to run the SQL arriving on the wire, for example from an agent.",
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(Textarea::new(&self.query).small())
                            .child(
                                Switch::new("read-only")
                                    .small()
                                    .checked(read_only)
                                    .label("Read-only: SELECT-style statements only")
                                    .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                        this.set_read_only(*checked, cx)
                                    })),
                            )
                            .when(!read_only, |d| {
                                d.child(div().text_xs().text_color(danger).child(
                                    "Writes allowed: the node is red on the canvas and runs whatever SQL it is given.",
                                ))
                            }),
                    ),
            )
            .child(
                field()
                    .label("Write the query with an agent")
                    .description(
                        "A saved agent gets the schema and your request; its answer lands in the query above for you to check.",
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .when(has_agents, |d| {
                                d.child(Select::new(&self.sql_agent).small().w_full())
                            })
                            .when(!has_agents, |d| {
                                d.child(div().text_xs().text_color(muted).child(
                                    "No saved agents yet: press Save agent on an agent node and it appears here.",
                                ))
                            })
                            .child(Textarea::new(&self.sql_request).small())
                            .child(
                                Button::new("write-sql")
                                    .outline()
                                    .small()
                                    .label(if self.writing_sql {
                                        "Writing…"
                                    } else {
                                        "Write query"
                                    })
                                    .disabled(self.writing_sql || !has_agents)
                                    .loading(self.writing_sql)
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.write_query(window, cx)
                                    })),
                            ),
                    ),
            )
    }

    fn render_script(&self, _: &mut Context<Self>) -> impl IntoElement + use<> {
        v_form()
            .small()
            .child(
                field()
                    .label("Runtime")
                    .child(Select::new(&self.runtime).small().w_full()),
            )
            .child(
                field()
                    .label("Script file")
                    .description(format!(
                        "Relative to {}/. Pick one from the sidebar or type a name.",
                        library::SCRIPTS_DIR
                    ))
                    .child(Input::new(&self.script_path).small()),
            )
            .child(
                field()
                    .label("Arguments")
                    .description(
                        "Split like a shell: spaces separate, double or single quotes keep words together, a backslash escapes.",
                    )
                    .child(Input::new(&self.args).small()),
            )
            .child(
                field()
                    .label("Timeout (seconds)")
                    .child(Input::new(&self.timeout).small()),
            )
    }

    fn render_data(&self, _: &mut Context<Self>) -> impl IntoElement + use<> {
        v_form()
            .small()
            .child(
                field()
                    .label("File")
                    .description(format!(
                        "Relative to {}/. Pick one from the sidebar or type a name.",
                        library::DATA_DIR
                    ))
                    .child(Input::new(&self.data_path).small()),
            )
            .child(
                field()
                    .label("Format")
                    .child(Select::new(&self.format).small().w_full()),
            )
    }

    fn render_directory(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let gitignore = self.dir_gitignore;
        let contents = self.dir_contents;
        v_form()
            .small()
            .child(
                field()
                    .label("Directory")
                    .description(
                        "Relative to the working directory the app runs in; `.` is that directory itself, so a graph can read the repository it lives in. Hidden entries and symlinks are never read.",
                    )
                    .child(Input::new(&self.dir_path).small()),
            )
            .child(
                field()
                    .label("Include patterns")
                    .description(
                        "Space-separated globs relative to the directory. `*` crosses `/`, so `*.rs` takes Rust files at any depth and `src/**/*.rs` only those under src/.",
                    )
                    .child(Input::new(&self.dir_include).small()),
            )
            .child(
                field()
                    .label("Exclude patterns")
                    .child(Input::new(&self.dir_exclude).small()),
            )
            .child(
                field().label("Options").child(
                    v_flex()
                        .gap_1()
                        .child(
                            Switch::new("dir-gitignore")
                                .small()
                                .checked(gitignore)
                                .label("Respect .gitignore files (skips build output)")
                                .on_change(cx.listener(|this, on: &bool, _, cx| {
                                    this.set_dir_gitignore(*on, cx)
                                })),
                        )
                        .child(
                            Switch::new("dir-contents")
                                .small()
                                .checked(contents)
                                .label("Include file contents after the listing")
                                .on_change(cx.listener(|this, on: &bool, _, cx| {
                                    this.set_dir_contents(*on, cx)
                                })),
                        ),
                ),
            )
            .child(
                field()
                    .label("Max files")
                    .child(Input::new(&self.dir_max_files).small()),
            )
            .child(
                field()
                    .label("Content budget (bytes)")
                    .description(
                        "Contents stop when the budget is spent. The agent downstream must still fit all of it in its context size.",
                    )
                    .child(Input::new(&self.dir_max_bytes).small()),
            )
    }

    fn render_file(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let unfence = self.file_unfence;
        let bundle = self.file_bundle;
        let danger = cx.theme().danger;
        let muted = cx.theme().muted_foreground;
        let reserved = library::RESERVED_DIRS
            .iter()
            .map(|d| format!("{d}/"))
            .collect::<Vec<_>>()
            .join(", ");
        v_form()
            .small()
            .child(
                field()
                    .label(if bundle { "Directory" } else { "File" })
                    .description(if bundle {
                        format!(
                            "Relative to the working directory; `.` is that directory itself. Each section's path goes under it. {reserved} are off limits."
                        )
                    } else {
                        format!(
                            "Relative to the working directory; missing directories are created. {reserved} are off limits."
                        )
                    })
                    .child(Input::new(&self.file_path).small()),
            )
            .child(
                field()
                    .label(if bundle {
                        "If a file exists"
                    } else {
                        "If the file exists"
                    })
                    .child(Select::new(&self.file_mode).small().w_full()),
            )
            .child(
                field().label("Content").child(
                    v_flex()
                        .gap_1()
                        .child(
                            Switch::new("file-bundle")
                                .small()
                                .checked(bundle)
                                .label("Split `=== path ===` sections into files under the directory")
                                .on_change(cx.listener(|this, on: &bool, _, cx| {
                                    this.set_file_bundle(*on, cx)
                                })),
                        )
                        .when(bundle, |d| {
                            d.child(div().text_xs().text_color(muted).child(
                                "The format a directory node emits. Ask the agent: \"Answer with one `=== path ===` section per file, nothing else.\" Nothing is written until every path checks out.",
                            ))
                        })
                        .child(
                            Switch::new("file-review")
                                .small()
                                .checked(self.file_review)
                                .label("Review changes before writing")
                                .on_change(cx.listener(|this, on: &bool, _, cx| {
                                    this.file_review = *on;
                                    this.sync(cx);
                                })),
                        )
                        .child(
                            Switch::new("file-unfence")
                                .small()
                                .checked(unfence)
                                .label("Strip a surrounding code fence and end with a newline")
                                .on_change(cx.listener(|this, on: &bool, _, cx| {
                                    this.set_file_unfence(*on, cx)
                                })),
                        )
                        .child(div().text_xs().text_color(danger).child(
                            "Review pauses this node with a diff for every file. Apply the batch to continue, or reject it to leave the files untouched.",
                        )),
                ),
            )
    }

    fn render_input_config(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        v_form().small()
            .child(field().label("Input type").child(Select::new(&self.input_kind).small().w_full()))
            .child(field().label("Default value").description("Used when this input has no value from an earlier run. File paths are relative to data/.")
                .child(Textarea::new(&self.input_default).small()))
            .child(field().label("Run form").description("Give this node a Name above, then wire it to the nodes that use its value. File inputs send the selected file's contents.")
                .child(Switch::new("input-required").small().checked(self.input_required).label("A value is required")
                    .on_change(cx.listener(|this, on: &bool, _, cx| { this.input_required = *on; this.sync(cx); }))))
    }

    fn render_run_form(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let form = self.run_form.as_ref()?;
        let muted = cx.theme().muted_foreground;
        let danger = cx.theme().danger;
        Some(v_flex().gap_2().p_2().border_1().border_color(cx.theme().border).rounded_md()
            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Run inputs"))
            .child(div().text_xs().text_color(muted).child("Values are saved with this run. The saved graph and its defaults stay unchanged."))
            .children(form.fields.iter().map(|editor| {
                field().label(editor.field.name.clone())
                    .description(format!("{} · {} · {}", editor.field.config.kind.label(), if editor.field.config.required { "required" } else { "optional" }, editor.field.node))
                    .child(v_flex().gap_1()
                        .children(editor.choices.as_ref().map(|s| Select::new(s).small().w_full()))
                        .when(editor.field.config.kind != InputType::Boolean, |d| d.child(Textarea::new(&editor.value).small())))
            }))
            .children(form.error.as_ref().map(|e| div().text_xs().text_color(danger).child(e.clone())))
            .child(h_flex().gap_2()
                .child(Button::new("submit-run-inputs").primary().small().label(if form.fresh { "Start fresh run" } else { "Start run" })
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.submit_run_form(cx))))
                .child(Button::new("dismiss-run-inputs").outline().small().label("Cancel")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| { this.run_form = None; cx.notify(); }))))
            .into_any_element())
    }

    fn respond_review(
        &mut self,
        request: &Arc<review::Request>,
        decision: review::Decision,
        cx: &mut Context<Self>,
    ) {
        let Some(job) = &self.job else {
            return;
        };
        if job.cancel.load(Ordering::Relaxed) {
            return;
        }
        let mut live = lock(&job.live);
        if live
            .review
            .as_ref()
            .is_some_and(|p| Arc::ptr_eq(&p.request, request))
            && let Some(pending) = live.review.take()
        {
            pending.decide(decision);
        }
        self.pending_review = None;
        cx.notify();
    }

    fn render_review(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        const PAGE_LINES: usize = 100;
        let pending = self.pending_review.as_ref()?;
        let request = &pending.request;
        let file = request.files.get(self.review_file)?;
        let pages = file.diff.lines().count().div_ceil(PAGE_LINES).max(1);
        let apply = request.clone();
        let reject = request.clone();
        let muted = cx.theme().muted_foreground;
        let success = cx.theme().success;
        let danger = cx.theme().danger;
        let cancelling = self
            .job
            .as_ref()
            .is_none_or(|j| j.cancel.load(Ordering::Relaxed));
        Some(v_flex().gap_2().p_2().border_1().border_color(cx.theme().border).rounded_md()
            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(format!("Review changes · {}", request.node)))
            .child(div().text_xs().text_color(muted).child("This node is paused. Apply approves the complete batch; changed destinations require a new review."))
            .child(h_flex().gap_1()
                .child(Button::new("review-prev-file").xsmall().label("Previous file").disabled(self.review_file == 0)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| { this.review_file = this.review_file.saturating_sub(1); this.review_page = 0; cx.notify(); })))
                .child(Button::new("review-next-file").xsmall().label("Next file").disabled(self.review_file + 1 >= request.files.len())
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| { if let Some(p) = &this.pending_review { this.review_file = (this.review_file + 1).min(p.request.files.len().saturating_sub(1)); } this.review_page = 0; cx.notify(); }))))
            .child(div().text_sm().child(format!("{}/{} · {}", self.review_file + 1, request.files.len(), file.path)))
            .child(div().text_xs().text_color(muted).child(format!("{} → {} bytes", file.before_bytes.map(|n| n.to_string()).unwrap_or_else(|| "new file".into()), file.after_bytes)))
            .child(div().id("review-diff").max_h(px(360.0)).overflow_y_scroll().overflow_x_scroll().p_2().bg(cx.theme().background)
                .child(v_flex().font_family("Menlo").text_xs().children(file.diff.lines().skip(self.review_page * PAGE_LINES).take(PAGE_LINES).map(|line| {
                    div().whitespace_nowrap().text_color(if line.starts_with('+') { success } else if line.starts_with('-') { danger } else { muted }).child(line.to_string())
                }))))
            .child(h_flex().gap_1()
                .child(Button::new("review-prev-page").xsmall().label("Previous page").disabled(self.review_page == 0)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| { this.review_page = this.review_page.saturating_sub(1); cx.notify(); })))
                .child(div().text_xs().child(format!("{} / {pages}", self.review_page + 1)))
                .child(Button::new("review-next-page").xsmall().label("Next page").disabled(self.review_page + 1 >= pages)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| { this.review_page = (this.review_page + 1).min(pages - 1); cx.notify(); }))))
            .child(h_flex().gap_1()
                .child(Button::new("review-apply").primary().small().label(format!("Apply {} file(s)", request.files.len())).disabled(cancelling)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.respond_review(&apply, review::Decision::Apply, cx))))
                .child(Button::new("review-reject").outline().small().label("Reject batch").disabled(cancelling)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.respond_review(&reject, review::Decision::Reject, cx))))
                .child(Button::new("review-cancel").ghost().small().label("Cancel run").disabled(cancelling)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.cancel_run(cx)))))
            .into_any_element())
    }

    /// One button per content type, with the writable database after the read-only one.
    /// An agent's inputs: how each is addressed, where it ends up, and what it produced
    /// the last time it ran.
    fn render_inputs(
        &self,
        id: NodeId,
        agent: &AgentConfig,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let fg = theme.sidebar_foreground;
        let muted = theme.muted_foreground;
        let danger = theme.danger;
        let graph = self.graph.read(cx);
        let in_prompt = validate::placeholders(&agent.prompt);
        let in_message = validate::placeholders(&agent.message);
        let inputs = graph.input_handles(id);
        let rows: Vec<AnyElement> = inputs
            .iter()
            .map(|input| {
                let key = input.clone();
                let name = graph.input_alias(input);
                let refers = |holes: &[String]| {
                    holes
                        .iter()
                        .any(|h| *h == key || name.as_deref() == Some(h.as_str()))
                };
                let placement = match (refers(&in_prompt), refers(&in_message)) {
                    (true, true) => "in the prompt and the message",
                    (true, false) => "in the prompt",
                    (false, true) => "in the message",
                    (false, false) => "appended to the message",
                };
                let (handle, detail) = match &name {
                    Some(n) => (format!("{{{{{n}}}}}"), format!("{key} · {placement}")),
                    None => (format!("{{{{{key}}}}}"), placement.to_string()),
                };
                let (base, port) = input
                    .split_once('.')
                    .map_or((input.as_str(), None), |(base, port)| (base, Some(port)));
                let result = node_id(base).and_then(|id| self.results.get(&id));
                let named = port.and_then(|port| result?.named_outputs.get(port));
                let (last, color) = if let Some(output) = named {
                    (output.label.clone(), muted)
                } else {
                    match result.map(|r| &r.result) {
                        Some(Ok(output)) => (output.label.clone(), muted),
                        Some(Err(_)) => ("failed last time".to_string(), danger),
                        None => ("not run yet".to_string(), muted),
                    }
                };
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(fg)
                            .child(handle),
                    )
                    .child(div().text_xs().text_color(muted).child(detail))
                    .child(div().ml_auto().text_xs().text_color(color).child(last))
                    .into_any_element()
            })
            .collect();
        let hint = |text: &'static str| div().text_xs().text_color(muted).child(text);
        v_flex()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(fg)
                    .child(format!("Inputs · {}", inputs.len())),
            )
            .children(rows)
            .when(inputs.is_empty(), |d| {
                d.child(hint(
                    "Nothing is wired in. Drag from another node's ● onto this one.",
                ))
            })
            .child(hint(
                "Name an upstream node and write {{name}} in the prompt or message to put its output there; the rest follow in id order.",
            ))
    }

    /// The graph's past runs, newest first, with Run all and Clear.
    fn render_runs(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let fg = theme.sidebar_foreground;
        let muted = theme.muted_foreground;
        let accent = theme.sidebar_accent;
        let accent_fg = theme.sidebar_accent_foreground;
        let running = self.running();
        let shown = self.shown_run.clone();
        let rows: Vec<AnyElement> = self
            .history
            .iter()
            .take(RUNS_SHOWN)
            .enumerate()
            .map(|(i, run)| {
                let id = run.id.clone();
                let active = shown.as_deref() == Some(run.id.as_str());
                div()
                    .id(("run", i))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .text_xs()
                    .text_color(fg)
                    .cursor_pointer()
                    .when(active, |d| d.bg(accent).text_color(accent_fg))
                    .hover(move |d| d.bg(accent))
                    .child(run.headline())
                    .on_click(
                        cx.listener(move |this, _: &ClickEvent, _, cx| this.open_run(&id, cx)),
                    )
                    .into_any_element()
            })
            .collect();
        let more = self.history.len().saturating_sub(RUNS_SHOWN);
        let hint = |text: String| div().text_xs().text_color(muted).child(text);
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(fg)
                            .child("Runs"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(self.history.len().to_string()),
                    )
                    .child(
                        div().ml_auto().child(
                            Button::new("run-all")
                                .primary()
                                .xsmall()
                                .label(if running { "Running…" } else { "Run all" })
                                .disabled(running)
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.run_all(false, window, cx)
                                })),
                        ),
                    )
                    .when(!self.history.is_empty() && !running, |d| {
                        d.child(
                            Button::new("clear-runs")
                                .ghost()
                                .xsmall()
                                .label("Clear")
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.clear_runs(cx)
                                })),
                        )
                    }),
            )
            .child(hint(format!(
                "Run all (⌘↩) runs every end node. Click a run to see its log and outputs without running anything; times are UTC; records live in {}/{}/.",
                history::DIR, self.graph_name
            )))
            .children(rows)
            .children(self.report.as_ref().filter(|(_, report)| !report.inputs.is_empty()).map(|(_, report)| {
                v_flex().gap_1().child(hint("Run inputs · these values will prefill the next Run form".into()))
                    .children(report.inputs.iter().map(|(name, value)| {
                        let shown: String = value.chars().take(240).collect();
                        hint(format!("{name}: {shown}{}", if shown.len() < value.len() { "…" } else { "" }))
                    }))
            }))
            .when(more > 0, |d| d.child(hint(format!("… {more} more on disk"))))
            .when(self.history.is_empty(), |d| {
                d.child(hint("No runs of this graph yet.".into()))
            })
    }

    fn render_type_buttons(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let choices: Vec<(&'static str, NodeData)> = NodeType::ALL
            .into_iter()
            .flat_map(|ty| {
                let mut choices = vec![(ty.label(), ty.default_data())];
                if ty == NodeType::Database {
                    choices.push((
                        WRITABLE_DATABASE_LABEL,
                        NodeData::Database(DatabaseConfig::writable()),
                    ));
                }
                choices
            })
            .collect();
        h_flex()
            .flex_wrap()
            .gap_2()
            .children(choices.into_iter().enumerate().map(|(i, (label, data))| {
                Button::new(("make", i))
                    .outline()
                    .small()
                    .label(label)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.set_content(data.clone(), window, cx)
                    }))
            }))
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let border = cx.theme().sidebar_border;
        h_flex()
            .gap_1()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(border)
            .children(
                [
                    (InspectorTab::Configure, "Configure"),
                    (InspectorTab::Results, "Results"),
                    (InspectorTab::History, "History"),
                ]
                .into_iter()
                .enumerate()
                .map(|(i, (tab, label))| {
                    Button::new(("inspector-tab", i))
                        .ghost()
                        .small()
                        .label(label)
                        .when(self.tab == tab, |button| button.outline())
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.tab = tab;
                            this.scroll.set_offset(point(px(0.0), px(0.0)));
                            cx.notify();
                        }))
                }),
            )
            .into_any_element()
    }

    fn render_named_outputs(&self, id: NodeId, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let outputs = self.results.get(&id).map(|result| &result.named_outputs);
        v_flex()
            .gap_3()
            .children(outputs.into_iter().flat_map(|outputs| outputs.iter()).map(
                |(name, output)| {
                    let preview = output
                        .as_text()
                        .map(|text| text.chars().take(PREVIEW_CHARS).collect::<String>());
                    v_flex()
                        .gap_2()
                        .p_3()
                        .border_1()
                        .border_color(border)
                        .rounded_lg()
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(name.clone()),
                                )
                                .child(
                                    div()
                                        .ml_auto()
                                        .text_xs()
                                        .text_color(muted)
                                        .child(output.label.clone()),
                                ),
                        )
                        .children(preview.map(|text| div().text_xs().child(text)))
                        .when(!output.text, |d| {
                            d.child(div().text_xs().text_color(muted).child("Binary output"))
                        })
                        .when(
                            output
                                .as_text()
                                .is_some_and(|s| s.chars().count() > PREVIEW_CHARS),
                            |d| {
                                d.child(
                                    div().text_xs().text_color(muted).child("Preview truncated"),
                                )
                            },
                        )
                        .children(output.saved.as_ref().map(|path| {
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(path.display().to_string())
                        }))
                },
            ))
            .into_any_element()
    }

    fn render_activity(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let danger = cx.theme().danger;
        let success = cx.theme().success;
        let running = self.job.is_some() || self.preview_live;
        let (headline, color) = if running {
            ("Running…".to_string(), muted)
        } else if let Some((_, report)) = &self.report {
            match &report.result {
                Ok(_) => (
                    format!("Completed in {:.1}s", report.elapsed.as_secs_f32()),
                    success,
                ),
                Err(error) if error == exec::CANCELLED => ("Run cancelled".to_string(), muted),
                Err(error) => (error.clone(), danger),
            }
        } else if self.preview_snapshot {
            (
                "This child did not execute in the displayed run.".to_string(),
                muted,
            )
        } else {
            (
                "No run to show yet. Run the workflow to inspect its results.".to_string(),
                muted,
            )
        };
        let log = if running {
            self.live_log.clone()
        } else {
            self.report
                .as_ref()
                .map(|(_, report)| report.log.clone())
                .unwrap_or_default()
        };
        let stream = self
            .live_stream
            .as_ref()
            .filter(|_| running)
            .map(|(id, text)| {
                let preview: String = text
                    .chars()
                    .rev()
                    .take(PREVIEW_CHARS)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                v_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(format!("{id} · streaming")),
                    )
                    .child(div().text_xs().child(preview))
            });
        v_flex()
            .gap_3()
            .child(div().text_sm().text_color(color).child(headline))
            .children(stream)
            .when(!log.is_empty(), |d| {
                d.child(
                    Button::new("activity-log")
                        .ghost()
                        .small()
                        .label(if self.show_log {
                            "Hide execution log"
                        } else {
                            "Show execution log"
                        })
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.show_log = !this.show_log;
                            cx.notify();
                        })),
                )
            })
            .when(self.show_log, |d| {
                d.children(
                    log.into_iter()
                        .map(|line| div().text_xs().text_color(muted).child(line)),
                )
            })
            .into_any_element()
    }

    fn render_preview(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let bg = theme.sidebar;
        let border = theme.sidebar_border;
        let muted = theme.muted_foreground;
        let danger = theme.danger;
        let selected = self.graph.read(cx).selected();
        let node = selected.as_ref().map(|node| node.id);
        let title = selected
            .as_ref()
            .map(|node| {
                node.name.map(str::to_string).unwrap_or_else(|| {
                    node.data
                        .map(NodeData::label)
                        .unwrap_or("Blank node")
                        .into()
                })
            })
            .unwrap_or_else(|| "Child workflow".into());
        let data = selected.as_ref().and_then(|node| node.data.cloned());
        let mut body = v_flex().gap_3();
        if let Some(id) = node {
            if self.tab == InspectorTab::Configure {
                body = body.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child("Read-only preview. Your parent workflow is preserved."),
                );
                if let Some(data) = &data {
                    body = body.children(config_details(data).into_iter().map(|(name, value)| {
                        v_flex()
                            .gap_1()
                            .child(div().text_xs().text_color(muted).child(name))
                            .child(
                                div()
                                    .text_sm()
                                    .child(value.chars().take(PREVIEW_CHARS).collect::<String>()),
                            )
                    }));
                }
                let names = self.graph.read(cx).published_names(id);
                if !names.is_empty() {
                    body = body.child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(format!("Published as {}", names.join(", "))),
                    );
                }
            } else if self.tab == InspectorTab::Results {
                if let Some(result) = self.results.get(&id) {
                    match &result.result {
                        Ok(output) => {
                            body = body.child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(output.label.clone()),
                            );
                            if result.named_outputs.is_empty() {
                                body = body.children(output.as_text().map(|text| {
                                    div()
                                        .text_xs()
                                        .child(text.chars().take(PREVIEW_CHARS).collect::<String>())
                                }));
                            }
                        }
                        Err(error) => {
                            body =
                                body.child(div().text_sm().text_color(danger).child(error.clone()))
                        }
                    }
                    body = body.child(self.render_named_outputs(id, cx));
                } else {
                    body = body.child(div().text_xs().text_color(muted).child(
                        if self.preview_live {
                            "This node has not produced a result yet."
                        } else {
                            "No recorded result for this node."
                        },
                    ));
                }
                body = body.child(self.render_activity(cx));
            }
            if matches!(data, Some(NodeData::Workflow(_))) {
                body = body.child(
                    Button::new("preview-open-child")
                        .outline()
                        .small()
                        .label("Open child graph")
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.open_workflow(id, cx)
                        })),
                );
            }
        } else {
            body = body.child(div().text_sm().text_color(muted).child("Select a node to inspect its configuration and recorded outputs. Double-click a Workflow card to go deeper."));
            if self.tab == InspectorTab::Results {
                body = body.child(self.render_activity(cx));
            }
        }
        if self.tab == InspectorTab::History {
            body = v_flex().gap_3();
            if let Some(run) = &self.preview_run {
                body =
                    body.child(div().text_sm().child(run.headline()))
                        .child(
                            div().text_xs().text_color(muted).child(
                                "This canvas shows the graph snapshot captured for this run.",
                            ),
                        )
                        .children(run.inputs.iter().map(|(name, value)| {
                            div().text_xs().child(format!(
                                "{name}: {}",
                                value.chars().take(240).collect::<String>()
                            ))
                        }));
            } else {
                body = body.child(div().text_sm().text_color(muted).child(if self.preview_live {
                    "This execution is still running."
                } else if self.preview_snapshot {
                    "This graph was prepared for the displayed run, but the child did not execute."
                } else {
                    "This is the saved graph. Open a run from the parent history to inspect its snapshot."
                }));
            }
        }
        v_flex()
            .size_full()
            .min_w_0()
            .bg(bg)
            .border_l_1()
            .border_color(border)
            .child(
                v_flex()
                    .gap_1()
                    .px_3()
                    .py_3()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(if self.preview_live {
                                "Live child execution"
                            } else if self.preview_snapshot {
                                "Run snapshot · read only"
                            } else {
                                "Saved workflow · read only"
                            }),
                    ),
            )
            .child(self.render_tabs(cx))
            .child(
                div()
                    .id("preview-inspector")
                    .track_scroll(&self.scroll)
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_3()
                    .child(body),
            )
            .into_any_element()
    }
}

fn config_details(data: &NodeData) -> Vec<(String, String)> {
    let pairs: Vec<(&str, String)> = match data {
        NodeData::Input(c) => vec![
            ("Input type", c.kind.label().into()),
            ("Default", c.default.clone()),
            ("Required", c.required.to_string()),
        ],
        NodeData::Workflow(c) => {
            let mut pairs = vec![
                ("Workflow", c.graph.clone()),
                (
                    "Outputs",
                    if c.output.is_empty() {
                        "Published outputs".into()
                    } else {
                        c.output.clone()
                    },
                ),
            ];
            for (name, binding) in &c.inputs {
                pairs.push((
                    "Input mapping",
                    match binding {
                        calls::Binding::Value { value } => format!("{name} ← {value}"),
                        calls::Binding::Output { node } => format!("{name} ← {node}"),
                    },
                ));
            }
            pairs
        }
        NodeData::Agent(c) => vec![
            ("Provider", c.provider.label().into()),
            ("Model", c.model.clone()),
            ("Prompt", c.prompt.clone()),
            ("Message", c.message.clone()),
            ("Output", c.output.label().into()),
        ],
        NodeData::Database(c) => vec![
            ("Engine", c.engine.label().into()),
            ("Database", c.path.clone()),
            ("Query", c.query.clone()),
            ("Read only", c.read_only.to_string()),
        ],
        NodeData::Script(c) => vec![
            ("Script", c.path.clone()),
            ("Runtime", c.runtime.label().into()),
            ("Arguments", validate::join_args(&c.args)),
        ],
        NodeData::Data(c) => vec![
            ("File", c.path.clone()),
            ("Format", c.format.label().into()),
        ],
        NodeData::Directory(c) => vec![
            ("Directory", c.path.clone()),
            ("Include", c.include.join(", ")),
            ("Exclude", c.exclude.join(", ")),
        ],
        NodeData::File(c) => vec![
            ("Destination", c.path.clone()),
            ("Mode", c.mode.label().into()),
            ("Review before writing", c.review.to_string()),
        ],
    };
    pairs
        .into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect()
}

impl Drop for Inspector {
    fn drop(&mut self) {
        if let Some(job) = &self.job {
            job.cancel.store(true, Ordering::Relaxed);
        }
    }
}

impl Render for Inspector {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.preview {
            return self.render_preview(cx);
        }
        let theme = cx.theme();
        let bg = theme.sidebar;
        let border = theme.sidebar_border;
        let fg = theme.sidebar_foreground;
        let muted = theme.muted_foreground;
        let danger = theme.danger;
        let success = theme.success;
        let panel = theme.background;

        let data = self
            .node
            .and_then(|id| self.graph.read(cx).node_data(id).cloned());
        let (title, swatch) = match (self.node, &data) {
            (None, _) => ("Inspector", None),
            (Some(_), None) => ("Blank node", None),
            (Some(_), Some(data)) => (data.label(), Some(data_color(data))),
        };
        let corner = self.node.map(|id| match self.graph.read(cx).node_name(id) {
            Some(name) => format!("{name} · n{id}"),
            None => format!("n{id}"),
        });
        let wiring = self
            .node
            .map(|id| self.wiring_issues(id, data.as_ref(), cx))
            .unwrap_or_default();
        // The name is the node's, whatever its content, so it heads both forms.
        let name_field = self.node.map(|_| {
            v_form().small().child(
                field()
                    .label("Name")
                    .description(
                        "The canvas label. An agent wired after this node can write {{name}} for its output.",
                    )
                    .child(Input::new(&self.node_name).small()),
            )
        });
        let header = h_flex()
            .flex_shrink_0()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(border)
            .items_center()
            .children(swatch.map(|color| {
                div()
                    .flex_shrink_0()
                    .w(px(12.0))
                    .h(px(12.0))
                    .rounded_sm()
                    .bg(color)
            }))
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(fg)
                    .child(title),
            )
            .children((self.node.is_some() && self.kind.is_some()).then(|| {
                let running = self.job.is_some();
                let cancelling = self
                    .job
                    .as_ref()
                    .is_some_and(|j| j.cancel.load(Ordering::Relaxed));
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("run")
                            .primary()
                            .xsmall()
                            .label(if running { "Running…" } else { "Run" })
                            .disabled(running || self.run_form.is_some())
                            .loading(running)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.run_selected(false, window, cx)
                            })),
                    )
                    .when(!running, |d| {
                        d.child(
                            Button::new("run-fresh")
                                .outline()
                                .xsmall()
                                .label("Run fresh")
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.run_selected(true, window, cx)
                                })),
                        )
                    })
                    .when(running, |d| {
                        d.child(
                            Button::new("cancel-run")
                                .danger()
                                .xsmall()
                                .label(if cancelling {
                                    "Cancelling…"
                                } else {
                                    "Cancel"
                                })
                                .disabled(cancelling)
                                .on_click(
                                    cx.listener(|this, _: &ClickEvent, _, cx| this.cancel_run(cx)),
                                ),
                        )
                    })
            }))
            .children(corner.map(|text| div().ml_auto().text_xs().text_color(muted).child(text)));

        let hint = |text: String| div().text_xs().text_color(muted).child(text);
        let issue_line = |i: &Issue| {
            div()
                .text_xs()
                .text_color(danger)
                .child(format!("{}: {}", i.field, i.message))
                .into_any_element()
        };

        let node_body: AnyElement = match (self.node, self.kind) {
            (None, _) => hint(
                "Select a node to edit it. Double-click the canvas or press Add node to make one."
                    .into(),
            )
            .into_any_element(),
            (Some(_), None) => v_flex()
                .gap_3()
                .children(name_field)
                .children(wiring.iter().map(issue_line))
                .child(hint("This node has no content yet. Give it some:".into()))
                .child(self.render_type_buttons(cx))
                .into_any_element(),
            (Some(id), Some(ty)) => {
                let form: AnyElement = match ty {
                    NodeType::Workflow => self.render_workflow(cx).into_any_element(),
                    NodeType::Input => self.render_input_config(cx).into_any_element(),
                    NodeType::Agent => self.render_agent(cx).into_any_element(),
                    NodeType::Database => self.render_database(cx).into_any_element(),
                    NodeType::Script => self.render_script(cx).into_any_element(),
                    NodeType::Data => self.render_data(cx).into_any_element(),
                    NodeType::Directory => self.render_directory(cx).into_any_element(),
                    NodeType::File => self.render_file(cx).into_any_element(),
                };
                let inputs_section = match &data {
                    Some(NodeData::Agent(agent)) => {
                        Some(self.render_inputs(id, agent, cx).into_any_element())
                    }
                    _ => None,
                };
                let issues: Vec<AnyElement> = if self.issues.is_empty() && wiring.is_empty() {
                    vec![
                        div()
                            .text_xs()
                            .text_color(success)
                            .child("✓ Configuration is valid")
                            .into_any_element(),
                    ]
                } else {
                    self.issues
                        .iter()
                        .chain(wiring.iter())
                        .map(issue_line)
                        .collect()
                };
                let saveable = matches!(ty, NodeType::Agent | NodeType::Database);
                let save_row = saveable.then(|| {
                    v_flex()
                        .gap_1()
                        .child(
                            h_flex()
                                .gap_2()
                                .child(Input::new(&self.preset_name).small())
                                .child(
                                    Button::new("save-preset")
                                        .primary()
                                        .small()
                                        .label(match ty {
                                            NodeType::Agent => "Save agent",
                                            _ => "Save database",
                                        })
                                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                            this.save_preset(cx)
                                        })),
                                ),
                        )
                        .child(hint(match ty {
                            NodeType::Agent => format!(
                                "Saves to {}/<name>/agent.json; runs go to its outputs/ folder.",
                                library::AGENTS_DIR
                            ),
                            _ => format!("Saves to {}/<name>.json.", library::DATABASES_DIR),
                        }))
                        .when(ty == NodeType::Agent && self.output_count > 0, |d| {
                            d.child(hint(format!(
                                "{} saved output{}",
                                self.output_count,
                                if self.output_count == 1 { "" } else { "s" }
                            )))
                        })
                });
                let preview_box = |text: String| {
                    div()
                        .p_2()
                        .rounded_md()
                        .bg(panel)
                        .text_xs()
                        .text_color(fg)
                        .child(text)
                };
                // What this node produced the last time a run included it, whether it was
                // the target or somewhere upstream.
                let output_section = self.node.and_then(|id| self.results.get(&id)).map(|last| {
                    let changed = data.as_ref() != last.data.as_ref();
                    let (headline, color) = match &last.result {
                        Ok(output) => (format!("Last output · {}", output.label), fg),
                        Err(e) if e == exec::CANCELLED => {
                            ("Last run was cancelled here".to_string(), muted)
                        }
                        Err(e) => (format!("✗ Failed here: {e}"), danger),
                    };
                    let output = last.result.as_ref().ok();
                    let saved = output
                        .and_then(|o| o.saved.clone())
                        .map(|p| hint(format!("saved {}", p.display())));
                    let preview = output
                        .filter(|_| last.named_outputs.is_empty())
                        .and_then(|o| o.as_text())
                        .map(|text| {
                            let shown: String = text.chars().take(PREVIEW_CHARS).collect();
                            let cut = text.chars().count() > PREVIEW_CHARS;
                            v_flex()
                                .gap_1()
                                .child(preview_box(shown))
                                .when(cut, |d| d.child(hint("… truncated in this preview".into())))
                        });
                    let binary = output
                        .filter(|o| !o.text)
                        .map(|o| hint(format!("{}: binary, no preview", o.label)));
                    // The same node's output in the run before this one, to compare.
                    let previous = self.previous_run(id).map(|(run, then)| {
                        let now_key = output
                            .zip(last.data.as_ref())
                            .map(|(o, d)| format!("{:032x}", exec::content_key(d.node_type(), o)));
                        let now_preview: Option<String> = output
                            .and_then(|o| o.as_text())
                            .map(|t| t.chars().take(PREVIEW_CHARS).collect());
                        let same = match (&then.key, &now_key) {
                            (Some(a), Some(b)) => a == b,
                            _ => {
                                then.error.is_none()
                                    && then.label == output.map(|o| o.label.clone())
                                    && now_preview.as_deref() == Some(then.preview.as_str())
                            }
                        };
                        let reconfigured = then.data != last.data;
                        let headline = format!(
                            "Previous · {} UTC · {}",
                            run.time(),
                            then.label.clone().unwrap_or_else(|| "no output".into())
                        );
                        v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(muted)
                                    .child(headline),
                            )
                            .when_some(then.error.clone(), |d, e| {
                                d.child(
                                    div()
                                        .text_xs()
                                        .text_color(danger)
                                        .child(format!("✗ Failed then: {e}")),
                                )
                            })
                            .when(then.error.is_none() && same, |d| {
                                d.child(hint("Same output as now.".into()))
                            })
                            .when(
                                then.error.is_none() && !same && !then.preview.is_empty(),
                                |d| {
                                    d.child(preview_box(then.preview.clone()))
                                        .when(!then.complete, |d| {
                                            d.child(hint("… truncated in this preview".into()))
                                        })
                                },
                            )
                            .when(reconfigured, |d| {
                                d.child(hint("The node was configured differently then.".into()))
                            })
                    });
                    v_flex()
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(color)
                                .child(headline),
                        )
                        .when(changed, |d| {
                            d.child(div().text_xs().text_color(danger).child(
                                "The node changed since this ran; run again for a current result.",
                            ))
                        })
                        .children(saved)
                        .children(preview)
                        .children(binary)
                        .children(previous)
                });
                let cached = lock(&self.cache).len();
                let cache_row = (cached > 0).then(|| {
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(hint(format!(
                            "{cached} output{} cached in {}/ across sessions; Run reuses them when nothing upstream changed.",
                            if cached == 1 { "" } else { "s" },
                            library::CACHE_DIR
                        )))
                        .child(
                            Button::new("clear-cache")
                                .ghost()
                                .xsmall()
                                .label("Clear")
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.clear_cache(cx)
                                })),
                        )
                });
                let configuration = v_flex()
                    .gap_3()
                    .children(name_field)
                    .child(form)
                    .children(inputs_section)
                    .child(div().h(px(1.0)).flex_shrink_0().bg(border))
                    .child(v_flex().gap_1().children(issues))
                    .child(v_form().small().child(field().label("Publish this output")
                        .description("Give this result a name for other workflows. Separate additional names with commas.")
                        .child(v_flex().gap_2()
                            .when(ty == NodeType::Workflow, |d| d.child(Select::new(&self.publish_source).small().w_full()))
                            .child(Input::new(&self.published).small()))))
                    .children(self.publish_issue.as_ref().map(|error| div().text_xs().text_color(danger).child(error.clone())))
                    .children(save_row)
                    .child(div().h(px(1.0)).flex_shrink_0().bg(border))
                    .child(Button::new("node-options").ghost().small().label(if self.show_advanced { "Hide node options" } else { "Node options" })
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| { this.show_advanced = !this.show_advanced; cx.notify(); })))
                    .when(self.show_advanced, |d| d.child(
                        v_flex().gap_2()
                            .child(self.render_type_buttons(cx))
                            .child(
                                Button::new("clear-content")
                                    .ghost()
                                    .small()
                                    .label("Make blank")
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.clear_content(window, cx)
                                    })),
                            ),
                    ));
                if self.tab == InspectorTab::Configure {
                    configuration.into_any_element()
                } else {
                    v_flex()
                        .gap_3()
                        .children(output_section)
                        .child(self.render_named_outputs(id, cx))
                        .children(
                            (ty == NodeType::Workflow)
                                .then(|| self.render_workflow_details(id, cx)),
                        )
                        .when(!self.results.contains_key(&id) && self.job.is_none(), |d| {
                            d.child(hint("Run this node to inspect its output here.".into()))
                        })
                        .child(self.render_activity(cx))
                        .children(cache_row)
                        .into_any_element()
                }
            }
        };
        let body = v_flex()
            .gap_3()
            .children(self.render_run_form(cx))
            .children(self.render_review(cx))
            .child(if self.tab == InspectorTab::History {
                self.render_runs(cx).into_any_element()
            } else if self.tab == InspectorTab::Results && self.node.is_none() {
                self.render_activity(cx)
            } else {
                node_body
            });

        v_flex()
            .size_full()
            .min_w_0()
            .bg(bg)
            .border_l_1()
            .border_color(border)
            .child(header)
            .child(self.render_tabs(cx))
            .child(
                div()
                    .id("inspector-body")
                    .track_scroll(&self.scroll)
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_3()
                    .child(body),
            )
            .into_any_element()
    }
}
