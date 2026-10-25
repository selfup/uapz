//! The node graph editor view: nodes that can be dragged around and wired together, each
//! optionally carrying typed content (an agent, database, script or data config).
//!
//! Interaction model:
//! - drag a node body to move it, drag empty space to pan the whole graph
//! - drag from a node's output port (right) and release on any other node to connect them
//! - click a node or a wire to select it, Backspace/Delete removes the selection
//! - double-click empty space (or press the button / cmd-n) to add a node
//!
//! The editor owns layout and selection only. Node content is edited in the inspector,
//! which follows [`GraphEvent::SelectionChanged`] and writes back with [`Graph::set_node_data`].

use gpui_kit::component::{ActiveTheme, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::collections::{HashMap, HashSet};

use crate::doc::{self, EdgeDoc, GraphDoc, NodeDoc, NodeKind, View};
use crate::node_data::{DatabaseConfig, NodeData, NodeType};

gpui_kit::actions!(graph, [AddNode, DeleteSelection, ClearAll, Cancel]);

pub const KEY_CONTEXT: &str = "Graph";
const NODE_W: f32 = 150.0;
const NODE_H: f32 = 72.0;
const PORT_R: f32 = 6.0;
const GRID: f32 = 24.0;
/// How close (in px) the cursor must be to a wire to pick it.
const WIRE_HIT: f32 = 6.0;

pub type NodeId = usize;

/// Emitted so the workspace and inspector can follow the editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphEvent {
    SelectionChanged,
    /// The whole graph was replaced or cleared; anything remembered per node id is stale.
    Replaced,
}

/// Where a node stands in the latest run, shown as a dot in its corner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// Part of the run, waiting for its inputs.
    Pending,
    /// Its own step is going on now.
    Running,
    /// Ran and produced output.
    Ran,
    /// Served from the cache.
    Reused,
    /// Its own step failed.
    Failed,
}

impl EventEmitter<GraphEvent> for Graph {}

/// What Add node, cmd-n and double-click create.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Current {
    #[default]
    Blank,
    Type(NodeType),
    /// A database node whose read-only flag starts off: the red one.
    WritableDatabase,
    /// A saved blank kind by name. Unknown names make blank nodes.
    Kind(String),
}

/// A read-only look at one node, for the inspector.
pub struct NodeView<'a> {
    pub id: NodeId,
    /// Name of the saved kind or preset this node was made from, if any.
    pub preset: Option<SharedString>,
    /// The node's own name, if it has one.
    pub name: Option<&'a str>,
    pub data: Option<&'a NodeData>,
}

#[derive(Clone, Debug)]
struct Node {
    id: NodeId,
    /// Top-left corner, in graph space.
    pos: Point<Pixels>,
    /// The kind or preset this node was made from, if any.
    style: Option<NodeStyle>,
    /// The node's own name, shown as its label and used by `{{name}}` in agent prompts.
    name: Option<String>,
    /// Typed content; `None` for a blank node.
    data: Option<NodeData>,
}

/// What a node shows for its kind. `color` is the value captured when the node was
/// created or loaded; the live color from the kind library wins when the kind exists, and
/// the node type's color wins over both when the node has typed content.
#[derive(Clone, Debug)]
struct NodeStyle {
    name: SharedString,
    color: Hsla,
}

/// Used for nodes whose kind is unknown and whose saved color doesn't parse.
const FALLBACK_KIND_COLOR: Hsla = Hsla {
    h: 0.0,
    s: 0.0,
    l: 0.6,
    a: 1.0,
};

impl Node {
    fn bounds(&self) -> Bounds<Pixels> {
        Bounds::new(self.pos, size(px(NODE_W), px(NODE_H)))
    }

    /// Center of the input port on the left edge.
    fn input(&self) -> Point<Pixels> {
        point(self.pos.x, self.pos.y + px(NODE_H / 2.0))
    }

