//! The inspector: the panel to the right of the canvas that shows the selected node's
//! content as a form and writes every change straight back into the node.
//!
//! A blank node offers the four content types; a typed node shows its type's fields,
//! live validation from [`NodeData::validate`], and a row to save the config into the
//! library (agents and databases) so it shows up in the sidebar for reuse. Agent prompts
//! can be saved to and loaded from `prompts/` on their own.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::form::{field, v_form};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme, Disableable, IndexPath, Sizable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::str::FromStr;

use crate::exec::{self, Report};
use crate::graph::{Graph, GraphEvent, NodeId, data_color};
use crate::library;
use crate::node_data::{
    AgentConfig, Auth, DataConfig, DataFormat, DatabaseConfig, DbEngine, DirectoryConfig,
    ModelType, NodeData, NodeType, OutputType, Provider, Runtime, ScriptConfig,
    WRITABLE_DATABASE_LABEL,
};
use crate::providers::{self, UreqHttp};
use crate::validate::{self, Issue};

/// Initial inspector width; the divider next to it resizes it.
pub const INSPECTOR_W: f32 = 320.0;

pub enum InspectorEvent {
    /// A preset or prompt was written or removed; the sidebar should re-list.
    LibraryChanged,
    Status {
        text: String,
        error: bool,
    },
}

impl EventEmitter<InspectorEvent> for Inspector {}

type StrSelect = SelectState<Vec<SharedString>>;

