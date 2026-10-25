//! The workspace: the sidebar of saved graphs, blank node kinds and the per-type library
//! (agents, prompts, databases, scripts, data), the toolbar, the editor, and the inspector.

use crate::doc::{self, GraphDoc, NodeKind};
use crate::graph::{Current, Graph, NodeId, data_color, hsla_of, type_color};
use crate::history;
use crate::inspector::{INSPECTOR_W, Inspector, InspectorEvent};
use crate::library::{self, Preset};
use crate::node_data::{
    AgentConfig, DataConfig, DataFormat, DatabaseConfig, NodeData, NodeType, Runtime, ScriptConfig,
    WRITABLE_DATABASE_LABEL,
};
use crate::validate;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::color_picker::{ColorPicker, ColorPickerState};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{ActiveTheme, Disableable, IndexPath, Sizable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::collections::{BTreeMap, HashSet};

gpui_kit::actions!(workspace, [Save, NewGraph, RunAll]);

pub const KEY_CONTEXT: &str = "Workspace";
/// Initial sidebar width; the divider next to it resizes it.
const SIDEBAR_W: f32 = 260.0;
/// How narrow and how wide either side panel can be dragged.
const PANEL_MIN_W: f32 = 200.0;
const PANEL_MAX_W: f32 = 720.0;
/// First entry of the kind dropdown: a plain node.
const BLANK: &str = "Blank";
/// Starting color in the node creator.
const DEFAULT_KIND_COLOR: Hsla = Hsla {
    h: 0.58,
    s: 0.75,
    l: 0.55,
    a: 1.0,
};

type KindSelect = SelectState<Vec<SharedString>>;

/// Sidebar sections, by the key used to remember which are collapsed.
const GRAPHS: &str = "graphs";
const NODES: &str = "nodes";
const MAX_CHILD_DEPTH: usize = 8;
const KINDS: &str = "kinds";
const AGENTS: &str = "agents";
const PROMPTS: &str = "prompts";
const DATABASES: &str = "databases";
const SCRIPTS: &str = "scripts";
const DATA: &str = "data";

pub struct Workspace {
    graph: Entity<Graph>,
    inspector: Entity<Inspector>,
    /// Preview entities live separately so parent edits and active runs stay intact.
    children: Vec<ChildView>,
    search: Entity<InputState>,
    query: String,
    /// Name field for the graph being edited.
    name: Entity<InputState>,
    files: Vec<String>,
    /// Name of the graph on disk that the editor currently reflects.
    current: Option<String>,
    /// Node creator fields.
    kind_name: Entity<InputState>,
    kind_color: Entity<ColorPickerState>,
    /// Toolbar dropdown choosing what Add node creates.
    kind_select: Entity<KindSelect>,
    kinds: Vec<NodeKind>,
    agents: Vec<Preset<AgentConfig>>,
    prompts: Vec<String>,
    databases: Vec<Preset<DatabaseConfig>>,
    scripts: Vec<String>,
    data: Vec<String>,
    collapsed: HashSet<&'static str>,
    status: Option<Status>,
    _subscriptions: Vec<Subscription>,
}

struct ChildView {
    graph: Entity<Graph>,
    inspector: Entity<Inspector>,
    label: String,
    path: String,
    snapshot: bool,
    _subscription: Subscription,
}

struct Status {
    text: String,
    error: bool,
}

/// Dropdown entries: Blank, the content types (with the writable database after the
/// read-only one), then every saved kind in list order.
fn select_items(kinds: &[NodeKind]) -> Vec<SharedString> {
    std::iter::once(SharedString::from(BLANK))
        .chain(NodeType::ALL.iter().flat_map(|t| {
            let mut labels = vec![SharedString::from(t.label())];
            if *t == NodeType::Database {
                labels.push(SharedString::from(WRITABLE_DATABASE_LABEL));
            }
            labels
        }))
        .chain(kinds.iter().map(|k| SharedString::from(k.name.clone())))
        .collect()
}

/// What a dropdown entry means for new nodes.
fn current_of(value: &str) -> Current {
    if value == BLANK {
        Current::Blank
    } else if value == WRITABLE_DATABASE_LABEL {
        Current::WritableDatabase
    } else if let Some(ty) = NodeType::from_label(value) {
        Current::Type(ty)
    } else {
        Current::Kind(value.to_string())
    }
}

/// Node content pointing at a file the user dropped into `scripts/` or `data/`.
fn file_data(ty: NodeType, path: &str) -> NodeData {
    let ext = validate::extension(path).unwrap_or_default();
    match ty {
        NodeType::Script => NodeData::Script(ScriptConfig {
            runtime: Runtime::for_extension(&ext).unwrap_or(Runtime::Python),
            path: path.to_string(),
            ..ScriptConfig::default()
        }),
        _ => NodeData::Data(DataConfig {
            path: path.to_string(),
            format: DataFormat::for_extension(&ext),
        }),
    }
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let kinds = doc::list_kinds();
        let graph = cx.new(|cx| {
            let mut graph = Graph::new(cx);
            graph.set_kinds(&kinds, cx);
            graph
        });
        let inspector = cx.new(|cx| Inspector::new(graph.clone(), window, cx));
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("graph name")
                .default_value("untitled")
        });
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search library…"));
        let kind_name = cx.new(|cx| InputState::new(window, cx).placeholder("node name"));
        let kind_color =
            cx.new(|cx| ColorPickerState::new(window, cx).default_value(DEFAULT_KIND_COLOR));
        let kind_select = cx
            .new(|cx| SelectState::new(select_items(&kinds), Some(IndexPath::new(0)), window, cx));

        let subscriptions = vec![
            cx.subscribe_in(&search, window, |this, input, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.query = input.read(cx).value().trim().to_lowercase();
                    cx.notify();
                }
            }),
            cx.subscribe_in(&name, window, |this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.save_now(cx);
                }
            }),
            cx.subscribe_in(
                &kind_name,
                window,
                |this, _, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.save_kind_now(window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &kind_select,
                window,
                |this, _, event: &SelectEvent<Vec<SharedString>>, _, cx| match event {
                    SelectEvent::Confirm(value) => {
                        let current = value
                            .as_ref()
                            .map_or(Current::Blank, |v| current_of(v.as_ref()));
                        this.graph
                            .update(cx, |graph, cx| graph.set_current(current, cx));
                    }
                },
            ),
            cx.subscribe_in(&inspector, window, |this, _, event, window, cx| {
                this.on_inspector_event(event, window, cx);
            }),
        ];

        graph.read(cx).focus_handle.clone().focus(window, cx);
        let mut workspace = Self {
            graph,
            inspector,
            children: Vec::new(),
            search,
            query: String::new(),
            name,
            files: doc::list(),
            current: None,
            kind_name,
            kind_color,
            kind_select,
            kinds,
            agents: Vec::new(),
            prompts: Vec::new(),
            databases: Vec::new(),
            scripts: Vec::new(),
            data: Vec::new(),
            collapsed: HashSet::from([KINDS, AGENTS, PROMPTS, DATABASES, SCRIPTS, DATA]),
            status: None,
            _subscriptions: subscriptions,
        };
        workspace.refresh_library();
        workspace
    }

    fn on_inspector_event(
        &mut self,
        event: &InspectorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InspectorEvent::LibraryChanged => self.refresh_library(),
            InspectorEvent::Status { text, error } => {
                self.status = Some(Status {
                    text: text.clone(),
                    error: *error,
                });
            }
            InspectorEvent::OpenWorkflow { node, graph, run } => {
                self.open_child(*node, graph, run.as_deref().cloned(), window, cx);
            }
            InspectorEvent::ExecutionChanged => {
                for child in &mut self.children {
                    if let Some(execution) = self.inspector.read(cx).child_execution(&child.path) {
                        child.snapshot = true;
                        child.inspector.update(cx, |inspector, cx| {
                            inspector.set_live_preview(execution, cx)
                        });
                    }
                }
            }
        }
        cx.notify();
    }

    fn active_graph(&self) -> Entity<Graph> {
        self.children
            .last()
            .map_or_else(|| self.graph.clone(), |view| view.graph.clone())
    }

    fn active_inspector(&self) -> Entity<Inspector> {
        self.children
            .last()
            .map_or_else(|| self.inspector.clone(), |view| view.inspector.clone())
    }

    fn open_child(
        &mut self,
        node: NodeId,
        doc: &GraphDoc,
        run: Option<history::Run>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.children.len() >= MAX_CHILD_DEPTH {
            self.err(format!(
                "Workflow navigation is limited to {MAX_CHILD_DEPTH} levels."
            ));
            return;
        }
        let path = self.children.last().map_or_else(
            || format!("n{node}"),
            |parent| format!("{}/n{node}", parent.path),
        );
        let execution = self.inspector.read(cx).child_execution(&path);
        // Live preview inspectors ask the root to resolve an exact snapshot path.
        // A waiting root Run form still allows ordinary saved-workflow previews.
        if execution.is_none() && doc == &GraphDoc::default() {
            self.ok(
                "No prepared snapshot is available for this child in the displayed run.".into(),
            );
            return;
        }
        let doc = execution.as_ref().map_or(doc, |execution| &execution.graph);
        let label = self
            .active_graph()
            .read(cx)
            .node_name(node)
            .map(str::to_string)
            .unwrap_or_else(|| {
                if doc.name.is_empty() {
                    format!("Workflow n{node}")
                } else {
                    doc.name.clone()
                }
            });
        let targets = execution
            .as_ref()
            .and_then(|execution| execution.run.as_ref())
            .or(run.as_ref())
            .map_or(&[][..], |run| run.targets.as_slice());
        let selected = preview_node(doc, targets);
        let workflows = execution
            .as_ref()
            .map(|execution| execution.workflows.clone())
            .unwrap_or_else(|| {
                run.as_ref()
                    .map(|run| {
                        run.workflows
                            .iter()
                            .filter(|(id, _)| !id.contains('/'))
                            .map(|(id, graph)| (id.clone(), graph.clone()))
                            .chain(
                                run.calls
                                    .iter()
                                    .map(|(id, call)| (id.clone(), call.graph.clone())),
                            )
                            .collect::<BTreeMap<_, _>>()
                    })
                    .unwrap_or_default()
            });
        let snapshot = run.is_some() || execution.is_some();
        let graph = cx.new(|cx| {
            let mut graph = Graph::new(cx);
            graph.set_kinds(&self.kinds, cx);
            graph.restore(doc, cx);
            if snapshot {
                graph.set_workflow_snapshots(&workflows, cx);
            }
            graph.set_read_only(true, cx);
            if let Some(selected) = selected {
                graph.select_node(selected, cx);
            }
            graph
        });
        let inspector = cx.new(|cx| {
            let mut inspector = Inspector::new(graph.clone(), window, cx);
            inspector.set_preview(run, cx);
            if let Some(execution) = execution {
                inspector.set_live_preview(execution, cx);
            }
            inspector
        });
        let subscription = cx.subscribe_in(&inspector, window, |this, _, event, window, cx| {
            this.on_inspector_event(event, window, cx);
        });
        graph.read(cx).focus_handle.clone().focus(window, cx);
        self.children.push(ChildView {
            graph,
            inspector,
            label,
            path,
            snapshot,
            _subscription: subscription,
        });
        cx.notify();
    }

    /// Keep every ancestor's entities and view; only discard deeper previews on return.
    fn return_to(&mut self, depth: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.children.truncate(depth);
        self.active_graph()
            .read(cx)
            .focus_handle
            .clone()
            .focus(window, cx);
        cx.notify();
    }

    fn matches(&self, category: &str, value: &str) -> bool {
        library_matches(&self.query, category, value)
    }

    fn ok(&mut self, text: String) {
        self.status = Some(Status { text, error: false });
    }

    fn err(&mut self, text: String) {
        self.status = Some(Status { text, error: true });
    }

    // ----- graphs -----

    fn save_now(&mut self, cx: &mut Context<Self>) {
        if !self.children.is_empty() {
            return;
        }
        let name = doc::sanitize_name(&self.name.read(cx).value());
        let snapshot = self.graph.read(cx).snapshot(&name);
        match doc::save(&snapshot) {
            Ok((md, json)) => {
                self.inspector
                    .update(cx, |inspector, cx| inspector.set_graph_name(&name, cx));
                self.current = Some(name);
                self.ok(format!("Saved {} and {}", md.display(), json.display()));
            }
            Err(text) => self.err(text),
        }
        self.files = doc::list();
        self.refresh_library();
        cx.notify();
    }

    fn load(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        match doc::load(name) {
            Ok(doc) => {
                self.return_to(0, window, cx);
                let (nodes, edges) = (doc.nodes.len(), doc.edges.len());
                self.graph.update(cx, |graph, cx| graph.restore(&doc, cx));
                self.inspector
                    .update(cx, |inspector, cx| inspector.set_graph_name(name, cx));
                self.name.update(cx, |input, cx| {
                    input.set_value(name.to_string(), window, cx)
                });
                self.current = Some(name.to_string());
                self.ok(format!("Loaded {name}: {nodes} nodes, {edges} connections"));
            }
            Err(text) => self.err(text),
        }
        self.files = doc::list();
        self.refresh_library();
        cx.notify();
    }

    fn new_graph(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.return_to(0, window, cx);
        self.graph.update(cx, |graph, cx| graph.clear(cx));
        self.inspector
            .update(cx, |inspector, cx| inspector.set_graph_name("untitled", cx));
        self.name
            .update(cx, |input, cx| input.set_value("untitled", window, cx));
        self.current = None;
        self.status = None;
        cx.notify();
    }

    fn on_save(&mut self, _: &Save, _: &mut Window, cx: &mut Context<Self>) {
        self.save_now(cx);
    }

    fn on_run_all(&mut self, _: &RunAll, window: &mut Window, cx: &mut Context<Self>) {
        if !self.children.is_empty() {
            return;
        }
        self.inspector
            .update(cx, |inspector, cx| inspector.run_all(false, window, cx));
    }

    fn on_new(&mut self, _: &NewGraph, window: &mut Window, cx: &mut Context<Self>) {
        if !self.children.is_empty() {
            return;
        }
        self.new_graph(window, cx);
    }

    // ----- blank node kinds -----

    fn save_kind_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = doc::sanitize_name(&self.kind_name.read(cx).value());
        if name == BLANK
            || name.eq_ignore_ascii_case(WRITABLE_DATABASE_LABEL)
            || NodeType::from_label(&name).is_some()
        {
            self.err(format!(
                "`{name}` is reserved for the node types; pick another name"
            ));
            cx.notify();
            return;
        }
        let color = Rgba::from(
            self.kind_color
                .read(cx)
                .value()
                .unwrap_or(DEFAULT_KIND_COLOR),
        );
        let kind = NodeKind {
            name,
            color: doc::rgb_to_hex(color.r, color.g, color.b),
        };
        match doc::save_kind(&kind) {
            Ok(path) => self.ok(format!("Saved node kind {}", path.display())),
            Err(text) => self.err(text),
        }
        self.refresh_kinds(window, cx);
    }

    fn delete_kind(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        match doc::delete_kind(name) {
            Ok(()) => self.ok(format!("Deleted node kind {name}")),
            Err(text) => self.err(text),
        }
        self.refresh_kinds(window, cx);
    }

    /// Make a saved kind the current one: pick it in the toolbar dropdown and load it
    /// into the creator so its color can be edited and saved again.
    fn select_kind(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(kind) = self.kinds.get(index).cloned() else {
            return;
        };
        let item = select_items(&self.kinds)
            .iter()
            .position(|label| label.as_ref() == kind.name);
        self.kind_select.update(cx, |select, cx| {
            select.set_selected_index(item.map(IndexPath::new), window, cx)
        });
        self.graph.update(cx, |graph, cx| {
            graph.set_current(Current::Kind(kind.name.clone()), cx)
        });
        self.kind_name.update(cx, |input, cx| {
            input.set_value(kind.name.clone(), window, cx)
        });
        if let Some(color) = hsla_of(&kind.color) {
            self.kind_color
                .update(cx, |picker, cx| picker.set_value(color, window, cx));
        }
        cx.notify();
    }

    fn add_kind(&mut self, kind: &NodeKind, cx: &mut Context<Self>) {
        if !self.children.is_empty() {
            return;
        }
        self.graph
            .update(cx, |graph, cx| graph.add_kind_node(kind, cx));
    }

    /// Re-read `nodes/`, hand the kinds to the editor so instances pick up color edits,
    /// and rebuild the dropdown, keeping its selection when that entry still exists.
    fn refresh_kinds(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.kinds = doc::list_kinds();
        let kinds = self.kinds.clone();
        self.graph
            .update(cx, |graph, cx| graph.set_kinds(&kinds, cx));

        let selected = self.kind_select.read(cx).selected_value().cloned();
        let items = select_items(&self.kinds);
        let index = selected
            .and_then(|value| items.iter().position(|item| *item == value))
            .unwrap_or(0);
        let current = current_of(items[index].as_ref());
        self.kind_select.update(cx, |select, cx| {
            select.set_items(items, window, cx);
            select.set_selected_index(Some(IndexPath::new(index)), window, cx);
        });
        self.graph
            .update(cx, |graph, cx| graph.set_current(current, cx));
        cx.notify();
    }

    // ----- library -----

    fn refresh_library(&mut self) {
        self.agents = library::list_agents();
        self.prompts = library::list_prompts();
        self.databases = library::list_databases();
        self.scripts = library::list_scripts();
        self.data = library::list_data();
    }

    fn add_preset(&mut self, name: &str, data: NodeData, cx: &mut Context<Self>) {
        if !self.children.is_empty() {
            return;
        }
        self.graph
            .update(cx, |graph, cx| graph.add_preset(name, data, cx));
    }

    fn delete_agent(&mut self, name: &str, cx: &mut Context<Self>) {
        match library::delete_agent(name) {
            Ok(()) => self.ok(format!("Deleted agent {name}; its outputs/ folder is kept")),
            Err(text) => self.err(text),
        }
        self.refresh_library();
        cx.notify();
    }

    fn delete_database(&mut self, name: &str, cx: &mut Context<Self>) {
        match library::delete_database(name) {
            Ok(()) => self.ok(format!("Deleted database {name}")),
            Err(text) => self.err(text),
        }
        self.refresh_library();
        cx.notify();
    }

    fn delete_prompt(&mut self, name: &str, cx: &mut Context<Self>) {
        match library::delete_prompt(name) {
            Ok(()) => self.ok(format!("Deleted prompt {name}")),
            Err(text) => self.err(text),
        }
        self.refresh_library();
        cx.notify();
    }

    fn apply_prompt(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.children.is_empty() {
            return;
        }
        self.inspector
            .update(cx, |inspector, cx| inspector.apply_prompt(name, window, cx));
    }

    /// Point the selected script/data node at the file, or add a new node for it.
    fn use_file(&mut self, ty: NodeType, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.children.is_empty() {
            return;
        }
        let applied = self
            .inspector
            .update(cx, |inspector, cx| inspector.use_file(ty, path, window, cx));
        if !applied {
            let data = file_data(ty, path);
            self.graph.update(cx, |graph, cx| graph.add_data(data, cx));
        }
    }

    fn toggle_section(&mut self, key: &'static str, cx: &mut Context<Self>) {
        if !self.collapsed.remove(key) {
            self.collapsed.insert(key);
        }
        cx.notify();
    }
}

