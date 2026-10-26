import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type KeyboardEvent,
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
  type Port,
  type WorkflowGraphs,
} from "./graph";

export interface CanvasProps {
  graph: GraphDoc;
  selected: string | null;
  onSelect: (id: string | null) => void;
  onChange: (doc: GraphDoc) => void;
  onOpenWorkflow: (id: string) => void;
  onAdd: (position: { x: number; y: number }) => void;
  readOnly: boolean;
  workflowGraphs: WorkflowGraphs;
  marks: Record<string, string>;
  frameSignal: number;
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
};
type CardNode = Node<CardData, "card">;
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

function CanvasInner(props: CanvasProps) {
  const { graph, selected, readOnly, workflowGraphs, marks } = props;
  const flow = useReactFlow<CardNode>();
  const latest = useRef(props);
  latest.current = props;
  const [selectedEdge, setSelectedEdge] = useState<string | null>(null);
  const [viewport, setViewport] = useState<Viewport>(() => viewportOf(graph));
  const previousFrameSignal = useRef(props.frameSignal);
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
          selected: selected === node.id,
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
            legacyDefault:
              named &&
              ports.filter((port) => !port.missing).length !== 1 &&
              graph.edges.some(
                (edge) => edge.from === node.id && edge.output == null,
              ),
          },
        };
      }),
    [graph, selected, portsById, readOnly, marks],
  );
  const [nodes, setNodes] = useState<CardNode[]>(cards);
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
            ? { stroke: "#d59855", strokeDasharray: "5 4" }
            : undefined,
        };
      }),
    [graph, portsById, selectedEdge, readOnly],
  );

  const select = (id: string | null) => {
    setSelectedEdge(null);
    props.onSelect(id);
  };
  const changeNodes = (changes: NodeChange<CardNode>[]) => {
    // React Flow's measurement and selection updates are local UI state. Persist
    // positions only at the end of a drag, and never mutate a read-only child.
    setNodes((current) =>
      applyNodeChanges(
        readOnly
          ? changes.filter(
              (change) =>
                change.type !== "position" &&
                change.type !== "remove" &&
                change.type !== "add" &&
                change.type !== "replace",
            )
          : changes,
        current,
      ),
    );
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
  const keyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    const target = event.target as HTMLElement;
    if (target.closest('input, textarea, select, [contenteditable="true"]'))
      return;
    if (event.key === "Escape") {
      select(null);
      return;
    }
    if (readOnly || !["Delete", "Backspace"].includes(event.key)) return;
    if (selectedEdge) {
      props.onChange({
        ...graph,
        edges: graph.edges.filter(
          (edge, index) => edgeId(edge, index) !== selectedEdge,
        ),
      });
      setSelectedEdge(null);
      event.preventDefault();
    } else if (selected) {
      props.onChange(deleteNodes(graph, [selected]));
      select(null);
      event.preventDefault();
    }
  };

  return (
    <div
      className={`graph-canvas${readOnly ? " is-readonly" : ""}`}
      onKeyDown={keyDown}
      tabIndex={0}
      aria-label={
        readOnly ? "Read-only workflow graph" : "Workflow graph editor"
      }
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
        multiSelectionKeyCode={null}
        selectionKeyCode={null}
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
          select(node.id);
        }}
        onNodeDoubleClick={(event, node) => {
          event.stopPropagation();
          if (typedData(node.data.node)?.type === "workflow")
            props.onOpenWorkflow(node.id);
        }}
        onNodeDragStop={(_event, node) => {
          const current = latest.current;
          if (!current.readOnly)
            current.onChange({
              ...current.graph,
              nodes: current.graph.nodes.map((item) =>
                item.id === node.id
                  ? { ...item, x: node.position.x, y: node.position.y }
                  : item,
              ),
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
          props.onAdd(
            flow.screenToFlowPosition({ x: event.clientX, y: event.clientY }),
          );
        }}
        attributionPosition="top-right"
        defaultEdgeOptions={{ type: "default", interactionWidth: 20 }}
      >
        <Background
          variant={BackgroundVariant.Dots}
          gap={24}
          size={1}
          color="#3a3d43"
        />
        <Controls showInteractive={false} />
      </ReactFlow>
      {graph.nodes.length === 0 && (
        <div className="graph-empty">
          <strong>
            {readOnly ? "This workflow is empty" : "Build a workflow"}
          </strong>
          <span>
            {readOnly
              ? "Select another graph in the library."
              : "Double-click the canvas to add a node."}
          </span>
        </div>
      )}
      <div className="graph-help">
        {readOnly
          ? "Read-only child · select to inspect · drag canvas to pan"
          : "Drag to pan · scroll to zoom · drag ports to connect · Delete removes selection"}
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