    /// Center of the output port on the right edge.
    fn output(&self) -> Point<Pixels> {
        point(self.pos.x + px(NODE_W), self.pos.y + px(NODE_H / 2.0))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Edge {
    from: NodeId,
    to: NodeId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Selection {
    Node(NodeId),
    Edge(Edge),
}

#[derive(Clone, Copy, Debug)]
enum Drag {
    /// Moving a node; `grab` is the cursor offset from the node's origin.
    Node { id: NodeId, grab: Point<Pixels> },
    /// Pulling a new wire out of `from`'s output port. `target` is the node under the cursor.
    Wire {
        from: NodeId,
        cursor: Point<Pixels>,
        target: Option<NodeId>,
    },
    /// Panning the graph; `last` is the previous cursor position in window space.
    Pan { last: Point<Pixels> },
}

/// Theme colors sampled once per frame so paint closures can own them.
#[derive(Clone, Copy)]
struct Palette {
    background: Hsla,
    grid: Hsla,
    node: Hsla,
    node_border: Hsla,
    accent: Hsla,
    port: Hsla,
    port_ring: Hsla,
    wire: Hsla,
    text: Hsla,
    ran: Hsla,
    reused: Hsla,
    failed: Hsla,
}

pub struct Graph {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    selection: Option<Selection>,
    drag: Option<Drag>,
    /// Offset from graph space to canvas space.
    pan: Point<Pixels>,
    /// Where the canvas sits in the window, captured each frame during prepaint.
    canvas: Bounds<Pixels>,
    /// Custom blank node kinds by name, for resolving node colors.
    kinds: HashMap<String, Hsla>,
    /// Run outcomes per node, drawn as a dot in the node's corner.
    marks: HashMap<NodeId, Mark>,
    current: Current,
    next_id: NodeId,
    pub(crate) focus_handle: FocusHandle,
}

impl Graph {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut graph = Self {
            nodes: Vec::new(),
            edges: Vec::new(),
            selection: None,
            drag: None,
            pan: Point::default(),
            canvas: Bounds::default(),
            kinds: HashMap::new(),
            marks: HashMap::new(),
            current: Current::Blank,
            next_id: 0,
            focus_handle: cx.focus_handle(),
        };
        graph.insert_node(point(px(160.0), px(220.0)), None, None, cx);
        graph.insert_node(point(px(460.0), px(330.0)), None, None, cx);
        graph.insert_node(point(px(760.0), px(200.0)), None, None, cx);
        graph.selection = None;
        graph
    }

    /// Window coordinates to graph coordinates.
    fn to_graph(&self, window_pos: Point<Pixels>) -> Point<Pixels> {
        window_pos - self.canvas.origin - self.pan
    }

    fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    fn node_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.nodes.iter_mut().find(|n| n.id == id)
    }

    /// Change the selection, telling subscribers when it actually changed.
    fn set_selection(&mut self, selection: Option<Selection>, cx: &mut Context<Self>) {
        if self.selection != selection {
            self.selection = selection;
            cx.emit(GraphEvent::SelectionChanged);
        }
    }

    fn insert_node(
        &mut self,
        pos: Point<Pixels>,
        style: Option<NodeStyle>,
        data: Option<NodeData>,
        cx: &mut Context<Self>,
    ) -> NodeId {
        let id = next_free_id(self.next_id, |id| self.nodes.iter().any(|n| n.id == id));
        self.next_id = id.wrapping_add(1);
        self.nodes.push(Node {
            id,
            pos,
            style,
            name: None,
            data,
        });
        self.set_selection(Some(Selection::Node(id)), cx);
        id
    }

    /// Topmost node whose (slightly dilated) body contains `p`, skipping `skip`.
    fn node_at(&self, p: Point<Pixels>, skip: Option<NodeId>) -> Option<NodeId> {
        self.nodes
            .iter()
            .rev()
            .filter(|n| Some(n.id) != skip)
            .find(|n| n.bounds().dilate(px(PORT_R)).contains(&p))
            .map(|n| n.id)
    }

    fn edge_at(&self, p: Point<Pixels>) -> Option<Edge> {
        self.edges.iter().copied().find(|e| {
            let (Some(a), Some(b)) = (self.node(e.from), self.node(e.to)) else {
                return false;
            };
            wire_distance(a.output(), b.input(), p) <= WIRE_HIT
        })
    }

    fn connect(&mut self, from: NodeId, to: NodeId, cx: &mut Context<Self>) {
        if from == to {
            return;
        }
        let edge = Edge { from, to };
        if !self.edges.contains(&edge) {
            self.edges.push(edge);
        }
        self.set_selection(Some(Selection::Edge(edge)), cx);
    }

    // ----- mouse -----

    fn on_canvas_mouse_down(
        &mut self,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_handle.focus(window, cx);
        let p = self.to_graph(ev.position);
        if ev.click_count == 2 {
            let (style, data) = self.current_parts();
            self.insert_node(
                p - point(px(NODE_W / 2.0), px(NODE_H / 2.0)),
                style,
                data,
                cx,
            );
        } else if let Some(edge) = self.edge_at(p) {
            self.set_selection(Some(Selection::Edge(edge)), cx);
        } else {
            self.set_selection(None, cx);
            self.drag = Some(Drag::Pan { last: ev.position });
        }
        cx.notify();
    }

    fn on_node_mouse_down(
        &mut self,
        id: NodeId,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.focus_handle.focus(window, cx);
        let Some(node) = self.node(id) else { return };
        let grab = self.to_graph(ev.position) - node.pos;
        // Raise the node so it renders above its siblings while dragging.
        if let Some(i) = self.nodes.iter().position(|n| n.id == id) {
            let node = self.nodes.remove(i);
            self.nodes.push(node);
        }
        self.set_selection(Some(Selection::Node(id)), cx);
        self.drag = Some(Drag::Node { id, grab });
        cx.notify();
    }

    fn on_port_mouse_down(
        &mut self,
        id: NodeId,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.focus_handle.focus(window, cx);
        self.set_selection(Some(Selection::Node(id)), cx);
        self.drag = Some(Drag::Wire {
            from: id,
            cursor: self.to_graph(ev.position),
            target: None,
        });
        cx.notify();
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.drag else { return };
        if ev.pressed_button != Some(MouseButton::Left) {
            // We missed the mouse-up (e.g. it happened over another window); stop dragging.
            self.drag = None;
            cx.notify();
            return;
        }
        let p = self.to_graph(ev.position);
        match drag {
            Drag::Node { id, grab } => {
                if let Some(node) = self.node_mut(id) {
                    node.pos = p - grab;
                }
            }
            Drag::Wire { from, .. } => {
                let target = self.node_at(p, Some(from));
                self.drag = Some(Drag::Wire {
                    from,
                    cursor: p,
                    target,
                });
            }
            Drag::Pan { last } => {
                self.pan += ev.position - last;
                self.drag = Some(Drag::Pan { last: ev.position });
            }
        }
        cx.notify();
    }

    fn on_mouse_up(&mut self, ev: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.drag.take() else { return };
        if let Drag::Wire { from, .. } = drag {
            let p = self.to_graph(ev.position);
            if let Some(to) = self.node_at(p, Some(from)) {
                self.connect(from, to, cx);
            }
        }
        cx.notify();
    }

    // ----- actions -----

    fn add_node_action(&mut self, _: &AddNode, _: &mut Window, cx: &mut Context<Self>) {
        self.add_current(cx);
    }

    /// Near the visible center, cascading so repeated adds don't stack exactly.
    fn spawn_pos(&self) -> Point<Pixels> {
        let center = self.to_graph(self.canvas.center());
        let step = px((self.next_id % 8) as f32 * 24.0);
        center - point(px(NODE_W / 2.0), px(NODE_H / 2.0)) + point(step, step)
    }

    fn delete_selection(&mut self, _: &DeleteSelection, _: &mut Window, cx: &mut Context<Self>) {
        match self.selection {
            Some(Selection::Node(id)) => {
                self.nodes.retain(|n| n.id != id);
                self.edges.retain(|e| e.from != id && e.to != id);
            }
            Some(Selection::Edge(edge)) => self.edges.retain(|e| *e != edge),
            None => return,
        }
        self.set_selection(None, cx);
        cx.notify();
    }

    fn clear_all(&mut self, _: &ClearAll, _: &mut Window, cx: &mut Context<Self>) {
        self.clear(cx);
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        self.drag = None;
        self.set_selection(None, cx);
        cx.notify();
    }

    // ----- node kinds and types -----

    /// Replace the blank-kind library. Existing nodes of a kind pick up its current color.
    pub fn set_kinds(&mut self, kinds: &[NodeKind], cx: &mut Context<Self>) {
        self.kinds = kinds
            .iter()
            .filter_map(|k| Some((k.name.clone(), hsla_of(&k.color)?)))
            .collect();
        cx.notify();
    }

    /// Replace the run marks: the dots describe one run, the latest.
    pub fn set_marks(&mut self, marks: HashMap<NodeId, Mark>, cx: &mut Context<Self>) {
        self.marks = marks;
        cx.notify();
    }

    /// Choose what Add node, cmd-n and double-click create.
    pub fn set_current(&mut self, current: Current, cx: &mut Context<Self>) {
        self.current = current;
        cx.notify();
    }

    fn current_parts(&self) -> (Option<NodeStyle>, Option<NodeData>) {
        match &self.current {
            Current::Blank => (None, None),
            Current::Type(ty) => (None, Some(ty.default_data())),
            Current::WritableDatabase => {
                (None, Some(NodeData::Database(DatabaseConfig::writable())))
            }
            Current::Kind(name) => {
                let style = self.kinds.get(name).map(|color| NodeStyle {
                    name: name.clone().into(),
                    color: *color,
                });
                (style, None)
            }
        }
    }

    /// Add a node of the current kind or type near the visible center.
    pub fn add_current(&mut self, cx: &mut Context<Self>) {
        let (style, data) = self.current_parts();
        let pos = self.spawn_pos();
        self.insert_node(pos, style, data, cx);
        cx.notify();
    }

    /// Add a blank node of a custom kind near the visible center.
    pub fn add_kind_node(&mut self, kind: &NodeKind, cx: &mut Context<Self>) {
        let style = NodeStyle {
            name: kind.name.clone().into(),
            color: hsla_of(&kind.color).unwrap_or(FALLBACK_KIND_COLOR),
        };
        let pos = self.spawn_pos();
        self.insert_node(pos, Some(style), None, cx);
        cx.notify();
    }

    /// Add a node carrying `data`, labeled by its type.
    pub fn add_data(&mut self, data: NodeData, cx: &mut Context<Self>) {
        let pos = self.spawn_pos();
        self.insert_node(pos, None, Some(data), cx);
        cx.notify();
    }

    /// Add a node made from a saved preset: its content plus the preset's name as label.
    pub fn add_preset(&mut self, name: &str, data: NodeData, cx: &mut Context<Self>) {
        let style = NodeStyle {
            name: name.to_string().into(),
            color: data_color(&data),
        };
        let pos = self.spawn_pos();
        self.insert_node(pos, Some(style), Some(data), cx);
        cx.notify();
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    // ----- node content -----

    /// The selected node, if the selection is a node.
    pub fn selected(&self) -> Option<NodeView<'_>> {
        let Some(Selection::Node(id)) = self.selection else {
            return None;
        };
        let node = self.node(id)?;
        Some(NodeView {
            id,
            preset: node.style.as_ref().map(|s| s.name.clone()),
            name: node.name.as_deref(),
            data: node.data.as_ref(),
        })
    }

    pub fn node_data(&self, id: NodeId) -> Option<&NodeData> {
        self.node(id)?.data.as_ref()
    }

    pub fn node_name(&self, id: NodeId) -> Option<&str> {
        self.node(id)?.name.as_deref()
    }

    /// Name or unname a node. The name is stored as typed; the inspector reports what is
    /// wrong with it and the executor ignores a name that does not validate.
    pub fn set_node_name(&mut self, id: NodeId, name: Option<String>, cx: &mut Context<Self>) {
        if let Some(node) = self.node_mut(id) {
            node.name = name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
            cx.notify();
        }
    }

    /// The nodes wired into `id`, in id order: the order the executor sees them in.
    pub fn inputs(&self, id: NodeId) -> Vec<NodeId> {
        let mut from: Vec<NodeId> = self
            .edges
            .iter()
            .filter(|e| e.to == id)
            .map(|e| e.from)
            .collect();
        from.sort_unstable();
        from.dedup();
        from
    }

    /// Every named node, so the inspector can flag a name used twice.
    pub fn named_nodes(&self) -> Vec<(NodeId, String)> {
        self.nodes
            .iter()
            .filter_map(|n| Some((n.id, n.name.clone()?)))
            .collect()
    }

    /// Replace a node's content. `None` makes it blank again.
    pub fn set_node_data(&mut self, id: NodeId, data: Option<NodeData>, cx: &mut Context<Self>) {
        if let Some(node) = self.node_mut(id) {
            node.data = data;
            cx.notify();
        }
    }

    /// Set or clear the kind/preset name shown on a node.
    pub fn set_node_label(&mut self, id: NodeId, label: Option<String>, cx: &mut Context<Self>) {
        let Some(node) = self.node_mut(id) else {
            return;
        };
        node.style = label.map(|name| {
            let color = match &node.data {
                Some(data) => data_color(data),
                None => FALLBACK_KIND_COLOR,
            };
            NodeStyle {
                name: name.into(),
                color,
            }
        });
        cx.notify();
    }

    /// The node's style for display. The node's own name is the label when it has one,
    /// else the preset or kind name, else the content type. Typed content sets the color
    /// (its type's, or red for a database that may write). Otherwise the live kind color
    /// wins over the one captured at load time; a named blank node without a kind is grey.
    fn resolved_style(&self, node: &Node) -> Option<NodeStyle> {
        resolve_style(node, &self.kinds)
    }

    // ----- persistence -----

    /// The editor state as a document ready to be written to disk. Nodes and edges are
    /// sorted by id so the files don't churn when a node is dragged to the front.
    pub fn snapshot(&self, name: &str) -> GraphDoc {
        snapshot_doc(name, &self.nodes, &self.edges, self.pan, &self.kinds)
    }

    /// Replace the editor state with a document; see [`restore_plan`] for the rules.
    pub fn restore(&mut self, doc: &GraphDoc, cx: &mut Context<Self>) {
        let restored = restore_plan(doc, &self.kinds);
        self.nodes = restored.nodes;
        self.edges = restored.edges;
        self.next_id = restored.next_id;
        self.pan = point(px(doc.view.pan_x), px(doc.view.pan_y));
        self.selection = None;
        self.drag = None;
        self.marks.clear();
        cx.emit(GraphEvent::Replaced);
        cx.emit(GraphEvent::SelectionChanged);
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.nodes.clear();
        self.edges.clear();
        self.selection = None;
        self.drag = None;
        self.pan = Point::default();
        self.marks.clear();
        cx.emit(GraphEvent::Replaced);
        cx.emit(GraphEvent::SelectionChanged);
        cx.notify();
    }

    // ----- rendering -----

    /// The grid and all wires are painted directly, beneath the node elements.
    fn render_canvas(&self, c: Palette, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let view = cx.entity().downgrade();
        let nodes = self.nodes.clone();
        let edges = self.edges.clone();
        let selection = self.selection;
        let drag = self.drag;
        let pan = self.pan;

        canvas(
            move |bounds, _, cx| {
                let _ = view.update(cx, |graph, _| graph.canvas = bounds);
            },
            move |bounds, _, window, _| {
                paint_grid(bounds, pan, c.grid, window);

                let origin = bounds.origin + pan;
                let find = |id: NodeId| nodes.iter().find(|n| n.id == id);

                for edge in &edges {
                    let (Some(a), Some(b)) = (find(edge.from), find(edge.to)) else {
                        continue;
                    };
                    let selected = selection == Some(Selection::Edge(*edge));
                    let (color, width) = if selected {
                        (c.accent, px(3.0))
                    } else {
                        (c.wire, px(2.0))
                    };
                    if let Some(path) =
                        wire_path(a.output() + origin, b.input() + origin, width, false)
                    {
                        window.paint_path(path, color);
                    }
                }

                if let Some(Drag::Wire {
                    from,
                    cursor,
                    target,
                }) = drag
                    && let Some(a) = find(from)
                {
                    let end = target.and_then(find).map(Node::input).unwrap_or(cursor);
                    if let Some(path) =
                        wire_path(a.output() + origin, end + origin, px(2.0), target.is_none())
                    {
                        window.paint_path(path, c.accent);
                    }
                }
            },
        )
        .absolute()
        .inset_0()
        .size_full()
    }

    fn render_node(
        &self,
        node: &Node,
        c: Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let id = node.id;
        let selected = self.selection == Some(Selection::Node(id));
        let is_target = matches!(self.drag, Some(Drag::Wire { target: Some(t), .. }) if t == id);
        let is_source = matches!(self.drag, Some(Drag::Wire { from, .. }) if from == id);
        let dragging = matches!(self.drag, Some(Drag::Node { id: d, .. }) if d == id);
        let top_left = node.pos + self.pan;

        let port = |active: bool| {
            div()
                .absolute()
                .top(px(NODE_H / 2.0 - PORT_R))
                .w(px(PORT_R * 2.0))
                .h(px(PORT_R * 2.0))
                .rounded_full()
                .border_2()
                .border_color(c.port_ring)
                .bg(if active { c.accent } else { c.port })
        };

        // The body is a child rather than styling on the node itself: gpui paints an
        // element's border after its children, so a border on the node would be drawn
        // over the ports. As the first sibling, the body stays beneath them.
        let style = self.resolved_style(node);
        let summary = node.data.as_ref().map(|d| d.summary());
        let fill = style.as_ref().map_or(c.node, |s| s.color);
        let edge = style.as_ref().map_or(c.node_border, |s| darken(s.color));
        let label_color =
            style.as_ref().map_or(
                c.text,
                |s| if is_light(s.color) { black() } else { white() },
            );
        let summary_color = Hsla {
            a: 0.75,
            ..label_color
        };
        let body = div()
            .absolute()
            .inset_0()
            .rounded_lg()
            .border_1()
            .border_color(if selected || is_target {
                c.accent
            } else {
                edge
            })
            .bg(fill)
            .when(selected || dragging, |d| d.shadow_md())
            .flex()
            .items_center()
            .justify_center()
            .px_2()
            .when_some(style, |d, s| {
                d.child(
                    v_flex()
                        .min_w_0()
                        .items_center()
                        .child(
                            div()
                                .max_w_full()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(label_color)
                                .child(s.name),
                        )
                        .children(summary.map(|text| {
                            div()
                                .max_w_full()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_xs()
                                .text_color(summary_color)
                                .child(text)
                        })),
                )
            });

        // The latest run's state, as a dot in the top-right corner: hollow while waiting,
        // the accent while running, then green ran, grey reused or red failed.
        let mark = self.marks.get(&id).map(|mark| {
            let dot = div()
                .absolute()
                .top(px(5.0))
                .right(px(5.0))
                .w(px(9.0))
                .h(px(9.0))
                .rounded_full()
                .border_1();
            match mark {
                Mark::Pending => dot.border_color(c.reused).bg(c.port_ring),
                Mark::Running => dot.border_color(c.port_ring).bg(c.accent),
                Mark::Ran => dot.border_color(c.port_ring).bg(c.ran),
                Mark::Reused => dot.border_color(c.port_ring).bg(c.reused),
                Mark::Failed => dot.border_color(c.port_ring).bg(c.failed),
            }
        });

        div()
            .id(("node", id))
            .absolute()
            .left(top_left.x)
            .top(top_left.y)
            .w(px(NODE_W))
            .h(px(NODE_H))
            .when(dragging, |d| d.cursor_grabbing())
            .when(!dragging, |d| d.cursor_grab())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |graph, ev: &MouseDownEvent, window, cx| {
                    graph.on_node_mouse_down(id, ev, window, cx)
                }),
            )
            .child(body)
            .children(mark)
            // Input port: a drop target, so it only needs to look right.
            .child(port(is_target).left(px(-PORT_R)))
            // Output port: where wires start.
            .child(
                port(is_source)
                    .right(px(-PORT_R))
                    .cursor_crosshair()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |graph, ev: &MouseDownEvent, window, cx| {
                            graph.on_port_mouse_down(id, ev, window, cx)
                        }),
                    ),
            )
    }
}