/// Prefer the returned result, then a published output, then a configured node.
fn preview_node(doc: &GraphDoc, targets: &[String]) -> Option<NodeId> {
    targets
        .iter()
        .chain(doc.outputs.values())
        .chain(
            doc.nodes
                .iter()
                .filter(|node| node.data.is_some())
                .map(|node| &node.id),
        )
        .find_map(|key| {
            let node_key = key.split('.').next()?;
            if !doc.nodes.iter().any(|node| node.id == node_key) {
                return None;
            }
            let digits = node_key.strip_prefix('n')?;
            if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            digits.parse().ok()
        })
}

/// Matching category names makes the entire group discoverable, as well as each item.
fn library_matches(query: &str, category: &str, value: &str) -> bool {
    query.is_empty() || format!("{category} {value}").to_lowercase().contains(query)
}

// ----- rendering helpers -----

#[derive(Clone, Copy)]
struct Colors {
    fg: Hsla,
    muted: Hsla,
    accent: Hsla,
    accent_fg: Hsla,
    border: Hsla,
}

fn row(id: impl Into<ElementId>, c: Colors) -> Stateful<Div> {
    let accent = c.accent;
    h_flex()
        .id(id)
        .gap_2()
        .px_2()
        .py_2()
        .rounded_md()
        .text_sm()
        .text_color(c.fg)
        .cursor_pointer()
        .hover(move |d| d.bg(accent))
}