pub struct Inspector {
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
    running: bool,
    /// Result of the last run started from this panel.
    report: Option<Report>,
    preset_name: Entity<InputState>,
    // agent
    provider: Entity<StrSelect>,
    endpoint: Entity<InputState>,
    model_type: Entity<StrSelect>,
    model: Entity<InputState>,
    auth_env: Entity<InputState>,
    prompt: Entity<TextareaState>,
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

fn set_text(state: &Entity<InputState>, value: String, window: &mut Window, cx: &mut App) {
    state.update(cx, |state, cx| state.set_value(value, window, cx));
}

fn set_textarea(state: &Entity<TextareaState>, value: String, window: &mut Window, cx: &mut App) {
    state.update(cx, |state, cx| state.set_value(value, window, cx));
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

        let preset_name = text("preset name", window, cx);
        let provider = enum_select(&Provider::ALL, Provider::label, window, cx);
        let endpoint = text(Provider::Ollama.default_endpoint(), window, cx);
        let model_type = enum_select(&ModelType::ALL, ModelType::label, window, cx);
        let model = text("model id", window, cx);
        let auth_env = text("ANTHROPIC_API_KEY", window, cx);
        let prompt = area("You are…", 6, window, cx);
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

        let mut subscriptions =
            vec![
                cx.subscribe_in(&graph, window, |this, _, event: &GraphEvent, window, cx| {
                    match event {
                        GraphEvent::SelectionChanged => this.reload(window, cx),
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
        for state in [&prompt, &query] {
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
        for state in [&output, &engine, &runtime, &format] {
            subscriptions.push(cx.subscribe_in(
                state,
                window,
                |this, _, _: &SelectEvent<Vec<SharedString>>, _, cx| this.sync(cx),
            ));
        }
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
            graph,
            node: None,
            kind: None,
            issues: Vec::new(),
            loading: false,
            output_count: 0,
            running: false,
            report: None,
            preset_name,
            provider,
            endpoint,
            model_type,
            model,
            auth_env,
            prompt,
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
            _subscriptions: subscriptions,
        };
        inspector.reload(window, cx);
        inspector
    }

    fn status(&mut self, text: String, error: bool, cx: &mut Context<Self>) {
        cx.emit(InspectorEvent::Status { text, error });
    }

    // ----- graph <-> form -----

    /// Fill the form from whatever the graph has selected.
    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.loading = true;
        let selected = {
            let graph = self.graph.read(cx);
            graph
                .selected()
                .map(|v| (v.id, v.preset.clone(), v.data.cloned()))
        };
        match selected {
            None => {
                self.node = None;
                self.kind = None;
                self.issues.clear();
                self.output_count = 0;
                self.report = None;
            }
            Some((id, preset, data)) => {
                if self.node != Some(id) {
                    self.report = None;
                }
                self.node = Some(id);
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
        }
    }

    /// Write the form into the selected node and re-validate.
    fn sync(&mut self, cx: &mut Context<Self>) {
        if self.loading {
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
        };
        (data, issues)
    }

    // ----- field reactions -----

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

    // ----- actions -----

    /// Give a blank node content, or replace a typed node's content.
    fn set_content(&mut self, data: NodeData, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.node else { return };
        self.graph.update(cx, |graph, cx| {
            graph.set_node_data(id, Some(data), cx);
            graph.set_node_label(id, None, cx);
        });
        self.reload(window, cx);
    }

    fn clear_content(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.node else { return };
        self.graph.update(cx, |graph, cx| {
            graph.set_node_data(id, None, cx);
            graph.set_node_label(id, None, cx);
        });
        self.reload(window, cx);
    }

    /// Save the node's content under the preset name, so it appears in the sidebar.
    fn save_preset(&mut self, cx: &mut Context<Self>) {
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
            NodeData::Script(_) | NodeData::Data(_) | NodeData::Directory(_) => {
                self.status(
                    format!(
                        "Scripts, data and directories are paths: put files in {}/ or {}/ and they appear in the sidebar",
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
        if self.kind != Some(NodeType::Agent) {
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
            NodeType::Agent | NodeType::Database | NodeType::Directory => return false,
        }
        true
    }

    /// Run the selected node and everything wired into it, off the UI thread.
    fn run_selected(&mut self, cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        let Some(id) = self.node else { return };
        let doc = self.graph.read(cx).snapshot("run");
        let node_id = format!("n{id}");
        self.running = true;
        self.report = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let report = cx
                .background_spawn(async move {
                    let http = UreqHttp::new(exec::HTTP_TIMEOUT);
                    exec::run(
                        &doc,
                        &node_id,
                        exec::Dirs::from_cwd(),
                        &http,
                        &exec::process_env,
                    )
                })
                .await;
            this.update(cx, |this, cx| {
                let (text, error) = match &report.result {
                    Ok(output) => {
                        let saved = output
                            .saved
                            .as_ref()
                            .map(|p| format!(" → {}", p.display()))
                            .unwrap_or_default();
                        (
                            format!(
                                "Run finished in {:.1}s: {}{saved}",
                                report.elapsed.as_secs_f32(),
                                output.label
                            ),
                            false,
                        )
                    }
                    Err(e) => (format!("Run failed: {e}"), true),
                };
                this.running = false;
                if let Ok(output) = &report.result
                    && output.saved.is_some()
                {
                    this.output_count += 1;
                }
                this.report = Some(report);
                this.status(text, error, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
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
                            "Saved prompts live in {}/ and load from the sidebar.",
                            library::PROMPTS_DIR
                        ))),
                ),
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

    /// One button per content type, with the writable database after the read-only one.
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
}

impl Render for Inspector {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                Button::new("run")
                    .primary()
                    .xsmall()
                    .label(if self.running { "Running…" } else { "Run" })
                    .disabled(self.running)
                    .loading(self.running)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.run_selected(cx)))
            }))
            .children(self.node.map(|id| {
                div()
                    .ml_auto()
                    .text_xs()
                    .text_color(muted)
                    .child(format!("n{id}"))
            }));

        let hint = |text: String| div().text_xs().text_color(muted).child(text);

        let body: AnyElement = match (self.node, self.kind) {
            (None, _) => hint(
                "Select a node to edit it. Double-click the canvas or press Add node to make one."
                    .into(),
            )
            .into_any_element(),
            (Some(_), None) => v_flex()
                .gap_3()
                .child(hint("This node has no content yet. Give it some:".into()))
                .child(self.render_type_buttons(cx))
                .into_any_element(),
            (Some(_), Some(ty)) => {
                let form: AnyElement = match ty {
                    NodeType::Agent => self.render_agent(cx).into_any_element(),
                    NodeType::Database => self.render_database(cx).into_any_element(),
                    NodeType::Script => self.render_script(cx).into_any_element(),
                    NodeType::Data => self.render_data(cx).into_any_element(),
                    NodeType::Directory => self.render_directory(cx).into_any_element(),
                };
                let issues: Vec<AnyElement> = if self.issues.is_empty() {
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
                        .map(|i| {
                            div()
                                .text_xs()
                                .text_color(danger)
                                .child(format!("{}: {}", i.field, i.message))
                                .into_any_element()
                        })
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
                let run_section = self.report.as_ref().map(|report| {
                    let (headline, color) = match &report.result {
                        Ok(output) => (
                            format!("✓ {} in {:.1}s", output.label, report.elapsed.as_secs_f32()),
                            success,
                        ),
                        Err(e) => (format!("✗ {e}"), danger),
                    };
                    let output = report.result.as_ref().ok();
                    let saved = output
                        .and_then(|o| o.saved.clone())
                        .map(|p| hint(format!("saved {}", p.display())));
                    let preview = output.and_then(|o| o.as_text()).map(|text| {
                        const LIMIT: usize = 4000;
                        let shown: String = text.chars().take(LIMIT).collect();
                        let cut = text.chars().count() > LIMIT;
                        v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .p_2()
                                    .rounded_md()
                                    .bg(panel)
                                    .text_xs()
                                    .text_color(fg)
                                    .child(shown),
                            )
                            .when(cut, |d| d.child(hint("… truncated in this preview".into())))
                    });
                    v_flex()
                        .gap_1()
                        .child(div().text_xs().text_color(color).child(headline))
                        .children(saved)
                        .children(report.log.iter().map(|line| hint(line.clone())))
                        .children(preview)
                });
                v_flex()
                    .gap_3()
                    .child(form)
                    .child(div().h(px(1.0)).flex_shrink_0().bg(border))
                    .child(v_flex().gap_1().children(issues))
                    .children(run_section)
                    .children(save_row)
                    .child(div().h(px(1.0)).flex_shrink_0().bg(border))
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(hint("Change type:".into()))
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
                    )
                    .into_any_element()
            }
        };

        v_flex()
            .size_full()
            .min_w_0()
            .bg(bg)
            .border_l_1()
            .border_color(border)
            .child(header)
            .child(
                div()
                    .id("inspector-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_3()
                    .child(body),
            )
    }
}
