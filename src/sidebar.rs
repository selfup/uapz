//! The workspace: the sidebar of saved graphs, blank node kinds and the per-type library
//! (agents, prompts, databases, scripts, data), the toolbar, the editor, and the inspector.

use crate::doc::{self, NodeKind};
use crate::graph::{Current, Graph, data_color, hsla_of, type_color};
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
use std::collections::HashSet;

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
const KINDS: &str = "kinds";
const AGENTS: &str = "agents";
const PROMPTS: &str = "prompts";
const DATABASES: &str = "databases";
const SCRIPTS: &str = "scripts";
const DATA: &str = "data";

pub struct Workspace {
    graph: Entity<Graph>,
    inspector: Entity<Inspector>,
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
        let kind_name = cx.new(|cx| InputState::new(window, cx).placeholder("node name"));
        let kind_color =
            cx.new(|cx| ColorPickerState::new(window, cx).default_value(DEFAULT_KIND_COLOR));
        let kind_select = cx
            .new(|cx| SelectState::new(select_items(&kinds), Some(IndexPath::new(0)), window, cx));

        let subscriptions = vec![
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
            cx.subscribe_in(
                &inspector,
                window,
                |this, _, event: &InspectorEvent, _, cx| {
                    match event {
                        InspectorEvent::LibraryChanged => this.refresh_library(),
                        InspectorEvent::Status { text, error } => {
                            this.status = Some(Status {
                                text: text.clone(),
                                error: *error,
                            });
                        }
                    }
                    cx.notify();
                },
            ),
        ];