fn swatch(color: Hsla) -> Div {
    div()
        .flex_shrink_0()
        .w(px(12.0))
        .h(px(12.0))
        .rounded_sm()
        .bg(color)
}

fn ellipsis(text: impl Into<SharedString>) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .child(text.into())
}

fn hint(text: impl Into<SharedString>, c: Colors) -> Div {
    div()
        .px_2()
        .text_xs()
        .text_color(c.muted)
        .child(text.into())
}

fn ghost_button(id: impl Into<ElementId>, label: &'static str) -> Button {
    Button::new(id).ghost().xsmall().label(label)
}

impl Workspace {
    /// A collapsible sidebar section: header row with a chevron, title and count; the body
    /// only when expanded.
    fn section(
        &self,
        key: &'static str,
        title: &'static str,
        count: usize,
        c: Colors,
        body: AnyElement,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let searching = !self.query.is_empty();
        let open = searching || !self.collapsed.contains(key);
        let header = h_flex()
            .id(key)
            .gap_2()
            .items_center()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.toggle_section(key, cx)))
            .child(
                div()
                    .w(px(12.0))
                    .text_xs()
                    .text_color(c.muted)
                    .child(if open { "▾" } else { "▸" }),
            )
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(c.fg)
                    .child(title),
            )
            .child(div().text_xs().text_color(c.muted).child(count.to_string()));
        v_flex().gap_2().child(header).when(open, |d| d.child(body))
    }

    fn render_graphs(&self, c: Colors, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let current = self.current.clone();
        let mut rows = Vec::with_capacity(self.files.len());
        for (i, name) in self.files.iter().enumerate() {
            if !self.matches("workflows", name) {
                continue;
            }
            let active = current.as_deref() == Some(name.as_str());
            let load_name = name.clone();
            rows.push(
                row(("file", i), c)
                    .when(active, |d| d.bg(c.accent).text_color(c.accent_fg))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.load(&load_name, window, cx)
                    }))
                    .child(ellipsis(name.clone())),
            );
        }
        let empty = self
            .files
            .is_empty()
            .then(|| hint("Save a workflow to find it here.", c));
        v_flex().gap_1().children(rows).children(empty)
    }

    fn render_kinds(&self, c: Colors, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let mut rows = Vec::with_capacity(self.kinds.len());
        for (i, kind) in self.kinds.iter().enumerate() {
            if !self.matches("custom nodes kinds", &kind.name) {
                continue;
            }
            let color = hsla_of(&kind.color).unwrap_or(c.muted);
            let add_kind = kind.clone();
            let delete_name = kind.name.clone();
            rows.push(
                row(("kind", i), c)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.select_kind(i, window, cx)
                    }))
                    .child(swatch(color))
                    .child(ellipsis(kind.name.clone()))
                    .child(ghost_button(("add-kind", i), "+").on_click(cx.listener(
                        move |this, _: &ClickEvent, _, cx| {
                            cx.stop_propagation();
                            this.add_kind(&add_kind, cx)
                        },
                    )))
                    .child(ghost_button(("delete-kind", i), "×").on_click(cx.listener(
                        move |this, _: &ClickEvent, window, cx| {
                            cx.stop_propagation();
                            this.delete_kind(&delete_name, window, cx)
                        },
                    ))),
            );
        }
        let empty = self
            .kinds
            .is_empty()
            .then(|| hint("Create a reusable colored node.", c));
        v_flex()
            .gap_2()
            .child(Input::new(&self.kind_name))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(ColorPicker::new(&self.kind_color))
                    .child(
                        Button::new("save-kind")
                            .primary()
                            .small()
                            .label("Save kind")
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.save_kind_now(window, cx)
                            })),
                    ),
            )
            .child(hint("Select to edit · + adds a node", c))
            .child(v_flex().gap_1().children(rows).children(empty))
    }

    fn render_agents(&self, c: Colors, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let color = type_color(NodeType::Agent);
        let mut rows = Vec::with_capacity(self.agents.len());
        for (i, agent) in self.agents.iter().enumerate() {
            if !self.matches(
                "agents",
                &format!("{} {}", agent.name, agent.config.summary()),
            ) {
                continue;
            }
            let (name, config) = (agent.name.clone(), agent.config.clone());
            let delete_name = agent.name.clone();
            rows.push(
                row(("agent", i), c)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.add_preset(&name, NodeData::Agent(config.clone()), cx)
                    }))
                    .child(swatch(color))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(ellipsis(agent.name.clone()))
                            .child(
                                ellipsis(agent.config.summary())
                                    .text_xs()
                                    .text_color(c.muted),
                            ),
                    )
                    .child(ghost_button(("delete-agent", i), "×").on_click(cx.listener(
                        move |this, _: &ClickEvent, _, cx| {
                            cx.stop_propagation();
                            this.delete_agent(&delete_name, cx)
                        },
                    ))),
            );
        }
        let empty = self
            .agents
            .is_empty()
            .then(|| hint("Save an agent from the inspector.", c));
        v_flex()
            .gap_1()
            .child(hint("Select to add a node", c))
            .children(rows)
            .children(empty)
    }

    fn render_prompts(&self, c: Colors, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let mut rows = Vec::with_capacity(self.prompts.len());
        for (i, name) in self.prompts.iter().enumerate() {
            if !self.matches("prompts", name) {
                continue;
            }
            let apply_name = name.clone();
            let delete_name = name.clone();
            rows.push(
                row(("prompt", i), c)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.apply_prompt(&apply_name, window, cx)
                    }))
                    .child(ellipsis(name.clone()))
                    .child(
                        ghost_button(("delete-prompt", i), "×").on_click(cx.listener(
                            move |this, _: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                this.delete_prompt(&delete_name, cx)
                            },
                        )),
                    ),
            );
        }
        let empty = self
            .prompts
            .is_empty()
            .then(|| hint("Save a prompt from an agent inspector.", c));
        v_flex()
            .gap_1()
            .child(hint("Apply to the selected agent", c))
            .children(rows)
            .children(empty)
    }

    fn render_databases(&self, c: Colors, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let mut rows = Vec::with_capacity(self.databases.len());
        for (i, db) in self.databases.iter().enumerate() {
            if !self.matches(
                "databases resources",
                &format!("{} {}", db.name, db.config.summary()),
            ) {
                continue;
            }
            let (name, config) = (db.name.clone(), db.config.clone());
            let color = data_color(&NodeData::Database(config.clone()));
            let delete_name = db.name.clone();
            rows.push(
                row(("database", i), c)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.add_preset(&name, NodeData::Database(config.clone()), cx)
                    }))
                    .child(swatch(color))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(ellipsis(db.name.clone()))
                            .child(ellipsis(db.config.summary()).text_xs().text_color(c.muted)),
                    )
                    .child(
                        ghost_button(("delete-database", i), "×").on_click(cx.listener(
                            move |this, _: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                this.delete_database(&delete_name, cx)
                            },
                        )),
                    ),
            );
        }
        let empty = self
            .databases
            .is_empty()
            .then(|| hint("Save a database from the inspector.", c));
        v_flex()
            .gap_1()
            .child(hint("Select to add a node", c))
            .children(rows)
            .children(empty)
    }

    /// Files under `scripts/` or `data/`: click points the selected node of that type at
    /// the file, or adds a node for it.
    fn render_files(
        &self,
        ty: NodeType,
        files: &[String],
        dir: &'static str,
        c: Colors,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let color = type_color(ty);
        let key = match ty {
            NodeType::Script => "script",
            _ => "datafile",
        };
        let mut rows = Vec::with_capacity(files.len());
        for (i, path) in files.iter().enumerate() {
            if !self.matches(
                if ty == NodeType::Script {
                    "scripts resources"
                } else {
                    "data resources"
                },
                path,
            ) {
                continue;
            }
            let use_path = path.clone();
            rows.push(
                row((key, i), c)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.use_file(ty, &use_path, window, cx)
                    }))
                    .child(swatch(color))
                    .child(ellipsis(path.clone())),
            );
        }
        let empty = files
            .is_empty()
            .then(|| hint(format!("Add files to {dir}/, then refresh the library."), c));
        v_flex()
            .gap_1()
            .child(hint("Apply to a selected node or add one", c))
            .children(rows)
            .children(empty)
    }

    fn render_nodes(&self, c: Colors, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let mut rows = Vec::new();
        for (index, ty) in NodeType::ALL.into_iter().enumerate() {
            if !self.matches("nodes", ty.label()) {
                continue;
            }
            rows.push(
                row(("node-type", index), c)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        if this.children.is_empty() {
                            this.graph
                                .update(cx, |graph, cx| graph.add_data(ty.default_data(), cx));
                        }
                    }))
                    .child(swatch(type_color(ty)))
                    .child(ellipsis(ty.label()))
                    .child(div().text_xs().text_color(c.muted).child("+")),
            );
        }
        v_flex().gap_1().children(rows)
    }

    fn refresh_button(&self, id: &'static str, cx: &mut Context<Self>) -> AnyElement {
        ghost_button(id, "↻")
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                cx.stop_propagation();
                this.files = doc::list();
                this.refresh_kinds(window, cx);
                this.refresh_library();
                this.ok("Library refreshed".into());
                cx.notify();
            }))
            .into_any_element()
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let bg = theme.sidebar;
        let canvas_bg = theme.background;
        let danger = theme.danger;
        let c = Colors {
            fg: theme.sidebar_foreground,
            muted: theme.muted_foreground,
            accent: theme.sidebar_accent,
            accent_fg: theme.sidebar_accent_foreground,
            border: theme.sidebar_border,
        };
        let preview = !self.children.is_empty();
        let active_graph = self.active_graph();
        let active_inspector = self.active_inspector();
        let (node_count, edge_count) = {
            let graph = active_graph.read(cx);
            (graph.node_count(), graph.edge_count())
        };
        let running = self.inspector.read(cx).running();
        let needs_review = self.inspector.read(cx).needs_review();
        let header = h_flex()
            .flex_shrink_0()
            .items_center()
            .gap_3()
            .px_4()
            .py_3()
            .bg(canvas_bg)
            .border_b_1()
            .border_color(c.border)
            .child(div().font_weight(FontWeight::BOLD).text_sm().child("uapz"))
            .child(div().w(px(1.0)).h(px(24.0)).bg(c.border))
            .child(
                Input::new(&self.name)
                    .small()
                    .w(px(260.0))
                    .disabled(preview),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(c.muted)
                    .child(if self.current.is_some() {
                        "Workflow"
                    } else {
                        "Unsaved workflow"
                    }),
            )
            .child(div().flex_1())
            .when(preview, |header| {
                header.child(
                    Button::new("return-root")
                        .outline()
                        .small()
                        .label(if needs_review {
                            "Review changes"
                        } else if running
                            || self.children.last().is_some_and(|child| child.snapshot)
                        {
                            "Return to run"
                        } else {
                            "Return to workflow"
                        })
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.return_to(0, window, cx)
                        })),
                )
            })
            .child(Button::new("new").ghost().small().label("New").on_click(
                cx.listener(|this, _: &ClickEvent, window, cx| this.new_graph(window, cx)),
            ))
            .child(
                Button::new("save")
                    .outline()
                    .small()
                    .label("Save")
                    .disabled(preview)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.save_now(cx))),
            )
            .child(
                Button::new("run-all")
                    .primary()
                    .small()
                    .label(if running {
                        "Running…"
                    } else {
                        "Run workflow"
                    })
                    .disabled(preview || running || self.graph.read(cx).node_count() == 0)
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.on_run_all(&RunAll, window, cx)
                    })),
            );

        let mut breadcrumbs = h_flex()
            .id("breadcrumbs")
            .flex_1()
            .min_w_0()
            .overflow_x_scroll()
            .gap_1()
            .items_center()
            .child(
                Button::new("breadcrumb-root")
                    .ghost()
                    .small()
                    .label(self.name.read(cx).value().to_string())
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.return_to(0, window, cx)
                    })),
            );
        for (index, child) in self.children.iter().enumerate() {
            breadcrumbs = breadcrumbs
                .child(div().text_color(c.muted).text_xs().child("/"))
                .child(
                    Button::new(("breadcrumb", index))
                        .ghost()
                        .small()
                        .label(child.label.clone())
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.return_to(index + 1, window, cx)
                        })),
                );
        }
        let provenance = self.children.last().map(|child| {
            div()
                .text_xs()
                .text_color(c.muted)
                .child(if child.snapshot {
                    "Run snapshot · Read only"
                } else {
                    "Saved workflow · Read only"
                })
        });
        let toolbar = v_flex()
            .flex_shrink_0()
            .gap_2()
            .px_3()
            .py_2()
            .bg(canvas_bg)
            .border_b_1()
            .border_color(c.border)
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(breadcrumbs)
                    .child(div().flex_1())
                    .children(provenance),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .when(!preview, |bar| {
                        bar.child(
                            Select::new(&self.kind_select)
                                .small()
                                .w(px(160.0))
                                .menu_width(px(220.0)),
                        )
                        .child(
                            Button::new("add-node")
                                .outline()
                                .small()
                                .label("Add node")
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    if this.children.is_empty() {
                                        this.graph.update(cx, |graph, cx| graph.add_current(cx));
                                    }
                                })),
                        )
                    })
                    .child(
                        Button::new("frame-all")
                            .ghost()
                            .small()
                            .label("Center graph")
                            .disabled(node_count == 0)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.active_graph()
                                    .update(cx, |graph, cx| graph.frame_all(cx))
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_xs()
                            .text_color(c.muted)
                            .child(format!("{node_count} nodes · {edge_count} connections")),
                    ),
            );

        let workflow_count = self
            .files
            .iter()
            .filter(|name| self.matches("workflows", name))
            .count();
        let node_type_count = NodeType::ALL
            .iter()
            .filter(|ty| self.matches("nodes", ty.label()))
            .count();
        let kind_count = self
            .kinds
            .iter()
            .filter(|kind| self.matches("custom nodes kinds", &kind.name))
            .count();
        let agent_count = self
            .agents
            .iter()
            .filter(|agent| {
                self.matches(
                    "agents",
                    &format!("{} {}", agent.name, agent.config.summary()),
                )
            })
            .count();
        let prompt_count = self
            .prompts
            .iter()
            .filter(|name| self.matches("prompts", name))
            .count();
        let database_count = self
            .databases
            .iter()
            .filter(|db| {
                self.matches(
                    "databases resources",
                    &format!("{} {}", db.name, db.config.summary()),
                )
            })
            .count();
        let script_count = self
            .scripts
            .iter()
            .filter(|name| self.matches("scripts resources", name))
            .count();
        let data_count = self
            .data
            .iter()
            .filter(|name| self.matches("data resources", name))
            .count();
        let match_count = workflow_count
            + node_type_count
            + kind_count
            + agent_count
            + prompt_count
            + database_count
            + script_count
            + data_count;
        let graphs = self.render_graphs(c, cx).into_any_element();
        let nodes = self.render_nodes(c, cx).into_any_element();
        let kinds = self.render_kinds(c, cx).into_any_element();
        let agents = self.render_agents(c, cx).into_any_element();
        let prompts = self.render_prompts(c, cx).into_any_element();
        let databases = self.render_databases(c, cx).into_any_element();
        let scripts = self
            .render_files(NodeType::Script, &self.scripts, library::SCRIPTS_DIR, c, cx)
            .into_any_element();
        let data = self
            .render_files(NodeType::Data, &self.data, library::DATA_DIR, c, cx)
            .into_any_element();
        let mut sections = v_flex().gap_4().when(match_count == 0, |sections| {
            sections.child(hint("No library items match your search.", c))
        });
        for (key, title, count, body) in [
            (GRAPHS, "Workflows", workflow_count, graphs),
            (NODES, "Nodes", node_type_count, nodes),
            (KINDS, "Custom nodes", kind_count, kinds),
            (AGENTS, "Agents", agent_count, agents),
            (PROMPTS, "Prompts", prompt_count, prompts),
            (DATABASES, "Databases", database_count, databases),
            (SCRIPTS, "Scripts", script_count, scripts),
            (DATA, "Data", data_count, data),
        ] {
            if self.query.is_empty() || count > 0 {
                sections = sections.child(self.section(key, title, count, c, body, cx));
            }
        }
        let sidebar = v_flex()
            .size_full()
            .min_w_0()
            .bg(bg)
            .border_r_1()
            .border_color(c.border)
            .child(
                v_flex()
                    .flex_shrink_0()
                    .gap_3()
                    .p_3()
                    .child(
                        h_flex()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Library"),
                            )
                            .child(self.refresh_button("refresh-library", cx)),
                    )
                    .child(Input::new(&self.search).small())
                    .when(preview, |header| {
                        header.child(hint("Return to the root workflow to add nodes.", c))
                    }),
            )
            .child(
                div()
                    .id("sidebar")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_3()
                    .pb_4()
                    .child(sections),
            );
        let center = v_flex()
            .size_full()
            .min_w_0()
            .child(toolbar)
            .child(div().flex_1().min_h_0().w_full().child(active_graph));
        let status_text = self.status.as_ref().map_or_else(
            || {
                if needs_review {
                    "Changes are ready for review".to_string()
                } else if running {
                    "Workflow is running".to_string()
                } else if preview {
                    "Read-only workflow preview".to_string()
                } else {
                    "Ready".to_string()
                }
            },
            |status| status.text.clone(),
        );
        let status_error = self.status.as_ref().is_some_and(|status| status.error);
        let footer = h_flex()
            .flex_shrink_0()
            .items_center()
            .gap_3()
            .px_4()
            .py_2()
            .bg(canvas_bg)
            .border_t_1()
            .border_color(c.border)
            .text_xs()
            .text_color(c.muted)
            .child(ellipsis(status_text).text_color(if status_error { danger } else { c.muted }))
            .when(running, |footer| footer.child(div().child("● Running")))
            .when(needs_review && preview, |footer| {
                footer.child(
                    Button::new("footer-review")
                        .ghost()
                        .xsmall()
                        .label("Review changes")
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.return_to(0, window, cx)
                        })),
                )
            })
            .child(div().child(if preview {
                "Drag space to pan · Select to inspect"
            } else {
                "⌘S Save · ⌘↵ Run · Drag space to pan"
            }));

        v_flex()
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(Self::on_save))
            .on_action(cx.listener(Self::on_new))
            .on_action(cx.listener(Self::on_run_all))
            .size_full()
            .bg(canvas_bg)
            .text_color(c.fg)
            .child(header)
            .child(
                div().flex_1().min_h_0().child(
                    h_resizable("layout")
                        .child(
                            resizable_panel()
                                .size(px(SIDEBAR_W))
                                .size_range(px(PANEL_MIN_W)..px(PANEL_MAX_W))
                                .flex_none()
                                .child(sidebar),
                        )
                        .child(resizable_panel().child(center))
                        .child(
                            resizable_panel()
                                .size(px(INSPECTOR_W))
                                .size_range(px(PANEL_MIN_W)..px(PANEL_MAX_W))
                                .flex_none()
                                .child(active_inspector),
                        ),
                ),
            )
            .child(footer)
    }
}

