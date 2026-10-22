//! The workspace: the sidebar of saved graphs and custom node kinds, the toolbar, and
//! the editor itself.

use crate::doc::{self, NodeKind};
use crate::graph::{Graph, hsla_of};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::color_picker::{ColorPicker, ColorPickerState};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{ActiveTheme, IndexPath, Sizable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

gpui_kit::actions!(workspace, [Save, NewGraph]);

pub const KEY_CONTEXT: &str = "Workspace";
const SIDEBAR_W: f32 = 240.0;
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

pub struct Workspace {
    graph: Entity<Graph>,
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
    status: Option<Status>,
    _subscriptions: Vec<Subscription>,
}

struct Status {
    text: String,
    error: bool,
}

/// Dropdown entries: Blank first, then every kind in list order.
fn select_items(kinds: &[NodeKind]) -> Vec<SharedString> {
    std::iter::once(SharedString::from(BLANK))
        .chain(kinds.iter().map(|k| SharedString::from(k.name.clone())))
        .collect()
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let kinds = doc::list_kinds();
        let graph = cx.new(|cx| {
            let mut graph = Graph::new(cx);
            graph.set_kinds(&kinds, cx);
            graph
        });
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
                        let kind = value
                            .as_ref()
                            .filter(|v| v.as_ref() != BLANK)
                            .map(|v| v.to_string());
                        this.graph
                            .update(cx, |graph, cx| graph.set_current_kind(kind, cx));
                    }
                },
            ),
        ];

        graph.read(cx).focus_handle.clone().focus(window, cx);
        Self {
            graph,
            name,
            files: doc::list(),
            current: None,
            kind_name,
            kind_color,
            kind_select,
            kinds,
            status: None,
            _subscriptions: subscriptions,
        }
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
                self.current = Some(name);
                self.ok(format!("Saved {} and {}", md.display(), json.display()));
            }
            Err(text) => self.err(text),
        }
        self.files = doc::list();
        cx.notify();
    }

    fn load(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        match doc::load(name) {
            Ok(doc) => {
                let (nodes, edges) = (doc.nodes.len(), doc.edges.len());
                self.graph.update(cx, |graph, cx| graph.restore(&doc, cx));
                self.name.update(cx, |input, cx| {
                    input.set_value(name.to_string(), window, cx)
                });
                self.current = Some(name.to_string());
                self.ok(format!("Loaded {name}: {nodes} nodes, {edges} connections"));
            }
            Err(text) => self.err(text),
        }
        self.files = doc::list();
        cx.notify();
    }

    fn new_graph(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.graph.update(cx, |graph, cx| graph.clear(cx));
        self.name
            .update(cx, |input, cx| input.set_value("untitled", window, cx));
        self.current = None;
        self.status = None;
        cx.notify();
    }

    fn on_save(&mut self, _: &Save, _: &mut Window, cx: &mut Context<Self>) {
        self.save_now(cx);
    }

    fn on_new(&mut self, _: &NewGraph, window: &mut Window, cx: &mut Context<Self>) {
        self.new_graph(window, cx);
    }

    // ----- node kinds -----

    fn save_kind_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = doc::sanitize_name(&self.kind_name.read(cx).value());
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
        self.kind_select.update(cx, |select, cx| {
            select.set_selected_index(Some(IndexPath::new(index + 1)), window, cx)
        });
        self.graph.update(cx, |graph, cx| {
            graph.set_current_kind(Some(kind.name.clone()), cx)
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
    /// and rebuild the dropdown, keeping its selection when that kind still exists.
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
        self.kind_select.update(cx, |select, cx| {
            select.set_items(items, window, cx);
            select.set_selected_index(Some(IndexPath::new(index)), window, cx);
        });
        let current = (index > 0).then(|| self.kinds[index - 1].name.clone());
        self.graph
            .update(cx, |graph, cx| graph.set_current_kind(current, cx));
        cx.notify();
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let bg = theme.sidebar;
        let canvas_bg = theme.background;
        let border = theme.sidebar_border;
        let fg = theme.sidebar_foreground;
        let muted = theme.muted_foreground;
        let accent = theme.sidebar_accent;
        let accent_fg = theme.sidebar_accent_foreground;
        let danger = theme.danger;

        let section_title = |text: &'static str| {
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(fg)
                .child(text)
        };

        // Saved graphs.
        let current = self.current.clone();
        let mut file_rows = Vec::with_capacity(self.files.len());
        for (i, name) in self.files.iter().enumerate() {
            let active = current.as_deref() == Some(name.as_str());
            let load_name = name.clone();
            file_rows.push(
                div()
                    .id(("file", i))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .text_sm()
                    .text_color(fg)
                    .cursor_pointer()
                    .when(active, |d| d.bg(accent).text_color(accent_fg))
                    .hover(|d| d.bg(accent))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.load(&load_name, window, cx)
                    }))
                    .child(name.clone()),
            );
        }
        let no_files = self.files.is_empty().then(|| {
            div()
                .px_2()
                .text_xs()
                .text_color(muted)
                .child("Nothing saved yet. Name the graph and press Save.")
        });

        // Custom node kinds: click a row to make it current, + adds one, × deletes it.
        let mut kind_rows = Vec::with_capacity(self.kinds.len());
        for (i, kind) in self.kinds.iter().enumerate() {
            let swatch = hsla_of(&kind.color).unwrap_or(muted);
            let add_kind = kind.clone();
            let delete_name = kind.name.clone();
            kind_rows.push(
                h_flex()
                    .id(("kind", i))
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .text_sm()
                    .text_color(fg)
                    .cursor_pointer()
                    .hover(|d| d.bg(accent))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.select_kind(i, window, cx)
                    }))
                    .child(
                        div()
                            .flex_shrink_0()
                            .w(px(12.0))
                            .h(px(12.0))
                            .rounded_sm()
                            .bg(swatch),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(kind.name.clone()),
                    )
                    .child(
                        Button::new(("add-kind", i))
                            .ghost()
                            .xsmall()
                            .label("+")
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                this.add_kind(&add_kind, cx)
                            })),
                    )
                    .child(
                        Button::new(("delete-kind", i))
                            .ghost()
                            .xsmall()
                            .label("×")
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                cx.stop_propagation();
                                this.delete_kind(&delete_name, window, cx)
                            })),
                    ),
            );
        }
        let no_kinds = self.kinds.is_empty().then(|| {
            div()
                .px_2()
                .text_xs()
                .text_color(muted)
                .child("No custom nodes yet. Name one, pick a color, press Save node.")
        });

        let status = self.status.as_ref().map(|s| {
            div()
                .text_xs()
                .text_color(if s.error { danger } else { muted })
                .child(s.text.clone())
        });

        let (node_count, edge_count) = {
            let graph = self.graph.read(cx);
            (graph.node_count(), graph.edge_count())
        };
        let toolbar =
            h_flex()
                .flex_shrink_0()
                .gap_2()
                .px_3()
                .py_2()
                .bg(canvas_bg)
                .border_b_1()
                .border_color(border)
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
                    div()
                        .ml_2()
                        .text_xs()
                        .text_color(muted)
                        .child(format!("{node_count} nodes · {edge_count} connections")),
                )
                .child(div().ml_2().text_xs().text_color(muted).child(
                    "drag ● to connect · double-click to add · ⌫ deletes · drag space to pan",
                ));

        div()
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(Self::on_save))
            .on_action(cx.listener(Self::on_new))
            .size_full()
            .flex()
            .flex_row()
            .child(
                v_flex()
                    .w(px(SIDEBAR_W))
                    .h_full()
                    .flex_shrink_0()
                    .bg(bg)
                    .border_r_1()
                    .border_color(border)
                    .p_3()
                    .gap_3()
                    .child(section_title("Graphs"))
                    .child(Input::new(&self.name))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("save")
                                    .primary()
                                    .small()
                                    .label("Save")
                                    .on_click(
                                        cx.listener(|this, _: &ClickEvent, _, cx| {
                                            this.save_now(cx)
                                        }),
                                    ),
                            )
                            .child(Button::new("new").outline().small().label("New").on_click(
                                cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.new_graph(window, cx)
                                }),
                            )),
                    )
                    .child(div().text_xs().text_color(muted).child(format!(
                        "{} saved in {}/",
                        self.files.len(),
                        doc::DIR
                    )))
                    .child(
                        div()
                            .id("files")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .child(v_flex().gap_1().children(file_rows).children(no_files)),
                    )
                    .child(div().h(px(1.0)).flex_shrink_0().bg(border))
                    .child(section_title("Nodes"))
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
                                    .label("Save node")
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.save_kind_now(window, cx)
                                    })),
                            ),
                    )
                    .child(div().text_xs().text_color(muted).child(format!(
                        "{} in {}/ · click to select, + adds, × deletes",
                        self.kinds.len(),
                        doc::NODES_DIR
                    )))
                    .child(
                        div()
                            .id("kinds")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .child(v_flex().gap_1().children(kind_rows).children(no_kinds)),
                    )
                    .children(status),
            )
            .child(
                v_flex()
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .child(toolbar)
                    .child(div().flex_1().min_h_0().w_full().child(self.graph.clone())),
            )
    }
}