        graph.read(cx).focus_handle.clone().focus(window, cx);
        let mut workspace = Self {
            graph,
            inspector,
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
            collapsed: HashSet::new(),
            status: None,
            _subscriptions: subscriptions,
        };
        workspace.refresh_library();
        workspace
    }

    fn ok(&mut self, text: String) {
        self.status = Some(Status { text, error: false });
    }

    fn err(&mut self, text: String) {
        self.status = Some(Status { text, error: true });
    }

    // ----- graphs -----

    fn save_now(&mut self, cx: &mut Context<Self>) {
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
        self.inspector
            .update(cx, |inspector, cx| inspector.run_all(false, window, cx));
    }

    fn on_new(&mut self, _: &NewGraph, window: &mut Window, cx: &mut Context<Self>) {
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
        self.inspector
            .update(cx, |inspector, cx| inspector.apply_prompt(name, window, cx));
    }

    /// Point the selected script/data node at the file, or add a new node for it.
    fn use_file(&mut self, ty: NodeType, path: &str, window: &mut Window, cx: &mut Context<Self>) {
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
        .py_1()
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
    #[allow(clippy::too_many_arguments)]
    fn section(
        &self,
        key: &'static str,
        title: &'static str,
        count: usize,
        c: Colors,
        extra: Option<AnyElement>,
        body: AnyElement,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let open = !self.collapsed.contains(key);
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
            .child(div().text_xs().text_color(c.muted).child(count.to_string()))
            .children(extra.map(|e| div().ml_auto().child(e)));
        v_flex()
            .gap_2()
            .child(header)
            .when(open, |d| d.child(body))
            .child(div().h(px(1.0)).flex_shrink_0().bg(c.border))
    }

    fn render_graphs(&self, c: Colors, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let current = self.current.clone();
        let mut rows = Vec::with_capacity(self.files.len());
        for (i, name) in self.files.iter().enumerate() {
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
            .then(|| hint("Nothing saved yet. Name the graph and press Save.", c));
        v_flex()
            .gap_2()
            .child(Input::new(&self.name))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("save")
                            .primary()
                            .small()
                            .label("Save")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.save_now(cx))),
                    )
                    .child(Button::new("new").outline().small().label("New").on_click(
                        cx.listener(|this, _: &ClickEvent, window, cx| this.new_graph(window, cx)),
                    )),
            )
            .child(hint(format!("saved in {}/", doc::DIR), c))
            .child(v_flex().gap_1().children(rows).children(empty))
    }

    fn render_kinds(&self, c: Colors, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let mut rows = Vec::with_capacity(self.kinds.len());
        for (i, kind) in self.kinds.iter().enumerate() {
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
        let empty = self.kinds.is_empty().then(|| {
            hint(
                "Blank, colored node kinds. Name one, pick a color, press Save kind.",
                c,
            )
        });
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
            .child(hint(
                format!("in {}/ · click selects, + adds, × deletes", doc::NODES_DIR),
                c,
            ))
            .child(v_flex().gap_1().children(rows).children(empty))
    }

    fn render_agents(&self, c: Colors, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let color = type_color(NodeType::Agent);
        let mut rows = Vec::with_capacity(self.agents.len());
        for (i, agent) in self.agents.iter().enumerate() {
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
        let empty = self.agents.is_empty().then(|| {
            hint(
                format!(
                    "Configure an Agent node and press Save agent; it lands in {}/<name>/.",
                    library::AGENTS_DIR
                ),
                c,
            )
        });
        v_flex()
            .gap_1()
            .child(hint("click adds to the canvas · × deletes", c))
            .children(rows)
            .children(empty)
    }

    fn render_prompts(&self, c: Colors, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let mut rows = Vec::with_capacity(self.prompts.len());
        for (i, name) in self.prompts.iter().enumerate() {
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
        let empty = self.prompts.is_empty().then(|| {
            hint(
                format!(
                    "Write a prompt in an Agent node and press Save prompt; it lands in {}/.",
                    library::PROMPTS_DIR
                ),
                c,
            )
        });
        v_flex()
            .gap_1()
            .child(hint("click loads into the selected agent · × deletes", c))
            .children(rows)
            .children(empty)
    }

    fn render_databases(&self, c: Colors, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let mut rows = Vec::with_capacity(self.databases.len());
        for (i, db) in self.databases.iter().enumerate() {
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
        let empty = self.databases.is_empty().then(|| {
            hint(
                format!(
                    "Configure a Database node and press Save database; it lands in {}/.",
                    library::DATABASES_DIR
                ),
                c,
            )
        });
        v_flex()
            .gap_1()
            .child(hint("click adds to the canvas · × deletes", c))
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
        let empty = files.is_empty().then(|| {
            hint(
                format!("Put files in {dir}/ and press ↻; they are listed here."),
                c,
            )
        });
        v_flex()
            .gap_1()
            .child(hint(
                "click points the selected node at the file, or adds one",
                c,
            ))
            .children(rows)
            .children(empty)
    }

    fn refresh_button(&self, id: &'static str, cx: &mut Context<Self>) -> AnyElement {
        ghost_button(id, "↻")
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                cx.stop_propagation();
                this.refresh_library();
                this.ok("Re-read the library directories".into());
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

        let status = self.status.as_ref().map(|s| {
            div()
                .flex_shrink_0()
                .px_3()
                .py_2()
                .border_t_1()
                .border_color(c.border)
                .text_xs()
                .text_color(if s.error { danger } else { c.muted })
                .child(s.text.clone())
        });

        let (node_count, edge_count) = {
            let graph = self.graph.read(cx);
            (graph.node_count(), graph.edge_count())
        };
        let running = self.inspector.read(cx).running();
        let toolbar = h_flex()
            .flex_shrink_0()
            .gap_2()
            .px_3()
            .py_2()
            .bg(canvas_bg)
            .border_b_1()
            .border_color(c.border)
            .child(
                Select::new(&self.kind_select)
                    .small()
                    .w(px(160.0))
                    .menu_width(px(200.0)),
            )
            .child(
                Button::new("add-node")
                    .primary()
                    .small()
                    .label("Add node")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.graph.update(cx, |graph, cx| graph.add_current(cx))
                    })),
            )
            .child(
                Button::new("clear")
                    .outline()
                    .small()
                    .label("Clear")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.graph.update(cx, |graph, cx| graph.clear(cx))
                    })),
            )
            .child(
                Button::new("run-all")
                    .outline()
                    .small()
                    .label(if running { "Running…" } else { "Run all" })
                    .disabled(running || node_count == 0)
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.inspector
                            .update(cx, |inspector, cx| inspector.run_all(false, window, cx))
                    })),
            )
            .child(
                div()
                    .ml_2()
                    .text_xs()
                    .text_color(c.muted)
                    .child(format!("{node_count} nodes · {edge_count} connections")),
            )
            .child(div().ml_2().text_xs().text_color(c.muted).child(
                "drag ● to connect · double-click to add · ⌫ deletes · drag space to pan · select a node to edit it",
            ));

        let graphs = self.render_graphs(c, cx).into_any_element();
        let kinds = self.render_kinds(c, cx).into_any_element();
        let agents = self.render_agents(c, cx).into_any_element();
        let prompts = self.render_prompts(c, cx).into_any_element();
        let databases = self.render_databases(c, cx).into_any_element();
        let scripts = self
            .render_files(
                NodeType::Script,
                &self.scripts.clone(),
                library::SCRIPTS_DIR,
                c,
                cx,
            )
            .into_any_element();
        let data = self
            .render_files(NodeType::Data, &self.data.clone(), library::DATA_DIR, c, cx)
            .into_any_element();
        let refresh_scripts = self.refresh_button("refresh-scripts", cx);
        let refresh_data = self.refresh_button("refresh-data", cx);

        let sections = v_flex()
            .gap_3()
            .child(self.section(GRAPHS, "Graphs", self.files.len(), c, None, graphs, cx))
            .child(self.section(KINDS, "Kinds", self.kinds.len(), c, None, kinds, cx))
            .child(self.section(AGENTS, "Agents", self.agents.len(), c, None, agents, cx))
            .child(self.section(PROMPTS, "Prompts", self.prompts.len(), c, None, prompts, cx))
            .child(self.section(
                DATABASES,
                "Databases",
                self.databases.len(),
                c,
                None,
                databases,
                cx,
            ))
            .child(self.section(
                SCRIPTS,
                "Scripts",
                self.scripts.len(),
                c,
                Some(refresh_scripts),
                scripts,
                cx,
            ))
            .child(self.section(
                DATA,
                "Data",
                self.data.len(),
                c,
                Some(refresh_data),
                data,
                cx,
            ));

        let sidebar = v_flex()
            .size_full()
            .min_w_0()
            .bg(bg)
            .border_r_1()
            .border_color(c.border)
            .child(
                div()
                    .id("sidebar")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_3()
                    .child(sections),
            )
            .children(status);
        let center = v_flex()
            .size_full()
            .min_w_0()
            .child(toolbar)
            .child(div().flex_1().min_h_0().w_full().child(self.graph.clone()));

        // The three columns form a resizable group: drag a divider to widen a side panel.
        div()
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(Self::on_save))
            .on_action(cx.listener(Self::on_new))
            .on_action(cx.listener(Self::on_run_all))
            .size_full()
            .child(
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
                            .child(self.inspector.clone()),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    // `use super::*` would also import gpui's `test` attribute macro, which shadows the
    // standard one, so the module names what it needs.
    use super::{current_of, file_data, select_items};
    use crate::doc::NodeKind;
    use crate::graph::Current;
    use crate::node_data::{DataFormat, NodeData, NodeType, Runtime};

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