#[cfg(test)]
mod tests {
    // `use super::*` would also import gpui's `test` attribute macro, which shadows the
    // standard one, so the module names what it needs.
    use super::{current_of, file_data, library_matches, preview_node, select_items};
    use crate::doc::{GraphDoc, NodeDoc, NodeKind};
    use crate::graph::Current;
    use crate::node_data::{DataFormat, NodeData, NodeType, Runtime};

    #[test]
    fn child_preview_selects_run_target_then_published_output_then_typed_node() {
        let node = |id: &str| NodeDoc {
            id: id.into(),
            x: 0.0,
            y: 0.0,
            kind: None,
            name: None,
            color: None,
            data: Some(NodeType::Script.default_data()),
        };
        let mut doc = GraphDoc {
            nodes: vec![node("n1"), node("n7")],
            ..GraphDoc::default()
        };
        doc.outputs.insert("result".into(), "n7".into());
        assert_eq!(preview_node(&doc, &["n1".into()]), Some(1));
        assert_eq!(preview_node(&doc, &["n99".into()]), Some(7));
        assert_eq!(preview_node(&doc, &[]), Some(7));
        doc.outputs.insert("result".into(), "n7.summary".into());
        assert_eq!(preview_node(&doc, &[]), Some(7));
        doc.outputs.clear();
        assert_eq!(preview_node(&doc, &[]), Some(1));
        assert_eq!(preview_node(&GraphDoc::default(), &[]), None);
    }