impl Render for Graph {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let c = Palette {
            background: theme.background,
            grid: theme.border,
            node: theme.secondary,
            node_border: theme.border,
            accent: theme.primary,
            port: theme.muted_foreground,
            port_ring: theme.background,
            wire: theme.muted_foreground,
            text: theme.muted_foreground,
            ran: theme.success,
            reused: theme.muted_foreground,
            failed: theme.danger,
        };
        let panning = matches!(self.drag, Some(Drag::Pan { .. }));

        let mut nodes = Vec::with_capacity(self.nodes.len());
        for node in &self.nodes {
            nodes.push(self.render_node(node, c, cx));
        }

        div()
            .id("graph")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::add_node_action))
            .on_action(cx.listener(Self::delete_selection))
            .on_action(cx.listener(Self::clear_all))
            .on_action(cx.listener(Self::cancel))
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(c.background)
            .when(panning, |d| d.cursor_grabbing())
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_canvas_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .child(self.render_canvas(c, cx))
            .children(nodes)
    }
}

// ----- documents -----

/// See [`Graph::resolved_style`].
fn resolve_style(node: &Node, kinds: &HashMap<String, Hsla>) -> Option<NodeStyle> {
    let own: Option<SharedString> = node.name.clone().map(SharedString::from);
    let preset = node.style.as_ref().map(|s| s.name.clone());
    if let Some(data) = &node.data {
        return Some(NodeStyle {
            name: own.or(preset).unwrap_or_else(|| data.label().into()),
            color: data_color(data),
        });
    }
    match (own, node.style.as_ref()) {
        (name, Some(style)) => Some(NodeStyle {
            name: name.unwrap_or_else(|| style.name.clone()),
            color: kinds
                .get(style.name.as_ref())
                .copied()
                .unwrap_or(style.color),
        }),
        (Some(name), None) => Some(NodeStyle {
            name,
            color: FALLBACK_KIND_COLOR,
        }),
        (None, None) => None,
    }
}

