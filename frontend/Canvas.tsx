import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type DragEvent,
  type KeyboardEvent,
  type MouseEvent as ReactMouseEvent,
} from "react";
import {
  ReactFlow,
  ReactFlowProvider,
  Background,
  BackgroundVariant,
  Controls,
  Handle,
  Position,
  applyNodeChanges,
  useReactFlow,
  useUpdateNodeInternals,
  type Connection,
  type Edge,
  type Node,
  type NodeChange,
  type NodeProps,
  type Viewport,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import "./canvas.css";
import type { GraphDoc, NodeDoc } from "./types";
import {
  connectNodes,
  deleteNodes,
  isNamedWorkflow,
  nodeColor,
  nodeLabel,
  nodeSummary,
  typedData,
  visibleEdgeHandle,
  workflowPorts,
  type Point,
  type Port,
  type WorkflowGraphs,
} from "./graph";

/** The drag payload type library rows put on the clipboard for canvas drops. */
export const LIBRARY_DRAG_TYPE = "application/x-uapz-library";

export interface CanvasProps {
  graph: GraphDoc;
  selected: string | null;
  /** Every selected node id. The primary `selected` node is always shown selected. */
  selection?: string[];
  onSelect: (id: string | null, extend?: boolean) => void;
  onSelectionChange?: (ids: string[]) => void;
  onChange: (doc: GraphDoc) => void;
  onOpenWorkflow: (id: string) => void;
  onAdd: (position: Point) => void;
  onDropLibrary?: (payload: string, position: Point) => void;
  onRunNode?: (id: string, fresh: boolean) => void;
  onDuplicate?: (ids: string[]) => void;
  onCopy?: (ids: string[]) => void;
  onPaste?: (position: Point) => void;
  canPaste?: boolean;
  readOnly: boolean;
  workflowGraphs: WorkflowGraphs;
  marks: Record<string, string>;
  /** The first validation problem per node id, shown on the card. */
  problems?: Record<string, string>;
  frameSignal: number;
  /** Pan and zoom to a node; each new sequence number is one request. */
  focus?: { id: string; seq: number } | null;
  /** View-only navigation state may be retained for read-only child frames. */
  onViewportChange?: (view: GraphDoc["view"]) => void;
}

type CardData = {
  node: NodeDoc;
  ports: Port[];
  named: boolean;
  readOnly: boolean;
  legacyDefault: boolean;
  exports: Record<string, string>;
  mark?: string;
  problem?: string;
};
type CardNode = Node<CardData, "card">;
type MenuTarget =
  | { kind: "node"; id: string }
  | { kind: "edge"; id: string }
  | { kind: "pane"; position: Point };
type MenuItem = {
  label: string;
  keys?: string;
  danger?: boolean;
  disabled?: boolean;
  run: () => void;
};
const MENU_WIDTH = 200;
const portHandle = (name: string) => `port:${encodeURIComponent(name)}`;

function Card({ id, data, selected }: NodeProps<CardNode>) {
  const updateInternals = useUpdateNodeInternals();
  const portSignature = JSON.stringify([
    data.ports,
    data.named,
    data.legacyDefault,
  ]);
  useEffect(() => {
    updateInternals(id);
  }, [id, portSignature, updateInternals]);
  const config = typedData(data.node);
  const writes =
    config?.type === "file" ||
    (config?.type === "database" && !config.read_only);
  const aliases = (port?: string) =>
    Object.entries(data.exports)
      .filter(
        ([, value]) => value === `${id}${port === undefined ? "" : `.${port}`}`,
      )
      .map(([name]) => name)
      .join(", ");
  return (
    <article
      className={`graph-card${selected ? " is-selected" : ""}${writes ? " writes" : ""}`}
      style={{ "--node-accent": nodeColor(data.node) } as CSSProperties}
    >
      <Handle
        type="target"
        position={Position.Left}
        id="input"
        className="card-port"
        style={{ top: 52 }}
        isConnectable={!data.readOnly}
      />
      <div className="graph-card-kind">
        <span>{config?.type || data.node.kind || "blank"}</span>
        <span className="graph-card-id">{id}</span>
        {data.problem && (
          <span
            className="card-problem"
            role="img"
            aria-label={`Problem: ${data.problem}`}
            title={data.problem}
          >
            !
          </span>
        )}
        {data.mark && (
          <span
            className={`run-dot ${data.mark.toLowerCase()}`}
            title={data.mark}
            aria-label={data.mark}
          />
        )}
      </div>
      <strong className="graph-card-title" title={nodeLabel(data.node)}>
        {nodeLabel(data.node)}
      </strong>
      <div className="graph-card-summary" title={nodeSummary(data.node)}>
        {nodeSummary(data.node)}
      </div>
      {!data.named && (
        <>
          <Handle
            type="source"
            position={Position.Right}
            id="default"
            className="card-port"
            style={{ top: 52 }}
            isConnectable={!data.readOnly}
          />
          {aliases() && (
            <div
              className="graph-card-alias"
              title={`Published as ${aliases()}`}
            >
              ↗ {aliases()}
            </div>
          )}
        </>
      )}
      {data.named && (
        <div className="graph-card-ports">
          {data.legacyDefault && (
            <>
              <Handle
                type="source"
                position={Position.Right}
                id="legacy-default"
                className="card-port missing"
                style={{ top: 52 }}
                isConnectable={false}
              />
              <div className="graph-card-warning">Unresolved legacy output</div>
            </>
          )}
          {data.ports.length === 0 && (
            <div className="graph-port-empty">No published outputs</div>
          )}
          {data.ports.map((port) => (
            <div
              className={`graph-port-row${port.missing ? " is-missing" : ""}`}
              key={port.id}
            >
              <span
                title={
                  port.missing
                    ? "This output is no longer published by the child graph. Existing wires are retained."
                    : port.id
                }
              >
                {port.id}
                {port.missing ? " · missing" : ""}
              </span>
              {aliases(port.id) && (
                <span
                  className="graph-port-alias"
                  title={`Published as ${aliases(port.id)}`}
                >
                  ↗ {aliases(port.id)}
                </span>
              )}
              <Handle
                type="source"
                position={Position.Right}
                id={portHandle(port.id)}
                className={`card-port${port.missing ? " missing" : ""}`}
                style={{ top: "50%" }}
                isConnectable={!data.readOnly && !port.missing}
              />
            </div>
          ))}
        </div>
      )}
    </article>
  );
}

const nodeTypes = { card: Card };
const edgeId = (edge: GraphDoc["edges"][number], index: number) =>
  JSON.stringify([edge.from, edge.to, edge.output ?? null, index]);
const viewportOf = (graph: GraphDoc): Viewport => ({
  x: graph.view.pan_x,
  y: graph.view.pan_y,
  zoom: graph.view.zoom ?? 1,
});
const sameIds = (left: string[], right: string[]) =>
  left.length === right.length &&
  left.every((id, index) => id === right[index]);

function CanvasInner(props: CanvasProps) {
  const { graph, selected, readOnly, workflowGraphs, marks, problems } = props;
  const flow = useReactFlow<CardNode>();
  const wrapper = useRef<HTMLDivElement>(null);
  const menuElement = useRef<HTMLDivElement>(null);
  const latest = useRef(props);
  latest.current = props;
  const [selectedEdge, setSelectedEdge] = useState<string | null>(null);
  const [menu, setMenu] = useState<{
    x: number;
    y: number;
    target: MenuTarget;
  } | null>(null);
  const [viewport, setViewport] = useState<Viewport>(() => viewportOf(graph));
  const previousFrameSignal = useRef(props.frameSignal);
  const selectionKey = JSON.stringify([props.selection || [], selected]);
  const selectionSet = useMemo(
    () =>
      new Set([...(props.selection || []), ...(selected ? [selected] : [])]),
    [selectionKey],
  );
  const portsById = useMemo(
    () =>
      new Map(
        graph.nodes.map((node) => [
          node.id,
          workflowPorts(graph, node, workflowGraphs),
        ]),
      ),
    [graph, workflowGraphs],
  );
  const cards = useMemo<CardNode[]>(
    () =>
      graph.nodes.map((node) => {
        const ports = portsById.get(node.id) || [];
        const named = isNamedWorkflow(node);
        return {
          id: node.id,
          type: "card",
          // React Flow adds nopan automatically only to draggable nodes. Keep
          // read-only cards outside the pane's gesture handling as well.
          className: "nopan",
          position: { x: node.x, y: node.y },
          selected: selectionSet.has(node.id),
          draggable: !readOnly,
          connectable: !readOnly,
          deletable: !readOnly,
          data: {
            node,
            ports,
            named,
            readOnly,
            exports: graph.outputs || {},
            mark: marks[node.id],
            problem: problems?.[node.id],
            legacyDefault:
              named &&
              ports.filter((port) => !port.missing).length !== 1 &&
              graph.edges.some(
                (edge) => edge.from === node.id && edge.output == null,
              ),
          },
        };
      }),
    [graph, selectionSet, portsById, readOnly, marks, problems],
  );
  const [nodes, setNodes] = useState<CardNode[]>(cards);
  const nodesRef = useRef(nodes);
  nodesRef.current = nodes;
  useEffect(() => {
    // React Flow owns measurements and in-progress drag positions. The graph's
    // positions are committed on drag stop, so status/selection refreshes must
    // not restore those older positions while the pointer is still down.
    setNodes((current) => {
      const previous = new Map(current.map((node) => [node.id, node]));
      return cards.map((node) => {
        const local = previous.get(node.id);
        return {
          ...node,
          measured: local?.measured,
          ...(!readOnly && local?.dragging
            ? { position: local.position, dragging: true }
            : {}),
        };
      });
    });
  }, [cards, readOnly]);
  useEffect(() => {
    setViewport(viewportOf(graph));
  }, [graph.view.pan_x, graph.view.pan_y, graph.view.zoom]);
  useEffect(() => {
    if (props.frameSignal !== previousFrameSignal.current) {
      previousFrameSignal.current = props.frameSignal;
      void flow.fitView({ padding: 0.2, duration: 200, maxZoom: 1 });
    }
  }, [props.frameSignal, flow]);
  useEffect(() => {
    if (!menu) return;
    const close = () => setMenu(null);
    const pointer = (event: PointerEvent) => {
      if (!menuElement.current?.contains(event.target as globalThis.Node))
        close();
    };
    const key = (event: globalThis.KeyboardEvent) => {
      if (event.key === "Escape") close();
    };
    window.addEventListener("pointerdown", pointer, true);
    window.addEventListener("keydown", key, true);
    window.addEventListener("wheel", close, { passive: true });
    window.addEventListener("blur", close);
    return () => {
      window.removeEventListener("pointerdown", pointer, true);
      window.removeEventListener("keydown", key, true);
      window.removeEventListener("wheel", close);
      window.removeEventListener("blur", close);
    };
  }, [menu]);
  useEffect(() => {
    if (menu) setMenu(null);
  }, [graph, readOnly]);
  useEffect(() => {
    if (!props.focus) return;
    void flow.fitView({
      nodes: [{ id: props.focus.id }],
      padding: 0.4,
      duration: 250,
      maxZoom: 1,
    });
  }, [props.focus?.seq, flow]);
  const edges = useMemo<Edge[]>(
    () =>
      graph.edges.map((edge, index) => {
        const node = graph.nodes.find((item) => item.id === edge.from);
        const ports = portsById.get(edge.from) || [];
        const visiblePort =
          node && isNamedWorkflow(node) ? visibleEdgeHandle(edge, ports) : null;
        const handle =
          node && isNamedWorkflow(node)
            ? visiblePort === null
              ? "legacy-default"
              : portHandle(visiblePort)
            : "default";
        const missing =
          node &&
          isNamedWorkflow(node) &&
          (visiblePort === null ||
            ports.some((port) => port.id === visiblePort && port.missing));
        return {
          id: edgeId(edge, index),
          source: edge.from,
          target: edge.to,
          sourceHandle: handle,
          targetHandle: "input",
          selected: selectedEdge === edgeId(edge, index),
          selectable: true,
          deletable: !readOnly,
          reconnectable: false,
          className: missing ? "missing-wire" : undefined,
          style: missing
            ? { stroke: "var(--warning)", strokeDasharray: "5 4" }
            : undefined,
        };
      }),
    [graph, portsById, selectedEdge, readOnly],
  );

  const select = (id: string | null, extend = false) => {
    setSelectedEdge(null);
    props.onSelect(id, extend);
  };
  const selectedIds = () =>
    nodesRef.current.filter((node) => node.selected).map((node) => node.id);
  const changeNodes = (changes: NodeChange<CardNode>[]) => {
    // React Flow's measurement and selection updates are local UI state. Persist
    // positions only at the end of a drag, and never mutate a read-only child.
    const allowed = readOnly
      ? changes.filter(
          (change) =>
            change.type !== "position" &&
            change.type !== "remove" &&
            change.type !== "add" &&
            change.type !== "replace",
        )
      : changes;
    const next = applyNodeChanges(allowed, nodesRef.current);
    nodesRef.current = next;
    setNodes(next);
    if (allowed.some((change) => change.type === "select")) {
      const ids = next.filter((node) => node.selected).map((node) => node.id);
      const current = latest.current;
      const known = [
        ...new Set([
          ...(current.selection || []),
          ...(current.selected ? [current.selected] : []),
        ]),
      ];
      if (!sameIds(ids, known)) current.onSelectionChange?.(ids);
    }
  };
  const connect = (connection: Connection) => {
    const current = latest.current;
    const output = connection.sourceHandle?.startsWith("port:")
      ? decodeURIComponent(connection.sourceHandle.slice(5))
      : undefined;
    const changed = connectNodes(
      current.graph,
      connection.source,
      connection.target,
      output,
      current.workflowGraphs,
      current.readOnly,
    );
    if (changed !== current.graph) current.onChange(changed);
  };
  const removeEdge = (id: string) => {
    const current = latest.current;
    current.onChange({
      ...current.graph,
      edges: current.graph.edges.filter(
        (edge, index) => edgeId(edge, index) !== id,
      ),
    });
    setSelectedEdge(null);
  };
  const removeNodes = (ids: string[]) => {
    const current = latest.current;
    if (!ids.length) return;
    current.onChange(deleteNodes(current.graph, ids));
    select(null);
  };
  const keyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    const target = event.target as HTMLElement;
    if (target.closest('input, textarea, select, [contenteditable="true"]'))
      return;
    if (event.key === "Escape") {
      setMenu(null);
      select(null);
      return;
    }
    if (readOnly || !["Delete", "Backspace"].includes(event.key)) return;
    if (selectedEdge) {
      removeEdge(selectedEdge);
      event.preventDefault();
    } else {
      const ids = selectedIds();
      if (!ids.length && selected) ids.push(selected);
      if (ids.length) {
        removeNodes(ids);
        event.preventDefault();
      }
    }
  };
  const openMenu = (
    event: ReactMouseEvent | MouseEvent,
    target: MenuTarget,
  ) => {
    event.preventDefault();
    const bounds = wrapper.current?.getBoundingClientRect();
    const x = event.clientX - (bounds?.left ?? 0);
    const y = event.clientY - (bounds?.top ?? 0);
    const width = bounds?.width ?? Infinity;
    const height = bounds?.height ?? Infinity;
    setMenu({
      x: Math.max(0, Math.min(x, width - MENU_WIDTH - 8)),
      y: Math.max(0, Math.min(y, height - 220)),
      target,
    });
  };
  const flowPoint = (event: { clientX: number; clientY: number }) =>
    flow.screenToFlowPosition({ x: event.clientX, y: event.clientY });
  const menuItems = (target: MenuTarget): MenuItem[] => {
    const current = latest.current;
    if (target.kind === "node") {
      const node = current.graph.nodes.find((item) => item.id === target.id);
      const ids = selectionSet.has(target.id) ? [...selectionSet] : [target.id];
      const many = ids.length > 1;
      const items: MenuItem[] = [];
      if (node && typedData(node)?.type === "workflow")
        items.push({
          label: "Open child graph",
          run: () => current.onOpenWorkflow(target.id),
        });
      if (readOnly) return items;
      if (current.onRunNode && !many) {
        const run = current.onRunNode;
        items.push({ label: "Run node", run: () => run(target.id, false) });
        items.push({ label: "Run fresh", run: () => run(target.id, true) });
      }
      if (current.onCopy) {
        const copy = current.onCopy;
        items.push({
          label: many ? `Copy ${ids.length} nodes` : "Copy",
          keys: "⌘C",
          run: () => copy(ids),
        });
      }
      if (current.onDuplicate) {
        const duplicate = current.onDuplicate;
        items.push({
          label: many ? `Duplicate ${ids.length} nodes` : "Duplicate",
          keys: "⌘D",
          run: () => duplicate(ids),
        });
      }
      items.push({
        label: many ? `Delete ${ids.length} nodes` : "Delete",
        keys: "⌫",
        danger: true,
        run: () => removeNodes(ids),
      });
      return items;
    }
    if (target.kind === "edge")
      return readOnly
        ? []
        : [
            {
              label: "Delete wire",
              keys: "⌫",
              danger: true,
              run: () => removeEdge(target.id),
            },
          ];
    const items: MenuItem[] = [];
    if (!readOnly) {
      items.push({
        label: "Add node here",
        run: () => current.onAdd(target.position),
      });
      if (current.onPaste) {
        const paste = current.onPaste;
        items.push({
          label: "Paste here",
          keys: "⌘V",
          disabled: !current.canPaste,
          run: () => paste(target.position),
        });
      }
    }
    items.push({
      label: "Fit graph",
      run: () => void flow.fitView({ padding: 0.2, duration: 200, maxZoom: 1 }),
    });
    return items;
  };
  const acceptsLibraryDrop = (event: DragEvent) =>
    !readOnly &&
    Array.from(event.dataTransfer.types).includes(LIBRARY_DRAG_TYPE);
  const items = menu ? menuItems(menu.target) : [];

  return (
    <div
      ref={wrapper}
      className={`graph-canvas${readOnly ? " is-readonly" : ""}`}
      onKeyDown={keyDown}
      tabIndex={0}
      aria-label={
        readOnly ? "Read-only workflow graph" : "Workflow graph editor"
      }
      onDragOver={(event) => {
        if (!acceptsLibraryDrop(event)) return;
        event.preventDefault();
        event.dataTransfer.dropEffect = "copy";
      }}
      onDrop={(event) => {
        if (!acceptsLibraryDrop(event)) return;
        const payload = event.dataTransfer.getData(LIBRARY_DRAG_TYPE);
        if (!payload) return;
        event.preventDefault();
        latest.current.onDropLibrary?.(payload, flowPoint(event));
      }}
    >
      <ReactFlow<CardNode>
        nodes={nodes}
        edges={edges}
        nodeTypes={nodeTypes}
        onNodesChange={changeNodes}
        onConnect={connect}
        nodesDraggable={!readOnly}
        nodesConnectable={!readOnly}
        edgesReconnectable={false}
        elementsSelectable
        selectionOnDrag={false}
        panOnDrag
        zoomOnDoubleClick={false}
        deleteKeyCode={null}
        multiSelectionKeyCode={readOnly ? null : ["Meta", "Control"]}
        selectionKeyCode={readOnly ? null : "Shift"}
        viewport={viewport}
        onViewportChange={setViewport}
        minZoom={0.15}
        maxZoom={2.5}
        onMoveEnd={(_event, next) => {
          const current = latest.current;
          if (
            current.graph.view.pan_x === next.x &&
            current.graph.view.pan_y === next.y &&
            (current.graph.view.zoom ?? 1) === next.zoom
          )
            return;
          const view = { pan_x: next.x, pan_y: next.y, zoom: next.zoom };
          if (current.onViewportChange) current.onViewportChange(view);
          else if (!current.readOnly)
            current.onChange({ ...current.graph, view });
        }}
        onNodeClick={(event, node) => {
          event.stopPropagation();
          select(node.id, event.metaKey || event.ctrlKey || event.shiftKey);
        }}
        onNodeDoubleClick={(event, node) => {
          event.stopPropagation();
          if (typedData(node.data.node)?.type === "workflow")
            props.onOpenWorkflow(node.id);
        }}
        onNodeContextMenu={(event, node) => {
          if (!selectionSet.has(node.id)) select(node.id);
          openMenu(event, { kind: "node", id: node.id });
        }}
        onEdgeContextMenu={(event, edge) => {
          props.onSelect(null);
          setSelectedEdge(edge.id);
          openMenu(event, { kind: "edge", id: edge.id });
        }}
        onPaneContextMenu={(event) =>
          openMenu(event, { kind: "pane", position: flowPoint(event) })
        }
        onNodeDragStop={(_event, _node, dragged) => {
          const current = latest.current;
          if (current.readOnly) return;
          const moved = new Map(
            dragged.map((node) => [node.id, node.position] as const),
          );
          current.onChange({
            ...current.graph,
            nodes: current.graph.nodes.map((item) => {
              const position = moved.get(item.id);
              return position
                ? { ...item, x: position.x, y: position.y }
                : item;
            }),
          });
        }}
        onEdgeClick={(event, edge) => {
          event.stopPropagation();
          props.onSelect(null);
          setSelectedEdge(edge.id);
        }}
        onPaneClick={() => select(null)}
        onDoubleClick={(event) => {
          if (
            readOnly ||
            !(event.target as Element).classList.contains("react-flow__pane")
          )
            return;
          props.onAdd(flowPoint(event));
        }}
        attributionPosition="top-right"
        defaultEdgeOptions={{ type: "default", interactionWidth: 20 }}
      >
        <Background variant={BackgroundVariant.Dots} gap={24} size={1} />
        <Controls showInteractive={false} />
      </ReactFlow>
      {menu && items.length > 0 && (
        <div
          ref={menuElement}
          className="canvas-menu"
          role="menu"
          aria-label="Canvas actions"
          style={{ left: menu.x, top: menu.y, width: MENU_WIDTH }}
        >
          {items.map((item) => (
            <button
              key={item.label}
              type="button"
              role="menuitem"
              className={item.danger ? "danger" : undefined}
              disabled={item.disabled}
              onClick={() => {
                setMenu(null);
                item.run();
              }}
            >
              <span>{item.label}</span>
              {item.keys && <kbd>{item.keys}</kbd>}
            </button>
          ))}
        </div>
      )}
      <div className="graph-help">
        {readOnly
          ? "Read-only child · select to inspect · drag canvas to pan"
          : "Drag to pan · ⇧ drag to select many · drag ports to connect · right-click for actions"}
      </div>
    </div>
  );
}

export function Canvas(props: CanvasProps) {
  return (
    <ReactFlowProvider>
      <CanvasInner {...props} />
    </ReactFlowProvider>
  );
}

export default Canvas;