    #[test]
    fn library_search_finds_items_categories_and_resource_paths() {
        assert!(library_matches("", "agents", "Research"));
        assert!(library_matches("research", "agents", "Research"));
        assert!(library_matches("agents", "agents", "Research"));
        assert!(library_matches(
            "resources",
            "scripts resources",
            "etl/load.py"
        ));
        assert!(library_matches("etl/", "scripts resources", "etl/load.py"));
        assert!(!library_matches("missing", "workflows", "Research"));
        assert!(!library_matches("data", "scripts resources", "etl/load.py"));
    }

    #[test]
    fn dropdown_entries_cover_blank_the_types_and_saved_kinds() {
        let kinds = vec![NodeKind {
            name: "Note".into(),
            color: "#112233".into(),
        }];
        let items = select_items(&kinds);
        let items: Vec<&str> = items.iter().map(|s| s.as_ref()).collect();
        assert_eq!(
            items,
            [
                "Blank",
                "Run input",
                "Workflow",
                "Agent",
                "Database",
                "Database (write)",
                "Script",
                "Data",
                "Directory",
                "Write file",
                "Note"
            ]
        );
        assert_eq!(current_of("Blank"), Current::Blank);
        assert_eq!(current_of("Run input"), Current::Type(NodeType::Input));
        assert_eq!(current_of("Workflow"), Current::Type(NodeType::Workflow));
        assert_eq!(current_of("Database"), Current::Type(NodeType::Database));
        assert_eq!(current_of("Database (write)"), Current::WritableDatabase);
        assert_eq!(current_of("Directory"), Current::Type(NodeType::Directory));
        assert_eq!(current_of("Write file"), Current::Type(NodeType::File));
        assert_eq!(current_of("Note"), Current::Kind("Note".into()));
    }