/// The editor state as a document ready for disk: nodes and edges sorted by id so the
/// files don't churn when a node is dragged to the front, and each node's display color
/// captured so the graph renders without its kind library.
fn snapshot_doc(
    name: &str,
    nodes: &[Node],
    edges: &[Edge],
    pan: Point<Pixels>,
    kinds: &HashMap<String, Hsla>,
) -> GraphDoc {
    let mut nodes: Vec<&Node> = nodes.iter().collect();
    nodes.sort_by_key(|n| n.id);
    let mut edges = edges.to_vec();
    edges.sort_by_key(|e| (e.from, e.to));
    GraphDoc {
        version: doc::VERSION,
        name: name.to_string(),
        view: View {
            pan_x: pan.x.as_f32(),
            pan_y: pan.y.as_f32(),
        },
        nodes: nodes
            .into_iter()
            .map(|n| NodeDoc {
                id: node_key(n.id),
                x: n.pos.x.as_f32(),
                y: n.pos.y.as_f32(),
                kind: n.style.as_ref().map(|s| s.name.to_string()),
                name: n.name.clone(),
                color: resolve_style(n, kinds).map(|s| hex_of(s.color)),
                data: n.data.clone(),
            })
            .collect(),
        edges: edges
            .iter()
            .map(|e| EdgeDoc {
                from: node_key(e.from),
                to: node_key(e.to),
            })
            .collect(),
    }
}

