//! The node graph editor view: empty nodes that can be dragged around and wired together.
//!
//! Interaction model:
//! - drag a node body to move it, drag empty space to pan the whole graph
//! - drag from a node's output port (right) and release on any other node to connect them
//! - click a node or a wire to select it, Backspace/Delete removes the selection
//! - double-click empty space (or press the button / cmd-n) to add a node

use gpui_kit::component::ActiveTheme;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

use crate::doc::{self, EdgeDoc, GraphDoc, NodeDoc, NodeKind, View};

gpui_kit::actions!(graph, [AddNode, DeleteSelection, ClearAll, Cancel]);

pub const KEY_CONTEXT: &str = "Graph";
const NODE_W: f32 = 120.0;
const NODE_H: f32 = 72.0;
const PORT_R: f32 = 6.0;
const GRID: f32 = 24.0;
/// How close (in px) the cursor must be to a wire to pick it.
const WIRE_HIT: f32 = 6.0;

type NodeId = usize;

#[derive(Clone, Debug)]
struct Node {
    id: NodeId,
    /// Top-left corner, in graph space.
    pos: Point<Pixels>,
    /// The custom kind this node was made from, if any.
    style: Option<NodeStyle>,
    /// Free-form payload, round-tripped through the JSON sidecar.
    data: Value,
}