    #[test]
    fn files_become_nodes_of_the_matching_runtime_or_format() {
        match file_data(NodeType::Script, "etl/run.py") {
            NodeData::Script(s) => {
                assert_eq!(s.runtime, Runtime::Python);
                assert_eq!(s.path, "etl/run.py");
            }
            other => panic!("{other:?}"),
        }
        match file_data(NodeType::Script, "tool.sh") {
            NodeData::Script(s) => assert_eq!(s.runtime, Runtime::Shell),
            other => panic!("{other:?}"),
        }
        match file_data(NodeType::Data, "sales.csv") {
            NodeData::Data(d) => {
                assert_eq!(d.format, DataFormat::Csv);
                assert_eq!(d.path, "sales.csv");
            }
            other => panic!("{other:?}"),
        }
        match file_data(NodeType::Data, "img.png") {
            NodeData::Data(d) => assert_eq!(d.format, DataFormat::Image),
            other => panic!("{other:?}"),
        }
    }
}

/// An opt-in native renderer harness, separate from parallel unit tests so AppKit
/// runs on the main thread. Fixtures stay in memory and never change the cwd.
#[cfg(all(test, feature = "ui-tests", target_os = "macos"))]
// The dedicated harness calls this module; the ordinary test target also compiles it.
#[allow(dead_code)]
pub(crate) mod rendering_tests {
    use super::{NewGraph, RunAll, Workspace};
    use crate::doc::{EdgeDoc, GraphDoc, NodeDoc, View};
    use crate::node_data::{NodeData, NodeType};
    use gpui_kit::{
        AppContext, Bounds, HeadlessAppContext, WindowBounds, WindowOptions,
        assets::Assets,
        component::{Theme, ThemeMode},
        px, size,
        test::TestWindowExt,
    };
    use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