/// What [`Graph::restore`] installs from a document.
struct Restored {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    next_id: NodeId,
}

/// Ids of the form `n<number>` are kept, the first time each appears, so the JSON stays
/// stable across a load/save cycle; anything else gets a fresh id above them, wrapping
/// to unused lower ids at the integer limit. Edges that name unknown nodes, loop on one
/// node or repeat an earlier edge are dropped.
fn restore_plan(doc: &GraphDoc, kinds: &HashMap<String, Hsla>) -> Restored {
    let mut taken: HashSet<NodeId> = HashSet::new();
    let parsed: Vec<Option<NodeId>> = doc
        .nodes
        .iter()
        .map(|n| parse_key(&n.id).filter(|id| taken.insert(*id)))
        .collect();
    let mut next = next_free_id(
        taken.iter().max().map_or(0, |max| max.wrapping_add(1)),
        |id| taken.contains(&id),
    );

    let mut ids: HashMap<&str, NodeId> = HashMap::new();
    let nodes: Vec<Node> = doc
        .nodes
        .iter()
        .zip(parsed)
        .map(|(n, parsed)| {
            let id = parsed.unwrap_or_else(|| {
                let id = next;
                taken.insert(id);
                next = next_free_id(id.wrapping_add(1), |id| taken.contains(&id));
                id
            });
            ids.insert(n.id.as_str(), id);
            Node {
                id,
                pos: point(px(n.x), px(n.y)),
                style: style_from_doc(n, kinds),
                name: n
                    .name
                    .as_deref()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_string),
                data: n.data.clone(),
            }
        })
        .collect();

    let mut edges = Vec::with_capacity(doc.edges.len());
    for e in &doc.edges {
        let (Some(&from), Some(&to)) = (ids.get(e.from.as_str()), ids.get(e.to.as_str())) else {
            continue;
        };
        let edge = Edge { from, to };
        if from != to && !edges.contains(&edge) {
            edges.push(edge);
        }
    }
    Restored {
        nodes,
        edges,
        next_id: next,
    }
}

/// A graph held in memory cannot fill the entire usize id space. After wrapping,
/// advance over occupied ids instead of saturating onto an existing node.
fn next_free_id(mut next: NodeId, is_taken: impl Fn(NodeId) -> bool) -> NodeId {
    while is_taken(next) {
        next = next.wrapping_add(1);
    }
    next
}

// ----- geometry helpers -----

fn paint_grid(bounds: Bounds<Pixels>, pan: Point<Pixels>, color: Hsla, window: &mut Window) {
    let grid = px(GRID);
    let dot = size(px(2.0), px(2.0));
    let mut x = bounds.origin.x + wrap(pan.x, grid);
    while x < bounds.right() {
        let mut y = bounds.origin.y + wrap(pan.y, grid);
        while y < bounds.bottom() {
            let dot_bounds = Bounds::new(point(x - px(1.0), y - px(1.0)), dot);
            window.paint_quad(fill(dot_bounds, color).corner_radii(Corners::all(px(1.0))));
            y += grid;
        }
        x += grid;
    }
}