/// What a node shows for its kind. `color` is the value captured when the node was
/// created or loaded; the live color from the kind library wins when the kind exists.
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
    /// Custom node kinds by name, for resolving node colors.
    kinds: HashMap<String, Hsla>,
    /// Kind that new nodes are made from; `None` means a blank node.
    current_kind: Option<String>,
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
            current_kind: None,
            next_id: 0,
            focus_handle: cx.focus_handle(),
        };
        graph.add_node(point(px(160.0), px(220.0)));
        graph.add_node(point(px(460.0), px(330.0)));
        graph.add_node(point(px(760.0), px(200.0)));
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

    fn add_node(&mut self, pos: Point<Pixels>) -> NodeId {
        self.insert_node(pos, None)
    }

    fn insert_node(&mut self, pos: Point<Pixels>, style: Option<NodeStyle>) -> NodeId {
        let id = self.next_id;
        self.next_id += 1;
        self.nodes.push(Node {
            id,
            pos,
            style,
            data: doc::empty_object(),
        });
        self.selection = Some(Selection::Node(id));
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

    fn connect(&mut self, from: NodeId, to: NodeId) {
        if from == to {
            return;
        }
        let edge = Edge { from, to };
        if !self.edges.contains(&edge) {
            self.edges.push(edge);
        }
        self.selection = Some(Selection::Edge(edge));
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
            let style = self.current_style();
            self.insert_node(p - point(px(NODE_W / 2.0), px(NODE_H / 2.0)), style);
        } else if let Some(edge) = self.edge_at(p) {
            self.selection = Some(Selection::Edge(edge));
        } else {
            self.selection = None;
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
        self.selection = Some(Selection::Node(id));
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
        self.selection = Some(Selection::Node(id));
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
                self.pan = self.pan + (ev.position - last);
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
                self.connect(from, to);
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
        match self.selection.take() {
            Some(Selection::Node(id)) => {
                self.nodes.retain(|n| n.id != id);
                self.edges.retain(|e| e.from != id && e.to != id);
            }
            Some(Selection::Edge(edge)) => self.edges.retain(|e| *e != edge),
            None => return,
        }
        cx.notify();
    }

    fn clear_all(&mut self, _: &ClearAll, _: &mut Window, cx: &mut Context<Self>) {
        self.clear(cx);
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        self.drag = None;
        self.selection = None;
        cx.notify();
    }

    // ----- node kinds -----

    /// Replace the kind library. Existing nodes of a kind pick up its current color.
    pub fn set_kinds(&mut self, kinds: &[NodeKind], cx: &mut Context<Self>) {
        self.kinds = kinds
            .iter()
            .filter_map(|k| Some((k.name.clone(), hsla_of(&k.color)?)))
            .collect();
        cx.notify();
    }

    /// Choose what Add node, cmd-n and double-click create. Unknown names make blank nodes.
    pub fn set_current_kind(&mut self, kind: Option<String>, cx: &mut Context<Self>) {
        self.current_kind = kind;
        cx.notify();
    }

    fn current_style(&self) -> Option<NodeStyle> {
        let name = self.current_kind.as_ref()?;
        let color = *self.kinds.get(name)?;
        Some(NodeStyle {
            name: name.clone().into(),
            color,
        })
    }

    /// Add a node of the current kind near the visible center.
    pub fn add_current(&mut self, cx: &mut Context<Self>) {
        let (pos, style) = (self.spawn_pos(), self.current_style());
        self.insert_node(pos, style);
        cx.notify();
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Add a node of a custom kind near the visible center.
    pub fn add_kind_node(&mut self, kind: &NodeKind, cx: &mut Context<Self>) {
        let style = NodeStyle {
            name: kind.name.clone().into(),
            color: hsla_of(&kind.color).unwrap_or(FALLBACK_KIND_COLOR),
        };
        let pos = self.spawn_pos();
        self.insert_node(pos, Some(style));
        cx.notify();
    }

    /// The node's style with the color taken from the live kind library when it has one.
    fn resolved_style(&self, node: &Node) -> Option<NodeStyle> {
        let style = node.style.as_ref()?;
        Some(NodeStyle {
            name: style.name.clone(),
            color: self
                .kinds
                .get(style.name.as_ref())
                .copied()
                .unwrap_or(style.color),
        })
    }

    // ----- persistence -----

    /// The editor state as a document ready to be written to disk. Nodes and edges are
    /// sorted by id so the files don't churn when a node is dragged to the front.
    pub fn snapshot(&self, name: &str) -> GraphDoc {
        let mut nodes: Vec<&Node> = self.nodes.iter().collect();
        nodes.sort_by_key(|n| n.id);
        let mut edges = self.edges.clone();
        edges.sort_by_key(|e| (e.from, e.to));
        GraphDoc {
            version: doc::VERSION,
            name: name.to_string(),
            view: View {
                pan_x: self.pan.x.as_f32(),
                pan_y: self.pan.y.as_f32(),
            },
            nodes: nodes
                .into_iter()
                .map(|n| {
                    let style = self.resolved_style(n);
                    NodeDoc {
                        id: node_key(n.id),
                        x: n.pos.x.as_f32(),
                        y: n.pos.y.as_f32(),
                        kind: style.as_ref().map(|s| s.name.to_string()),
                        color: style.as_ref().map(|s| hex_of(s.color)),
                        data: n.data.clone(),
                    }
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

    /// Replace the editor state with a document. Ids of the form `n<number>` are kept so
    /// the JSON stays stable across a load/save cycle; anything else gets a fresh id.
    pub fn restore(&mut self, doc: &GraphDoc, cx: &mut Context<Self>) {
        let mut taken: HashSet<NodeId> = HashSet::new();
        let parsed: Vec<Option<NodeId>> = doc
            .nodes
            .iter()
            .map(|n| parse_key(&n.id).filter(|id| taken.insert(*id)))
            .collect();
        let mut next = taken.iter().max().map_or(0, |max| max + 1);

        let mut ids: HashMap<&str, NodeId> = HashMap::new();
        self.nodes = doc
            .nodes
            .iter()
            .zip(parsed)
            .map(|(n, parsed)| {
                let id = parsed.unwrap_or_else(|| {
                    next += 1;
                    next - 1
                });
                ids.insert(n.id.as_str(), id);
                Node {
                    id,
                    pos: point(px(n.x), px(n.y)),
                    style: style_from_doc(n, &self.kinds),
                    data: n.data.clone(),
                }
            })
            .collect();

        let mut edges = Vec::with_capacity(doc.edges.len());
        for e in &doc.edges {
            let (Some(&from), Some(&to)) = (ids.get(e.from.as_str()), ids.get(e.to.as_str()))
            else {
                continue;
            };
            let edge = Edge { from, to };
            if from != to && !edges.contains(&edge) {
                edges.push(edge);
            }
        }
        self.edges = edges;

        self.next_id = next;
        self.pan = point(px(doc.view.pan_x), px(doc.view.pan_y));
        self.selection = None;
        self.drag = None;
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.nodes.clear();
        self.edges.clear();
        self.selection = None;
        self.drag = None;
        self.pan = Point::default();
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
        let fill = style.as_ref().map_or(c.node, |s| s.color);
        let edge = style.as_ref().map_or(c.node_border, |s| darken(s.color));
        let label_color =
            style.as_ref().map_or(
                c.text,
                |s| if is_light(s.color) { black() } else { white() },
            );
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
                    div()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(label_color)
                        .child(s.name),
                )
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
            y = y + grid;
        }
        x = x + grid;
    }
}

/// `v` modulo `grid`, always in `0..grid` even for negative `v`.
fn wrap(v: Pixels, grid: Pixels) -> Pixels {
    let r = v % grid;
    if r < px(0.0) { r + grid } else { r }
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
    key.strip_prefix('n')?.parse().ok()
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