    fn node(id: &str, name: &str, ty: NodeType, x: f32, y: f32) -> NodeDoc {
        NodeDoc {
            id: id.into(),
            x,
            y,
            kind: None,
            name: Some(name.into()),
            color: None,
            data: Some(ty.default_data()),
        }
    }

    fn wire(from: &str, to: &str) -> EdgeDoc {
        let (from, output) = from
            .split_once('.')
            .map_or((from, None), |(node, port)| (node, Some(port.to_string())));
        EdgeDoc {
            from: from.into(),
            to: to.into(),
            output,
        }
    }

    fn documents() -> (GraphDoc, GraphDoc, GraphDoc) {
        let mut root = GraphDoc {
            version: crate::doc::VERSION,
            name: "Parent draft".into(),
            view: View {
                pan_x: 32.0,
                pan_y: 24.0,
            },
            nodes: vec![
                node("n0", "render_fixture_input", NodeType::Input, 24.0, 36.0),
                node("n1", "Analyze-documents", NodeType::Workflow, 300.0, 36.0),
                node("n2", "Summarize-report", NodeType::Agent, 300.0, 330.0),
            ],
            edges: vec![wire("n0", "n1"), wire("n1.summary", "n2")],
            ..GraphDoc::default()
        };
        if let Some(NodeData::Workflow(config)) = &mut root.nodes[1].data {
            config.graph = "render_fixture_child".into();
        }
        let mut child = GraphDoc {
            version: crate::doc::VERSION,
            name: "Analysis workflow".into(),
            nodes: vec![
                node("n4", "Extract-content", NodeType::Workflow, 32.0, 64.0),
                node("n8", "Build-summary", NodeType::Agent, 320.0, 64.0),
                node("n9", "Structured-data", NodeType::Data, 320.0, 340.0),
            ],
            edges: vec![wire("n4.summary", "n8"), wire("n4.structured", "n9")],
            ..GraphDoc::default()
        };
        if let Some(NodeData::Workflow(config)) = &mut child.nodes[0].data {
            config.graph = "render_fixture_nested".into();
        }
        child.outputs.insert("summary".into(), "n8".into());
        child.outputs.insert("structured".into(), "n9".into());
        let mut nested = GraphDoc {
            version: crate::doc::VERSION,
            name: "Extraction workflow".into(),
            nodes: vec![node("n21", "Source-document", NodeType::Data, 80.0, 80.0)],
            ..GraphDoc::default()
        };
        nested.outputs.insert("summary".into(), "n21".into());
        nested.outputs.insert("structured".into(), "n21".into());
        (root, child, nested)
    }