/// `v` modulo `grid`, always in `0..grid` even for negative `v`. Negative zero (from
/// `-24 % 24`) is folded into zero, since `Pixels` orders it below zero.
fn wrap(v: Pixels, grid: Pixels) -> Pixels {
    let r = v.as_f32().rem_euclid(grid.as_f32());
    px(if r == 0.0 { 0.0 } else { r })
}

/// Bezier control points for a wire leaving `a` to the right and entering `b` from the left.
fn wire_controls(a: Point<Pixels>, b: Point<Pixels>) -> (Point<Pixels>, Point<Pixels>) {
    let reach = ((b.x - a.x).abs() * 0.5).max(px(40.0));
    (point(a.x + reach, a.y), point(b.x - reach, b.y))
}

fn wire_path(
    a: Point<Pixels>,
    b: Point<Pixels>,
    width: Pixels,
    dashed: bool,
) -> Option<Path<Pixels>> {
    let (c1, c2) = wire_controls(a, b);
    let mut builder = PathBuilder::stroke(width);
    if dashed {
        builder = builder.dash_array(&[px(6.0), px(4.0)]);
    }
    builder.move_to(a);
    builder.cubic_bezier_to(b, c1, c2);
    builder.build().ok()
}

/// Shortest distance from `p` to the wire between `a` and `b`, by sampling the curve.
fn wire_distance(a: Point<Pixels>, b: Point<Pixels>, p: Point<Pixels>) -> f32 {
    const STEPS: usize = 24;
    let (c1, c2) = wire_controls(a, b);
    let (a, b, c1, c2, p) = (f2(a), f2(b), f2(c1), f2(c2), f2(p));
    let bezier = |t: f32| {
        let u = 1.0 - t;
        let (w0, w1, w2, w3) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
        (
            w0 * a.0 + w1 * c1.0 + w2 * c2.0 + w3 * b.0,
            w0 * a.1 + w1 * c1.1 + w2 * c2.1 + w3 * b.1,
        )
    };
    let mut best = f32::MAX;
    let mut prev = bezier(0.0);
    for i in 1..=STEPS {
        let next = bezier(i as f32 / STEPS as f32);
        best = best.min(segment_distance(prev, next, p));
        prev = next;
    }
    best
}

fn f2(p: Point<Pixels>) -> (f32, f32) {
    (p.x.as_f32(), p.y.as_f32())
}

fn segment_distance(a: (f32, f32), b: (f32, f32), p: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 == 0.0 {
        0.0
    } else {
        (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0)
    };
    let (cx, cy) = (a.0 + t * dx, a.1 + t * dy);
    ((p.0 - cx).powi(2) + (p.1 - cy).powi(2)).sqrt()
}

fn node_key(id: NodeId) -> String {
    format!("n{id}")
}

fn parse_key(key: &str) -> Option<NodeId> {
    // Only what `node_key` writes and the mermaid parser reads back whole: `n` and
    // decimal digits. The parser takes an id as a leading run of alphanumerics and `_`,
    // so `n-1` would come back as `n` and `1` and silently become two nodes.
    let digits = key.strip_prefix('n')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn style_from_doc(n: &NodeDoc, kinds: &HashMap<String, Hsla>) -> Option<NodeStyle> {
    let name = n.kind.clone()?;
    let color = kinds
        .get(&name)
        .copied()
        .or_else(|| n.color.as_deref().and_then(hsla_of))
        .unwrap_or(FALLBACK_KIND_COLOR);
    Some(NodeStyle {
        name: name.into(),
        color,
    })
}

pub(crate) fn hsla_of(hex: &str) -> Option<Hsla> {
    let (r, g, b) = doc::hex_to_rgb(hex)?;
    Some(Hsla::from(Rgba { r, g, b, a: 1.0 }))
}

/// The fill color nodes of a type use everywhere: canvas, sidebar swatches, inspector.
pub(crate) fn type_color(ty: NodeType) -> Hsla {
    hsla_of(ty.color()).unwrap_or(FALLBACK_KIND_COLOR)
}

/// The fill color for a node with this content: see [`NodeData::color`].
pub(crate) fn data_color(data: &NodeData) -> Hsla {
    hsla_of(data.color()).unwrap_or(FALLBACK_KIND_COLOR)
}

fn hex_of(color: Hsla) -> String {
    let rgb = Rgba::from(color);
    doc::rgb_to_hex(rgb.r, rgb.g, rgb.b)
}

fn is_light(color: Hsla) -> bool {
    let rgb = Rgba::from(color);
    doc::is_light(rgb.r, rgb.g, rgb.b)
}

/// A border tone for a colored node body.
fn darken(color: Hsla) -> Hsla {
    Hsla {
        l: color.l * 0.75,
        ..color
    }
}

#[cfg(test)]
mod tests {
    // `use super::*` would also import gpui's `test` attribute macro, which shadows the
    // standard one, so the module names what it needs.
    use super::{
        Edge, FALLBACK_KIND_COLOR, Node, NodeId, NodeStyle, hex_of, hsla_of, node_key, parse_key,
        resolve_style, restore_plan, segment_distance, snapshot_doc, style_from_doc, wire_distance,
        wrap,
    };
    use crate::doc::{self, EdgeDoc, GraphDoc, NodeDoc, View};
    use crate::node_data::{
        DataConfig, DataFormat, DatabaseConfig, NodeData, NodeType, WRITE_COLOR,
    };
    use gpui_kit::{Hsla, point, px};
    use std::collections::HashMap;

    fn doc_node(id: &str, kind: Option<&str>, color: Option<&str>) -> NodeDoc {
        NodeDoc {
            id: id.into(),
            x: 10.0,
            y: 20.0,
            kind: kind.map(String::from),
            name: None,
            color: color.map(String::from),
            data: None,
        }
    }

    fn edge_doc(from: &str, to: &str) -> EdgeDoc {
        EdgeDoc {
            from: from.into(),
            to: to.into(),
        }
    }

    fn kinds(pairs: &[(&str, &str)]) -> HashMap<String, Hsla> {
        pairs
            .iter()
            .map(|(name, hex)| (name.to_string(), hsla_of(hex).unwrap()))
            .collect()
    }

    #[test]
    fn parse_key_rejects_ids_the_mermaid_writer_cannot_write() {
        // These are exactly the ids `doc.rs` would write and then read back split in two.
        for bad in ["n-1", "n 2", "n,3", "n", "x", "plain", ""] {
            assert_eq!(parse_key(bad), None, "{bad:?} must not claim an id");
        }
        assert_eq!(parse_key("n0"), Some(0));
        assert_eq!(parse_key("n7"), Some(7));
        // Every id this module writes survives its own parser.
        for id in [0usize, 1, 42, usize::MAX] {
            assert_eq!(parse_key(&node_key(id)), Some(id), "{}", node_key(id));
        }
    }

    fn graph_doc(nodes: Vec<NodeDoc>, edges: Vec<EdgeDoc>) -> GraphDoc {
        GraphDoc {
            version: doc::VERSION,
            name: "t".into(),
            view: View::default(),
            nodes,
            edges,
        }
    }

    #[test]
    fn wrap_keeps_values_inside_the_grid() {
        let grid = px(24.0);
        assert_eq!(wrap(px(0.0), grid), px(0.0));
        assert_eq!(wrap(px(30.0), grid), px(6.0));
        assert_eq!(wrap(px(-6.0), grid), px(18.0));
        assert_eq!(wrap(px(-24.0), grid), px(0.0));
        assert_eq!(wrap(px(-25.0), grid), px(23.0));
    }

    #[test]
    fn wire_distance_is_zero_on_the_curve_and_grows_away_from_it() {
        let a = point(px(0.0), px(0.0));
        let b = point(px(200.0), px(0.0));
        assert!(wire_distance(a, b, point(px(100.0), px(0.0))) < 0.01);
        assert!((wire_distance(a, b, point(px(100.0), px(30.0))) - 30.0).abs() < 0.01);
        assert!((wire_distance(a, b, point(px(-10.0), px(0.0))) - 10.0).abs() < 0.01);
        // An S-curve between (0,0) and (200,100) passes through (100,50) at its middle.
        let c = point(px(200.0), px(100.0));
        assert!(wire_distance(a, c, point(px(100.0), px(50.0))) < 1.0);
        assert!(wire_distance(a, c, point(px(100.0), px(0.0))) > 20.0);
    }

    #[test]
    fn segment_distance_handles_degenerate_segments() {
        assert_eq!(segment_distance((1.0, 1.0), (1.0, 1.0), (4.0, 5.0)), 5.0);
        assert_eq!(segment_distance((0.0, 0.0), (10.0, 0.0), (5.0, 3.0)), 3.0);
        assert_eq!(segment_distance((0.0, 0.0), (10.0, 0.0), (13.0, 4.0)), 5.0);
    }

    #[test]
    fn node_keys_round_trip() {
        assert_eq!(node_key(7), "n7");
        assert_eq!(parse_key("n7"), Some(7));
        assert_eq!(parse_key("n"), None);
        assert_eq!(parse_key("x7"), None);
        assert_eq!(parse_key("n-1"), None);
        assert_eq!(parse_key("n7a"), None);
    }

    #[test]
    fn styles_prefer_the_live_kind_then_the_saved_color_then_the_fallback() {
        let live = kinds(&[("Note", "#ff0000")]);
        let s = style_from_doc(&doc_node("n0", Some("Note"), Some("#00ff00")), &live).unwrap();
        assert_eq!(s.name.as_ref(), "Note");
        assert_eq!(hex_of(s.color), "#ff0000");
        let s = style_from_doc(&doc_node("n0", Some("Gone"), Some("#00ff00")), &live).unwrap();
        assert_eq!(hex_of(s.color), "#00ff00");
        let s = style_from_doc(&doc_node("n0", Some("Gone"), Some("nope")), &live).unwrap();
        assert_eq!(s.color, FALLBACK_KIND_COLOR);
        assert!(style_from_doc(&doc_node("n0", None, Some("#00ff00")), &live).is_none());
    }

    #[test]
    fn restore_keeps_numeric_ids_once_and_numbers_the_rest_above_them() {
        let doc = graph_doc(
            vec![
                doc_node("n5", None, None),
                doc_node("n2", None, None),
                doc_node("n5", None, None),
                doc_node("x", None, None),
            ],
            vec![],
        );
        let restored = restore_plan(&doc, &HashMap::new());
        let ids: Vec<NodeId> = restored.nodes.iter().map(|n| n.id).collect();
        assert_eq!(ids, [5, 2, 6, 7]);
        assert_eq!(restored.next_id, 8);
        assert_eq!(restored.nodes[0].pos, point(px(10.0), px(20.0)));
        assert_eq!(
            restore_plan(&graph_doc(vec![], vec![]), &HashMap::new()).next_id,
            0
        );
    }

    #[test]
    fn restore_survives_ids_the_mermaid_writer_cannot_write() {
        // The JSON sidecar accepts any string as an id, but `doc.rs` writes ids into a
        // mermaid diagram that is read back without the sidecar. Only `n<digits>`
        // survives that trip, so anything else gets a fresh id here instead of coming
        // back as a different node.
        let doc = graph_doc(
            vec![
                doc_node("n-1", None, None),
                doc_node("n 2", None, None),
                doc_node("n", None, None),
                doc_node("plain", None, None),
                doc_node("n7", None, None),
            ],
            vec![],
        );
        let restored = restore_plan(&doc, &HashMap::new());
        let ids: Vec<NodeId> = restored.nodes.iter().map(|n| n.id).collect();
        // The one id that round-trips keeps its key; the rest are numbered above it.
        assert_eq!(ids, [8, 9, 10, 11, 7]);
        assert_eq!(restored.next_id, 12);
    }

    #[test]
    fn restore_does_not_panic_on_the_largest_id() {
        // A hand-edited sidecar can name the top of the range. Adding one to it must not
        // panic or assign overlapping ids, even when lower ids are already in use.
        let top = usize::MAX;
        let doc = graph_doc(
            vec![
                doc_node(&format!("n{top}"), None, None),
                doc_node(&format!("n{}", top - 1), None, None),
                doc_node("n0", None, None),
                doc_node("x", None, None),
                doc_node("y", None, None),
            ],
            vec![],
        );
        let restored = restore_plan(&doc, &HashMap::new());
        let ids: Vec<_> = restored.nodes.iter().map(|n| n.id).collect();
        assert_eq!(ids, [top, top - 1, 0, 1, 2]);
        assert_eq!(restored.next_id, 3);
    }

    #[test]
    fn restore_drops_dangling_self_and_duplicate_edges() {
        let doc = graph_doc(
            vec![
                doc_node("n0", None, None),
                doc_node("n1", None, None),
                doc_node("n2", None, None),
            ],
            vec![
                edge_doc("n0", "n1"),
                edge_doc("n0", "n1"),
                edge_doc("n1", "n1"),
                edge_doc("n1", "ghost"),
                edge_doc("ghost", "n2"),
                edge_doc("n1", "n2"),
            ],
        );
        let restored = restore_plan(&doc, &HashMap::new());
        assert_eq!(
            restored.edges,
            [Edge { from: 0, to: 1 }, Edge { from: 1, to: 2 }]
        );
    }

    #[test]
    fn labels_prefer_the_name_then_the_preset_then_the_type() {
        let kinds = kinds(&[("Note", "#112233")]);
        let label = |node: &Node| resolve_style(node, &kinds).map(|s| s.name.to_string());
        let color = |node: &Node| resolve_style(node, &kinds).unwrap().color;
        let mut node = Node {
            id: 0,
            pos: point(px(0.0), px(0.0)),
            style: None,
            name: None,
            data: None,
        };
        assert_eq!(label(&node), None);
        // A named blank node shows its name in the fallback grey.
        node.name = Some("schema".into());
        assert_eq!(label(&node), Some("schema".into()));
        assert_eq!(color(&node), FALLBACK_KIND_COLOR);
        // With a kind, the name still wins the label and the live kind color the fill.
        node.style = Some(NodeStyle {
            name: "Note".into(),
            color: hsla_of("#999999").unwrap(),
        });
        assert_eq!(label(&node), Some("schema".into()));
        assert_eq!(color(&node), hsla_of("#112233").unwrap());
        node.name = None;
        assert_eq!(label(&node), Some("Note".into()));
        // Content keeps the preset label and colors by type; the type label is the last
        // resort; a name beats both.
        node.data = Some(NodeData::Data(DataConfig {
            path: "a.csv".into(),
            format: DataFormat::Csv,
        }));
        assert_eq!(label(&node), Some("Note".into()));
        assert_eq!(color(&node), hsla_of(NodeType::Data.color()).unwrap());
        node.style = None;
        assert_eq!(label(&node), Some("Data".into()));
        node.name = Some("rows".into());
        assert_eq!(label(&node), Some("rows".into()));
    }

    #[test]
    fn snapshots_are_sorted_and_colored_by_content_and_round_trip() {
        let kinds = kinds(&[("Note", "#112233")]);
        let style = |name: &str, hex: &str| {
            Some(NodeStyle {
                name: name.to_string().into(),
                color: hsla_of(hex).unwrap(),
            })
        };
        let nodes = vec![
            Node {
                id: 3,
                pos: point(px(1.0), px(2.0)),
                style: None,
                name: Some("sink".into()),
                data: Some(NodeData::Database(DatabaseConfig::writable())),
            },
            Node {
                id: 1,
                pos: point(px(0.0), px(0.0)),
                style: style("Note", "#999999"),
                name: None,
                data: None,
            },
            Node {
                id: 2,
                pos: point(px(5.0), px(5.0)),
                style: style("Gone", "#abcdef"),
                name: None,
                data: Some(NodeData::Data(DataConfig {
                    path: "a.csv".into(),
                    format: DataFormat::Csv,
                })),
            },
        ];
        let edges = vec![
            Edge { from: 3, to: 1 },
            Edge { from: 1, to: 2 },
            Edge { from: 1, to: 3 },
        ];
        let doc = snapshot_doc("g", &nodes, &edges, point(px(5.0), px(-6.0)), &kinds);
        assert_eq!(doc.name, "g");
        assert_eq!((doc.view.pan_x, doc.view.pan_y), (5.0, -6.0));
        let ids: Vec<&str> = doc.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, ["n1", "n2", "n3"]);
        // The live kind color wins; typed content colors by content, keeping its preset
        // name; a writable database is red.
        assert_eq!(doc.nodes[0].color.as_deref(), Some("#112233"));
        assert_eq!(doc.nodes[1].kind.as_deref(), Some("Gone"));
        assert_eq!(doc.nodes[1].color.as_deref(), Some(NodeType::Data.color()));
        assert_eq!(doc.nodes[2].kind, None);
        assert_eq!(doc.nodes[2].color.as_deref(), Some(WRITE_COLOR));
        assert_eq!(doc.nodes[2].name.as_deref(), Some("sink"));
        assert_eq!(doc.nodes[0].name, None);
        let edges: Vec<(&str, &str)> = doc
            .edges
            .iter()
            .map(|e| (e.from.as_str(), e.to.as_str()))
            .collect();
        assert_eq!(edges, [("n1", "n2"), ("n1", "n3"), ("n3", "n1")]);

        let restored = restore_plan(&doc, &kinds);
        let ids: Vec<NodeId> = restored.nodes.iter().map(|n| n.id).collect();
        assert_eq!(ids, [1, 2, 3]);
        assert_eq!(restored.edges.len(), 3);
        assert_eq!(restored.next_id, 4);
        assert_eq!(restored.nodes[2].data, nodes[0].data);
        assert_eq!(restored.nodes[2].name.as_deref(), Some("sink"));
        let again = snapshot_doc(
            "g",
            &restored.nodes,
            &restored.edges,
            point(px(5.0), px(-6.0)),
            &kinds,
        );
        assert_eq!(
            serde_json::to_value(&again).unwrap(),
            serde_json::to_value(&doc).unwrap()
        );
    }
}