    pub(crate) fn run() {
        let output = std::env::var_os("UAPZ_UI_SCREENSHOT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(format!("/private/tmp/uapz-ui-{}", std::process::id()))
            });
        std::fs::create_dir_all(&output).expect("create screenshot directory");
        let mut cx = HeadlessAppContext::with_platform(
            gpui_kit::platform::current_platform(true).text_system(),
            Arc::new(Assets),
            gpui_kit::platform::current_headless_renderer,
        );
        cx.update(|cx| {
            gpui_kit::init(cx);
            Theme::change(ThemeMode::Dark, None, cx);
        });
        let (handle, workspace) = cx
            .update(|cx| {
                gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: Default::default(),
                            size: size(px(1600.0), px(1000.0)),
                        })),
                        focus: false,
                        show: false,
                        ..WindowOptions::default()
                    },
                    cx,
                    |window, cx| cx.new(|cx| Workspace::new(window, cx)),
                )
            })
            .expect("open headless workspace");
        let (parent, child, nested) = documents();
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.files.clear();
                workspace.kinds.clear();
                workspace.agents.clear();
                workspace.prompts.clear();
                workspace.databases.clear();
                workspace.scripts.clear();
                workspace.data.clear();
                workspace.graph.update(cx, |graph, cx| {
                    graph.restore(&parent, cx);
                    graph.set_workflow_snapshots(
                        &BTreeMap::from([("n1".into(), child.clone())]),
                        cx,
                    );
                    graph.select_node(2, cx);
                });
                workspace
                    .name
                    .update(cx, |name, cx| name.set_value("Parent draft", window, cx));
            });
        })
        .unwrap();
        cx.update_window(handle, |_, window, cx| window.render_frame(cx))
            .unwrap();
        let root_pixels = cx
            .capture_screenshot(handle)
            .expect("Metal headless rendering must be available");
        assert!(root_pixels.width() > 0 && root_pixels.height() > 0);
        assert!(
            root_pixels
                .as_raw()
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel != &root_pixels.as_raw()[..4]),
            "frame must contain painted controls and graph cards"
        );
        root_pixels
            .save(output.join("01-root.png"))
            .expect("save root screenshot");
        let (root_graph_id, root_inspector_id, root_snapshot, root_selected) = cx.update(|cx| {
            let workspace = workspace.read(cx);
            assert_eq!(
                workspace.graph.read(cx).output_ports(1),
                ["structured", "summary"]
            );
            (
                workspace.graph.entity_id(),
                workspace.inspector.entity_id(),
                workspace.graph.read(cx).snapshot("Parent draft"),
                workspace.graph.read(cx).selected().map(|node| node.id),
            )
        });
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace
                    .inspector
                    .update(cx, |inspector, cx| inspector.run_all(false, window, cx))
            });
        })
        .unwrap();
        cx.update(|cx| {
            assert!(
                workspace.read(cx).inspector.read(cx).running(),
                "reachable Run input must keep a pending run form without execution"
            )
        });
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_child(1, &child, None, window, cx);
                workspace.active_graph().update(cx, |graph, cx| {
                    graph.set_workflow_snapshots(
                        &BTreeMap::from([("n4".into(), nested.clone())]),
                        cx,
                    );
                });
                // Root keyboard actions and library additions must never edit the parent while previewing.
                workspace.on_new(&NewGraph, window, cx);
                workspace.on_run_all(&RunAll, window, cx);
                workspace.add_preset("Unexpected edit", NodeType::Data.default_data(), cx);
                assert_eq!(workspace.children.len(), 1);
                assert_eq!(workspace.graph.entity_id(), root_graph_id);
                assert_eq!(workspace.inspector.entity_id(), root_inspector_id);
                assert!(workspace.inspector.read(cx).running());
                assert_eq!(
                    workspace.graph.read(cx).snapshot("Parent draft"),
                    root_snapshot
                );
                let preview = workspace.active_graph();
                let before = preview.read(cx).snapshot("Preview");
                preview.update(cx, |graph, cx| {
                    graph.add_data(NodeType::Agent.default_data(), cx);
                    graph.set_node_name(8, Some("Unexpected edit".into()), cx);
                    graph.clear(cx);
                });
                assert_eq!(
                    preview.read(cx).snapshot("Preview"),
                    before,
                    "child preview mutations must be rejected"
                );
                assert_eq!(preview.read(cx).selected().map(|node| node.id), Some(9));
            });
        })
        .unwrap();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("save").visible());
            assert!(window.find("run-all").visible());
            // Component buttons do not expose disabled metadata in this backend.
            // Exercise Run safely: the parent already waits for input and cannot launch.
            window.click("run-all", cx);
            assert!(window.try_find("add-node").is_none());
            assert!(window.find(("inspector-tab", 0usize)).visible());
        })
        .unwrap();
        let child_pixels = cx.capture_screenshot(handle).unwrap();
        assert_ne!(
            root_pixels.as_raw(),
            child_pixels.as_raw(),
            "child navigation must paint a different canvas and inspector"
        );
        child_pixels.save(output.join("02-child.png")).unwrap();
        let child_graph_id = cx.update(|cx| workspace.read(cx).active_graph().entity_id());
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_child(4, &nested, None, window, cx)
            });
        })
        .unwrap();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click(("breadcrumb", 0usize), cx);
        })
        .unwrap();
        cx.update(|cx| {
            let workspace = workspace.read(cx);
            assert_eq!(workspace.children.len(), 1);
            assert_eq!(
                workspace.active_graph().entity_id(),
                child_graph_id,
                "ancestor preview entity must survive deeper navigation"
            );
            assert_eq!(
                workspace
                    .active_graph()
                    .read(cx)
                    .selected()
                    .map(|node| node.id),
                Some(9)
            );
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("breadcrumb-root", cx);
        })
        .unwrap();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(
                window.find("submit-run-inputs").visible(),
                "the parent Run input form must remain intact"
            );
            assert!(window.find("save").visible());
            let workspace = workspace.read(cx);
            assert!(workspace.children.is_empty());
            assert_eq!(workspace.graph.entity_id(), root_graph_id);
            assert_eq!(workspace.inspector.entity_id(), root_inspector_id);
            assert_eq!(workspace.name.read(cx).value().as_ref(), "Parent draft");
            assert_eq!(
                workspace.graph.read(cx).snapshot("Parent draft"),
                root_snapshot,
                "parent document and viewport must survive navigation exactly"
            );
            assert_eq!(
                workspace.graph.read(cx).selected().map(|node| node.id),
                root_selected
            );
            assert!(workspace.inspector.read(cx).running());
        })
        .unwrap();
        cx.capture_screenshot(handle)
            .unwrap()
            .save(output.join("03-returned-parent.png"))
            .unwrap();
        println!(
            "workspace_rendering: passed native root/child/nested navigation, read-only, pending-run and screenshot checks; artifacts: {}",
            output.display()
        );
    }
}
