import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type CSSProperties,
  type ReactNode,
} from "react";
import {
  ArrowLeft,
  ArrowUpRight,
  Boxes,
  ChevronRight,
  CircleHelp,
  Files,
  FolderOpen,
  PanelLeft,
  PanelRight,
  Pencil,
  Play,
  Plus,
  Redo2,
  RefreshCw,
  Save,
  Search,
  Square,
  Trash2,
  Undo2,
  X,
} from "lucide-react";
import Canvas, { LIBRARY_DRAG_TYPE } from "./Canvas";
import ComboInput from "./ComboInput";
import Inspector from "./Inspector";
import RunPanel from "./RunPanel";
import { onHostEvent, quitApp, request } from "./api";
import {
  CARD_SIZE,
  cloneNodes,
  dataFormatFor,
  defaultNodeData,
  deleteNodes,
  flowToGraphPoint,
  freePosition,
  graphToFlowPoint,
  nextNodeId,
  nodeColor,
  nodeLabel,
  scriptRuntimeFor,
  typedData,
  type Placement,
  type Point,
} from "./graph";
import {
  callAt,
  childInterfaces,
  emptyGraph,
  firstSelection,
  localMarks,
  normalizeGraph,
  runMarks,
  safeGraphName,
} from "./workspace";
import type {
  EdgeDoc,
  GraphDoc,
  Issue,
  Library,
  NodeData,
  NodeDoc,
  NodeRun,
  NodeType,
  Run,
  RunForm,
  RuntimeState,
} from "./types";

interface ChildView {
  path: string;
  label: string;
  graph: GraphDoc;
  selected: string | null;
  snapshot: boolean;
}
const labels: Record<NodeType, string> = {
  input: "Run input",
  workflow: "Workflow",
  agent: "Agent",
  database: "Database",
  script: "Script",
  data: "Data",
  directory: "Directory",
  file: "Write file",
};
const messageOf = (error: unknown) =>
  error instanceof Error ? error.message : String(error);
type CreateType = NodeType | "blank" | "database-write";
function createData(type: CreateType): NodeData | undefined {
  if (type === "blank") return undefined;
  if (type === "database-write") {
    const database = defaultNodeData("database");
    if (database.type === "database") return { ...database, read_only: false };
  }
  return defaultNodeData(type as NodeType);
}
const LAYOUT_KEY = "uapz.layout";
type Theme = "system" | "dark" | "light";
interface Layout {
  leftWidth?: number;
  rightWidth?: number;
  libraryOpen?: boolean;
  inspectorOpen?: boolean;
  theme?: Theme;
  lastGraph?: { workspace: string; name: string };
}
/** Per-user conveniences only. Storage can be missing, blocked or cleared, and
 * the app must behave the same without it. */
function readLayout(): Layout {
  try {
    const raw = window.localStorage.getItem(LAYOUT_KEY);
    const value: unknown = raw ? JSON.parse(raw) : null;
    return value && typeof value === "object" ? (value as Layout) : {};
  } catch {
    return {};
  }
}
function writeLayout(patch: Layout) {
  try {
    window.localStorage.setItem(
      LAYOUT_KEY,
      JSON.stringify({ ...readLayout(), ...patch }),
    );
  } catch {
    // The layout memory is optional.
  }
}
const clamp = (value: unknown, low: number, high: number, fallback: number) =>
  typeof value === "number" && Number.isFinite(value)
    ? Math.min(high, Math.max(low, value))
    : fallback;
type Choice = "cancel" | "confirm" | "alternative";
/** The workflows the service can install from its bundled examples. */
const EXAMPLE_NAMES = [
  "reviewed-note",
  "call-reviewed-note",
  "note-parts",
  "use-note-parts",
];
const SHORTCUTS: [string, string][] = [
  ["⌘S", "Save the workflow"],
  ["⌘↵", "Run the workflow"],
  ["⌘⇧↵", "Run the workflow without cached outputs"],
  ["⌘N", "Add a node of the selected type"],
  ["⌘⇧N", "New workflow"],
  ["⌘Z · ⌘⇧Z", "Undo · redo"],
  ["⌘C · ⌘V · ⌘D", "Copy · paste · duplicate the selected nodes"],
  ["⌫", "Delete the selected nodes or wire"],
  ["Esc", "Clear the selection"],
  ["⇧ drag", "Select every node in a box"],
  ["⌘ click", "Add a node to the selection"],
  ["Double-click", "Add a node on the canvas · open a child workflow"],
  ["Right-click", "Actions for a node, wire or the canvas"],
  ["⌘B · ⌘J", "Show or hide the library · the inspector"],
  ["?", "This list"],
];
/** What a library row adds, carried on a drag so the canvas can place it at the drop. */
type LibraryItem =
  | { kind: "type"; type: CreateType }
  | { kind: "kind"; name: string; color: string }
  | { kind: "agent"; name: string }
  | { kind: "database"; name: string }
  | { kind: "script"; name: string }
  | { kind: "data"; name: string };
function parseLibraryItem(payload: string): LibraryItem | null {
  let value: unknown;
  try {
    value = JSON.parse(payload);
  } catch {
    return null;
  }
  if (!value || typeof value !== "object") return null;
  const item = value as Record<string, unknown>;
  if (
    item.kind === "type" &&
    typeof item.type === "string" &&
    (item.type === "blank" ||
      item.type === "database-write" ||
      Object.hasOwn(labels, item.type))
  )
    return { kind: "type", type: item.type as CreateType };
  if (typeof item.name !== "string") return null;
  if (item.kind === "kind" && typeof item.color === "string")
    return { kind: "kind", name: item.name, color: item.color };
  if (
    item.kind === "agent" ||
    item.kind === "database" ||
    item.kind === "script" ||
    item.kind === "data"
  )
    return { kind: item.kind, name: item.name };
  return null;
}

export default function App() {
  const [library, setLibrary] = useState<Library | null>(null);
  const [graph, setGraph] = useState<GraphDoc>(emptyGraph);
  const [selected, setSelected] = useState<string | null>(null);
  const [selection, setSelection] = useState<string[]>([]);
  const [children, setChildren] = useState<ChildView[]>([]);
  const [dirty, setDirty] = useState(false);
  const [runtime, setRuntime] = useState<RuntimeState | null>(null);
  const [attachedRun, setAttachedRun] = useState<string | null>(null);
  const [shownRun, setShownRun] = useState<Run | null>(null);
  const [historical, setHistorical] = useState(false);
  const [remembered, setRemembered] = useState<{
    nodes: Record<string, NodeRun>;
    origins: Record<string, Run>;
  }>({ nodes: {}, origins: {} });
  const [history, setHistory] = useState<Run[]>([]);
  const [validation, setValidation] = useState<{
    key: string;
    issues: Issue[];
  } | null>(null);
  const [form, setForm] = useState<RunForm | null>(null);
  const [panel, setPanel] = useState<"inspector" | "run">("inspector");
  const [query, setQuery] = useState("");
  const [notice, setNotice] = useState("Ready");
  const [error, setError] = useState<string | null>(null);
  const [confirmation, setConfirmation] = useState<{
    title: string;
    message: string;
    confirm: string;
    alternative?: string;
    input?: { label: string; initial: string };
    resolve: (choice: Choice, value: string) => void;
  } | null>(null);
  const [focusRequest, setFocusRequest] = useState<{
    id: string;
    seq: number;
  } | null>(null);
  const [shortcutsOpen, setShortcutsOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [pendingRunId, setPendingRunId] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [kindEditor, setKindEditor] = useState(false);
  const [kindName, setKindName] = useState("");
  const [kindColor, setKindColor] = useState("#64748b");
  const [addType, setAddType] = useState<CreateType>("blank");
  const [frameSignal, setFrameSignal] = useState(0);
  const [refreshVersion, setRefreshVersion] = useState(0);
  const [savedInterfaces, setSavedInterfaces] = useState<{
    key: string;
    graphs: Record<string, GraphDoc>;
  } | null>(null);
  const [leftWidth, setLeftWidth] = useState(() =>
    clamp(readLayout().leftWidth, 190, 380, 246),
  );
  const [rightWidth, setRightWidth] = useState(() =>
    clamp(readLayout().rightWidth, 300, 1600, 370),
  );
  const [libraryOpen, setLibraryOpen] = useState(
    () => readLayout().libraryOpen !== false,
  );
  const [inspectorOpen, setInspectorOpen] = useState(
    () => readLayout().inspectorOpen !== false,
  );
  const [theme, setTheme] = useState<Theme>(() => {
    const stored = readLayout().theme;
    return stored === "dark" || stored === "light" ? stored : "system";
  });
  useEffect(() => {
    const root = document.documentElement;
    if (theme === "system") delete root.dataset.theme;
    else root.dataset.theme = theme;
  }, [theme]);
  const inspectorColumn = useRef<HTMLElement>(null);
  const canvasHost = useRef<HTMLDivElement>(null);
  const [historyVersion, setHistoryVersion] = useState(0);
  const [clipboardSize, setClipboardSize] = useState(0);
  const undoStack = useRef<{ graph: GraphDoc; key: string; at: number }[]>([]);
  const redoStack = useRef<GraphDoc[]>([]);
  const clipboard = useRef<{
    nodes: NodeDoc[];
    edges: EdgeDoc[];
    pastes: number;
  } | null>(null);
  const savedKey = useRef<string | null>(null);
  const dirtyRef = useRef(dirty);
  dirtyRef.current = dirty;
  const closePrompt = useRef(false);
  const closeRequested = useRef<() => void>(() => {});
  const lastCompleted = useRef<string | null>(null);
  const documentEpoch = useRef(0);
  const graphRef = useRef(graph);
  graphRef.current = graph;
  const runtimeRef = useRef(runtime);
  runtimeRef.current = runtime;
  const historicalRef = useRef(historical);
  historicalRef.current = historical;
  const saveInFlight = useRef(false);
  const preparingRun = useRef(false);
  const startingRun = useRef(false);
  const pendingRun = useRef<string | null>(null);
  const runtimeRequest = useRef(0);
  const appliedRuntimeRequest = useRef(0);
  const actionRequest = useRef(0);
  const promptRequest = useRef(0);
  const rootNavigation = useRef(0);
  const childNavigation = useRef(0);
  const epoch = documentEpoch.current;
  const initialSession = useRef(true);
  const activeChild = children.at(-1);
  const activeGraph = activeChild?.graph || graph;
  const activeSelected = activeChild ? activeChild.selected : selected;
  const activeNode =
    activeGraph.nodes.find((node) => node.id === activeSelected) || null;
  const readOnly = !!activeChild;
  const running = !!runtime?.running || pendingRunId !== null;
  const occupied = running || !!form || busy;
  const runMatches = !!runtime && runtime.run_id === attachedRun;
  const currentRun = shownRun || (runMatches ? runtime?.run : null) || null;
  const snapshotContext = (runMatches && running) || !!currentRun;
  const captured =
    runMatches && running ? runtime.workflows : currentRun?.workflows || {};
  const childRun = activeChild
    ? callAt(currentRun, activeChild.path)?.run || null
    : currentRun;
  const outputRun =
    !readOnly && !historical && activeSelected
      ? remembered.origins[activeSelected] || currentRun
      : currentRun;
  const childPath = activeChild?.path || "";
  const promptSelection = useRef({
    epoch,
    childPath,
    selected: activeSelected,
  });
  promptSelection.current = { epoch, childPath, selected: activeSelected };
  const validationKey = JSON.stringify({
    ...graph,
    view: { pan_x: 0, pan_y: 0, zoom: 1 },
    nodes: graph.nodes.map((node) => ({ ...node, x: 0, y: 0 })),
  });
  const currentIssues =
    !readOnly && validation?.key === validationKey ? validation.issues : [];
  // The first problem per node wins; later entries would otherwise replace it.
  const problems = Object.fromEntries(
    [...currentIssues]
      .reverse()
      .flatMap((issue) =>
        issue.node ? [[issue.node, `${issue.field}: ${issue.message}`]] : [],
      ),
  );
  const problemCount = currentIssues.length;
  const interfaceKey = JSON.stringify(
    activeGraph.nodes
      .filter((node) => typedData(node)?.type === "workflow")
      .map((node) => [node.id, typedData(node)]),
  );
  const workflowGraphs = activeChild?.snapshot
    ? childInterfaces(captured, childPath, childRun)
    : savedInterfaces?.key === interfaceKey
      ? savedInterfaces.graphs
      : {};
  const marks =
    runMatches && running
      ? localMarks(runtime.marks, childPath)
      : runMarks(childRun);
  const prefix = childPath ? `${childPath}/` : "";
  const liveLog =
    (runMatches && running
      ? runtime.log
          .filter((line) => !prefix || line.startsWith(prefix))
          .map((line) => (prefix ? line.slice(prefix.length) : line))
      : childRun?.log) || [];
  const liveStream =
    runMatches && running && runtime.stream?.id.startsWith(prefix)
      ? {
          id: runtime.stream.id.slice(prefix.length),
          text: runtime.stream.text,
        }
      : null;

  const refreshLibrary = useCallback(async () => {
    const data = await request("bootstrap");
    setLibrary(data);
    setRefreshVersion((version) => version + 1);
    return data;
  }, []);
  const refreshHistory = useCallback(async (name: string) => {
    const currentEpoch = documentEpoch.current;
    const runs = await request("history", { graph: name });
    if (currentEpoch === documentEpoch.current) setHistory(runs);
  }, []);
  const action = useCallback(async (task: () => Promise<void>) => {
    const requestId = ++actionRequest.current;
    const requestEpoch = documentEpoch.current;
    setError(null);
    try {
      await task();
    } catch (error) {
      if (
        requestId === actionRequest.current &&
        requestEpoch === documentEpoch.current
      )
        setError(messageOf(error));
    }
  }, []);
  const readRuntime = useCallback(
    async (isCurrent: (state: RuntimeState | null) => boolean) => {
      const requestId = ++runtimeRequest.current;
      let state: RuntimeState | null;
      try {
        state = await request("runtime_state");
      } catch (error) {
        if (requestId < appliedRuntimeRequest.current)
          return { state: null, applied: false };
        throw error;
      }
      // Polls and command follow-up reads overlap. A response issued before
      // the last applied read cannot restore an older review or running state.
      if (requestId < appliedRuntimeRequest.current || !isCurrent(state))
        return { state, applied: false };
      appliedRuntimeRequest.current = requestId;
      runtimeRef.current = state;
      setRuntime(state);
      if (state?.run_id === pendingRun.current) {
        pendingRun.current = null;
        setPendingRunId(null);
      }
      return { state, applied: true };
    },
    [],
  );

  useEffect(() => {
    void refreshLibrary().catch((error) => setError(messageOf(error)));
  }, [refreshLibrary]);
  useEffect(() => {
    // The shell holds the window open until unsaved work is settled here.
    let active = true;
    let stop = () => {};
    void onHostEvent("close-requested", () => closeRequested.current()).then(
      (unlisten) => {
        if (active) stop = unlisten;
        else unlisten();
      },
    );
    return () => {
      active = false;
      stop();
    };
  }, []);
  useEffect(() => {
    if (library)
      void refreshHistory(graph.name).catch((error) =>
        setError(messageOf(error)),
      );
  }, [library?.workspace, graph.name, refreshHistory]);
  useEffect(() => {
    if (!library) return;
    let disposed = false;
    let timer: ReturnType<typeof setTimeout>;
    const poll = async () => {
      let delay = 1000;
      try {
        const { state, applied } = await readRuntime(() => !disposed);
        if (disposed) return;
        if (!applied) {
          timer = setTimeout(poll, 150);
          return;
        }
        if (initialSession.current) {
          initialSession.current = false;
          if (state?.running && documentEpoch.current === 0) {
            setGraph(normalizeGraph(state.graph));
            resetHistory(null);
            setAttachedRun(state.run_id);
            setPanel("run");
            setDirty(true);
          } else if (documentEpoch.current === 0) {
            // Pick up where the last session left off.
            const last = readLayout().lastGraph;
            if (
              last &&
              typeof last.name === "string" &&
              last.workspace === library.workspace &&
              library.graphs.includes(last.name)
            )
              void action(() => openGraph(last.name));
          }
        }
        if (state?.running) delay = 150;
        if (
          state?.run &&
          state.run_id !== lastCompleted.current &&
          state.run_id === attachedRun
        ) {
          lastCompleted.current = state.run_id;
          // Review responses can expose finished state before this poll sees
          // it. Keep a history run explicitly selected in that interval while
          // still recording the newly completed results and refreshing history.
          const showCompletion = !historicalRef.current;
          if (showCompletion) {
            setShownRun(state.run);
            setHistorical(false);
          }
          const run = state.run;
          setRemembered((current) => ({
            nodes: { ...current.nodes, ...run.nodes },
            origins: {
              ...current.origins,
              ...Object.fromEntries(
                Object.keys(run.nodes).map((id) => [id, run]),
              ),
            },
          }));
          void refreshHistory(graph.name);
          if (showCompletion) {
            setNotice(
              state.run.outcome.kind === "ok"
                ? `Completed in ${(state.run.elapsed_ms / 1000).toFixed(1)}s`
                : state.run.outcome.kind === "cancelled"
                  ? "Run cancelled"
                  : "Run failed",
            );
            if (state.error) setError(state.error);
          }
        }
      } catch (error) {
        if (!disposed) setError(messageOf(error));
      }
      if (!disposed) timer = setTimeout(poll, delay);
    };
    void poll();
    return () => {
      disposed = true;
      clearTimeout(timer);
    };
  }, [
    library?.workspace,
    graph.name,
    attachedRun,
    refreshHistory,
    readRuntime,
  ]);

  useEffect(() => {
    if (!library || readOnly) return;
    let disposed = false;
    const timer = setTimeout(() => {
      void request("validate_graph", {
        doc: JSON.parse(validationKey),
      })
        .then((result) => {
          if (!disposed)
            setValidation({ key: validationKey, issues: result.issues });
        })
        .catch((error) => {
          if (!disposed) setError(messageOf(error));
        });
    }, 250);
    return () => {
      disposed = true;
      clearTimeout(timer);
    };
  }, [validationKey, library?.workspace, refreshVersion, readOnly]);

  useEffect(() => {
    if (activeChild?.snapshot || !library) return;
    let disposed = false;
    const workflows = activeGraph.nodes.flatMap((node) => {
      const data = typedData(node);
      return data?.type === "workflow" && data.graph
        ? [{ id: node.id, name: data.graph }]
        : [];
    });
    void Promise.all(
      workflows.map(async ({ id, name }) => {
        try {
          return [id, await request("load_graph", { name })] as const;
        } catch {
          return null;
        }
      }),
    ).then((entries) => {
      if (!disposed)
        setSavedInterfaces({
          key: interfaceKey,
          graphs: Object.fromEntries(entries.filter((entry) => entry !== null)),
        });
    });
    return () => {
      disposed = true;
    };
  }, [
    interfaceKey,
    activeChild?.path,
    activeChild?.snapshot,
    library?.workspace,
    refreshVersion,
  ]);

  const documentKey = (doc: GraphDoc) =>
    JSON.stringify({ ...doc, view: undefined });
  function resetHistory(doc: GraphDoc | null) {
    undoStack.current = [];
    redoStack.current = [];
    savedKey.current = doc ? documentKey(doc) : null;
    setHistoryVersion((version) => version + 1);
  }
  /** Keep the document from before an edit so the edit can be undone. Edits to
   * one node within a short pause form a single step, so typing a name does
   * not undo one letter at a time. */
  function remember(previous: GraphDoc, key: string) {
    const stack = undoStack.current;
    const top = stack.at(-1);
    const now = Date.now();
    if (top && top.graph === previous) return;
    if (top && key !== "graph" && top.key === key && now - top.at < 800) {
      top.at = now;
      return;
    }
    stack.push({ graph: previous, key, at: now });
    if (stack.length > 100) stack.shift();
    redoStack.current = [];
    setHistoryVersion((version) => version + 1);
  }
  function restore(doc: GraphDoc) {
    // Pan and zoom are navigation, not edits: keep the current view.
    const restored = { ...doc, view: graphRef.current.view };
    setGraph(restored);
    graphRef.current = restored;
    setDirty(documentKey(restored) !== savedKey.current);
    const present = new Set(restored.nodes.map((node) => node.id));
    if (selected && !present.has(selected)) setSelected(null);
    setSelection((ids) => ids.filter((id) => present.has(id)));
    setHistoryVersion((version) => version + 1);
  }
  function undo() {
    if (readOnly) return;
    const entry = undoStack.current.pop();
    if (!entry) return;
    redoStack.current.push(graphRef.current);
    restore(entry.graph);
  }
  function redo() {
    if (readOnly) return;
    const next = redoStack.current.pop();
    if (!next) return;
    undoStack.current.push({ graph: graphRef.current, key: "graph", at: 0 });
    restore(next);
  }
  function updateGraph(next: GraphDoc) {
    if (readOnly) return;
    remember(graphRef.current, "graph");
    setGraph(next);
    graphRef.current = next;
    setDirty(true);
  }
  function selectNode(id: string | null, extend = false) {
    if (id && !occupied) setPanel("inspector");
    if (readOnly)
      setChildren((items) =>
        items.map((item, index) =>
          index === items.length - 1 ? { ...item, selected: id } : item,
        ),
      );
    else {
      setSelected(id);
      if (!extend) setSelection(id ? [id] : []);
      else if (id)
        setSelection((ids) => (ids.includes(id) ? ids : [...ids, id]));
    }
  }
  function changeSelection(ids: string[]) {
    if (readOnly) return;
    setSelection(ids);
    setSelected((current) =>
      current && ids.includes(current) ? current : (ids[0] ?? null),
    );
  }
  function updateNode(node: NodeDoc) {
    if (readOnly || epoch !== documentEpoch.current) return;
    remember(graphRef.current, node.id);
    setGraph((current) => ({
      ...current,
      nodes: current.nodes.map((item) => (item.id === node.id ? node : item)),
    }));
    setDirty(true);
  }
  /** Every selected root node that still exists, the primary one included. */
  function selectedIds(): string[] {
    const present = new Set(graph.nodes.map((node) => node.id));
    const ids = selection.filter((id) => present.has(id));
    if (selected && present.has(selected) && !ids.includes(selected))
      ids.push(selected);
    return ids;
  }
  function visibleCenter(): Point {
    const host = canvasHost.current;
    const center = flowToGraphPoint(
      {
        x: (host?.clientWidth || 800) / 2,
        y: (host?.clientHeight || 500) / 2,
      },
      graph.view,
    );
    return {
      x: center.x - CARD_SIZE.width / 2,
      y: center.y - CARD_SIZE.height / 2,
    };
  }
  function copyNodes(ids = selectedIds()) {
    if (readOnly || !ids.length) return false;
    const chosen = new Set(ids);
    clipboard.current = {
      nodes: graph.nodes
        .filter((node) => chosen.has(node.id))
        .map((node) => structuredClone(node)),
      edges: graph.edges.filter(
        (edge) => chosen.has(edge.from) && chosen.has(edge.to),
      ),
      pastes: 0,
    };
    setClipboardSize(ids.length);
    setNotice(`Copied ${ids.length} node${ids.length === 1 ? "" : "s"}`);
    return true;
  }
  function insertCopies(
    nodes: NodeDoc[],
    edges: EdgeDoc[],
    placement: Placement,
  ) {
    const { graph: next, ids } = cloneNodes(graph, nodes, edges, placement);
    if (!ids.length) return 0;
    updateGraph(next);
    setSelected(ids[0] ?? null);
    setSelection(ids);
    setPanel("inspector");
    return ids.length;
  }
  function duplicateNodes(ids = selectedIds()) {
    if (readOnly || !ids.length) return;
    const chosen = new Set(ids);
    const count = insertCopies(
      graph.nodes.filter((node) => chosen.has(node.id)),
      graph.edges.filter(
        (edge) => chosen.has(edge.from) && chosen.has(edge.to),
      ),
      { offset: { x: 40, y: 40 } },
    );
    setNotice(`Duplicated ${count} node${count === 1 ? "" : "s"}`);
  }
  function pasteNodes(at?: Point) {
    const clip = clipboard.current;
    if (readOnly || !clip) return false;
    clip.pastes += 1;
    const host = canvasHost.current;
    const onScreen = (node: NodeDoc) => {
      const point = graphToFlowPoint({ x: node.x, y: node.y }, graph.view);
      return (
        point.x >= 0 &&
        point.y >= 0 &&
        point.x <= (host?.clientWidth ?? 0) &&
        point.y <= (host?.clientHeight ?? 0)
      );
    };
    // Repeated pastes step away from the originals; copies from another
    // workflow or off-screen land in the middle of the visible canvas.
    const step = 40 * clip.pastes;
    const placement: Placement = at
      ? { at }
      : clip.nodes.some(onScreen)
        ? { offset: { x: step, y: step } }
        : { at: visibleCenter() };
    const count = insertCopies(clip.nodes, clip.edges, placement);
    setNotice(`Pasted ${count} node${count === 1 ? "" : "s"}`);
    return true;
  }
  function addNode(
    data?: NodeData,
    position?: { x: number; y: number },
    kind?: { name: string; color: string },
  ) {
    if (readOnly) return;
    const id = nextNodeId(graph);
    // A double-click or drop lands the card under the pointer; the toolbar and
    // library place it in free space around the middle of the visible canvas.
    const placed = position
      ? { x: position.x - CARD_SIZE.width / 2, y: position.y - 24 }
      : freePosition(graph, visibleCenter());
    const node: NodeDoc = {
      id,
      x: placed.x,
      y: placed.y,
      data: data || {},
      ...(kind ? { kind: kind.name, color: kind.color } : {}),
    };
    if (data && !kind) node.color = nodeColor(node);
    updateGraph({ ...graph, nodes: [...graph.nodes, node] });
    setSelected(id);
    setSelection([id]);
    setPanel("inspector");
  }
  /** Add what a library row stands for, at a drop position or in free space. */
  function addFromLibrary(item: LibraryItem, position?: Point) {
    switch (item.kind) {
      case "type":
        addNode(createData(item.type), position);
        return;
      case "kind":
        addNode(undefined, position, { name: item.name, color: item.color });
        return;
      case "agent": {
        const agent = library?.agents.find((entry) => entry.name === item.name);
        if (agent)
          addNode({ type: "agent", ...agent.config }, position, {
            name: agent.name,
            color: "#8b5cf6",
          });
        return;
      }
      case "database": {
        const database = library?.databases.find(
          (entry) => entry.name === item.name,
        );
        if (database)
          addNode({ type: "database", ...database.config }, position, {
            name: database.name,
            color: database.config.read_only ? "#0ea5e9" : "#ef4444",
          });
        return;
      }
      case "script":
        if (!position && pointNodeAt("script", item.name)) return;
        addNode(
          {
            type: "script",
            runtime: scriptRuntimeFor(item.name),
            path: item.name,
            args: [],
            timeout_secs: 60,
          },
          position,
        );
        return;
      case "data":
        if (!position && pointNodeAt("data", item.name)) return;
        addNode(
          { type: "data", path: item.name, format: dataFormatFor(item.name) },
          position,
        );
        return;
    }
  }
  /** Point the selected script or data node at a library file instead of adding one. */
  function pointNodeAt(type: "script" | "data", name: string) {
    if (readOnly || !activeNode) return false;
    const config = typedData(activeNode);
    let data: NodeData | undefined;
    if (type === "script" && config?.type === "script")
      data = { ...config, path: name, runtime: scriptRuntimeFor(name) };
    if (type === "data" && config?.type === "data")
      data = { ...config, path: name, format: dataFormatFor(name) };
    if (!data) return false;
    updateNode({ ...activeNode, data });
    setNotice(`${nodeLabel(activeNode)} now reads ${name}`);
    return true;
  }
  function choose(
    message: string,
    options: {
      title?: string;
      confirm?: string;
      alternative?: string;
      input?: { label: string; initial: string };
    } = {},
  ) {
    return new Promise<{ choice: Choice; value: string }>((resolve) =>
      setConfirmation({
        title: options.title ?? "Confirm change",
        message,
        confirm: options.confirm ?? "Continue",
        ...(options.alternative ? { alternative: options.alternative } : {}),
        ...(options.input ? { input: options.input } : {}),
        resolve: (choice, value) => resolve({ choice, value }),
      }),
    ).then((result) => {
      lastChoiceValue.current = result.value;
      return result.choice;
    });
  }
  const lastChoiceValue = useRef("");
  /** Ask for one line of text; null when the dialog is cancelled. */
  async function promptFor(
    message: string,
    options: { title: string; confirm: string; label: string; initial: string },
  ) {
    const choice = await choose(message, {
      title: options.title,
      confirm: options.confirm,
      input: { label: options.label, initial: options.initial },
    });
    return choice === "confirm" ? lastChoiceValue.current : null;
  }
  async function confirmAction(
    message: string,
    options?: { title?: string; confirm?: string },
  ) {
    return (await choose(message, options)) === "confirm";
  }
  async function discardDraft() {
    if (!dirty) return true;
    const choice = await choose("This workflow has unsaved changes.", {
      title: "Unsaved changes",
      confirm: "Discard changes",
      ...(readOnly ? {} : { alternative: "Save and continue" }),
    });
    if (choice !== "alternative") return choice === "confirm";
    if (saveInFlight.current) return false;
    try {
      await save();
    } catch (error) {
      setError(messageOf(error));
      return false;
    }
    return true;
  }
  async function newGraph() {
    if (
      occupied ||
      preparingRun.current ||
      startingRun.current ||
      pendingRun.current
    )
      return;
    const navigation = ++rootNavigation.current;
    if (!(await discardDraft()) || navigation !== rootNavigation.current)
      return;
    documentEpoch.current += 1;
    setRemembered({ nodes: {}, origins: {} });
    setHistorical(false);
    const fresh = emptyGraph();
    setGraph(fresh);
    resetHistory(fresh);
    setSelected(null);
    setSelection([]);
    setChildren([]);
    setShownRun(null);
    setAttachedRun(null);
    setDirty(false);
    setNotice("New workflow");
    setPanel("inspector");
  }
  async function openGraph(name: string) {
    if (
      occupied ||
      preparingRun.current ||
      startingRun.current ||
      pendingRun.current
    )
      return;
    const navigation = ++rootNavigation.current;
    if (!(await discardDraft()) || navigation !== rootNavigation.current)
      return;
    let loaded: GraphDoc;
    try {
      loaded = normalizeGraph(await request("load_graph", { name }));
    } catch (error) {
      if (navigation !== rootNavigation.current) return;
      throw error;
    }
    if (navigation !== rootNavigation.current) return;
    documentEpoch.current += 1;
    setRemembered({ nodes: {}, origins: {} });
    setHistorical(false);
    setGraph(loaded);
    resetHistory(loaded);
    const first = firstSelection(loaded);
    setSelected(first);
    setSelection(first ? [first] : []);
    setChildren([]);
    setShownRun(null);
    setAttachedRun(null);
    setDirty(false);
    setNotice(`Opened ${name}`);
    setPanel("inspector");
    writeLayout({ lastGraph: { workspace: library?.workspace ?? "", name } });
  }
  async function save() {
    if (readOnly || saveInFlight.current) return;
    saveInFlight.current = true;
    setSaving(true);
    const saved = { ...graph, name: safeGraphName(graph.name) };
    try {
      await request("save_graph", { doc: saved });
      if (epoch === documentEpoch.current) {
        // A save acknowledges only the submitted document. Preserve any edits
        // made during IPC, including changes to the workflow name or viewport.
        const unchanged = graphRef.current === graph;
        if (unchanged) {
          setGraph(saved);
          savedKey.current = documentKey(saved);
        }
        setDirty(!unchanged);
        writeLayout({
          lastGraph: { workspace: library?.workspace ?? "", name: saved.name },
        });
        setNotice(
          unchanged
            ? `Saved ${saved.name}`
            : `Saved ${saved.name}; newer edits are unsaved`,
        );
      }
      await refreshLibrary();
    } finally {
      saveInFlight.current = false;
      setSaving(false);
    }
  }
  function navigateChildren(depth = 0) {
    childNavigation.current += 1;
    setChildren((items) => items.slice(0, depth));
  }
  async function openChild(id: string) {
    if (children.length >= 8)
      throw new Error("Workflow navigation is limited to 8 levels.");
    const node = activeGraph.nodes.find((node) => node.id === id);
    const config = node && typedData(node);
    if (!node || config?.type !== "workflow") return;
    const path = childPath ? `${childPath}/${id}` : id;
    const navigation = ++childNavigation.current;
    const root = rootNavigation.current;
    const stillCurrent = () =>
      epoch === documentEpoch.current &&
      navigation === childNavigation.current &&
      root === rootNavigation.current;
    const snapshot = activeChild?.snapshot || (!activeChild && snapshotContext);
    const call = callAt(currentRun, path);
    let doc: GraphDoc | undefined;
    try {
      doc = snapshot
        ? captured[path] || call?.graph
        : await request("load_graph", { name: config.graph });
    } catch (error) {
      if (!stillCurrent()) return;
      throw error;
    }
    if (!stillCurrent()) return;
    if (!doc)
      throw new Error(
        "No prepared snapshot is available for this child in the displayed run.",
      );
    setChildren((items) => [
      ...items,
      {
        path,
        label: nodeLabel(node),
        graph: normalizeGraph(doc),
        selected: firstSelection(doc, call?.run),
        snapshot: !!snapshot,
      },
    ]);
  }
  async function startRun(prepared: RunForm) {
    if (
      startingRun.current ||
      pendingRun.current ||
      runtimeRef.current?.running
    )
      return;
    startingRun.current = true;
    setBusy(true);
    try {
      let started: { run_id: string };
      try {
        started = await request("start_run", {
          doc: prepared.graph,
          targets: prepared.targets,
          inputs: prepared.inputs,
          fresh: prepared.fresh,
        });
      } catch (error) {
        if (prepared.fields.length) {
          setForm({ ...prepared, error: messageOf(error) });
          return;
        }
        throw error;
      }
      // The service accepted this execution. A failed status read must not
      // restore the submission form and allow the same run to start again.
      pendingRun.current = started.run_id;
      setPendingRunId(started.run_id);
      setAttachedRun(started.run_id);
      setForm(null);
      setShownRun(null);
      setHistorical(false);
      setPanel("run");
      setNotice("Workflow is running");
      await readRuntime((state) => state?.run_id === started.run_id);
    } finally {
      startingRun.current = false;
      setBusy(false);
    }
  }
  async function prepareRun(fresh = false, node?: string) {
    if (
      readOnly ||
      occupied ||
      preparingRun.current ||
      startingRun.current ||
      pendingRun.current
    )
      return;
    preparingRun.current = true;
    rootNavigation.current += 1;
    setBusy(true);
    const requestedGraph = graph;
    const requestedEpoch = documentEpoch.current;
    try {
      const prepared = await request("prepare_run", {
        doc: requestedGraph,
        ...(node ? { targets: [node] } : {}),
      });
      if (requestedEpoch !== documentEpoch.current) return;
      if (graphRef.current !== requestedGraph)
        throw new Error(
          "The workflow changed while preparing. Run it again to use the latest edits.",
        );
      const inputs = { ...prepared.inputs };
      for (const field of prepared.fields) {
        const previousInput = history[0]?.inputs?.[field.name];
        if (previousInput !== undefined) inputs[field.name] = previousInput;
      }
      const form: RunForm = {
        ...prepared,
        inputs,
        graph: structuredClone(requestedGraph),
        fresh,
      };
      setPanel("run");
      if (prepared.fields.length) {
        setForm(form);
        setNotice("Fill in the run inputs");
      } else await startRun(form);
    } finally {
      preparingRun.current = false;
      setBusy(false);
    }
  }
  async function cancelRun() {
    const runId =
      pendingRun.current || (runtime?.running ? runtime.run_id : null);
    if (runId) {
      await request("cancel_run", { run_id: runId });
      if (
        pendingRun.current === runId ||
        (runtimeRef.current?.run_id === runId && runtimeRef.current.running)
      )
        setNotice("Cancelling…");
    }
  }
  async function reviewDecision(decision: "apply" | "reject") {
    if (!runtime?.review) return;
    const runId = runtime.run_id;
    await request("review", {
      run_id: runtime.run_id,
      review_id: runtime.review.id,
      decision,
    });
    if (runtimeRef.current?.run_id !== runId) return;
    await readRuntime(
      (state) =>
        state?.run_id === runId && runtimeRef.current?.run_id === runId,
    );
  }
  async function usePrompt(name: string) {
    const config = activeNode && typedData(activeNode);
    if (readOnly || !activeNode || config?.type !== "agent")
      throw new Error("Select an Agent node to load a prompt.");
    const requestId = ++promptRequest.current;
    const selection = promptSelection.current;
    const stillSelected = () =>
      requestId === promptRequest.current &&
      selection.epoch === documentEpoch.current &&
      promptSelection.current.selected === activeNode.id &&
      promptSelection.current.childPath === selection.childPath;
    let prompt: string;
    try {
      prompt = await request("load_prompt", { name });
    } catch (error) {
      if (stillSelected()) throw error;
      return;
    }
    if (!stillSelected()) return;
    const current = graphRef.current.nodes.find(
      (node) => node.id === activeNode.id,
    );
    const currentConfig = current && typedData(current);
    if (
      !current ||
      currentConfig?.type !== "agent" ||
      currentConfig.prompt !== config.prompt
    )
      return;
    updateNode({ ...current, data: { ...currentConfig, prompt } });
    setNotice(`Loaded ${name}`);
  }
  async function removeLibrary(
    method: "delete_kind" | "delete_prompt" | "delete_preset",
    args: { name: string; type?: "agent" | "database" },
    name: string,
  ) {
    if (
      !(await confirmAction(`Delete ${name} from the library?`, {
        title: "Delete from library",
        confirm: "Delete",
      }))
    )
      return;
    if (method === "delete_preset") {
      if (!args.type) throw new Error("Choose a preset type to delete.");
      await request(method, { name: args.name, type: args.type });
    } else await request(method, { name: args.name });
    await refreshLibrary();
    setNotice(`Deleted ${name}`);
  }
  async function deleteGraph(name: string) {
    if (occupied) return;
    if (
      !(await confirmAction(
        `Delete the workflow ${name} and its run history?`,
        { title: "Delete workflow", confirm: "Delete" },
      ))
    )
      return;
    await request("delete_graph", { name });
    await refreshLibrary();
    if (graphRef.current.name === name) {
      // The open document outlives its file; saving writes it again.
      savedKey.current = null;
      setDirty(true);
      setNotice(`Deleted ${name}; the open copy is unsaved`);
    } else setNotice(`Deleted ${name}`);
  }
  async function renameGraph(name: string) {
    if (occupied) return;
    const to = await promptFor(`Rename ${name} to:`, {
      title: "Rename workflow",
      confirm: "Rename",
      label: "New name",
      initial: name,
    });
    if (to === null || safeGraphName(to) === name) return;
    const result = await request("rename_graph", {
      from: name,
      to: safeGraphName(to),
    });
    await refreshLibrary();
    if (graphRef.current.name === name) {
      const renamed = { ...graphRef.current, name: result.name };
      setGraph(renamed);
      graphRef.current = renamed;
      if (!dirtyRef.current) savedKey.current = documentKey(renamed);
      writeLayout({
        lastGraph: { workspace: library?.workspace ?? "", name: result.name },
      });
    }
    setNotice(`Renamed ${name} to ${result.name}`);
  }
  async function duplicateGraph(name: string) {
    if (occupied) return;
    const to = await promptFor(`Save a copy of ${name} as:`, {
      title: "Duplicate workflow",
      confirm: "Duplicate",
      label: "Copy name",
      initial: `${name}-copy`,
    });
    if (to === null) return;
    const result = await request("duplicate_graph", {
      from: name,
      to: safeGraphName(to),
    });
    await refreshLibrary();
    setNotice(`Saved ${result.name}`);
  }
  async function installExamples() {
    if (occupied || readOnly) return;
    const result = await request("install_examples");
    await refreshLibrary();
    setNotice(
      result.installed.length
        ? `Installed ${result.installed.length} example workflow${result.installed.length === 1 ? "" : "s"}`
        : "The example workflows were already installed",
    );
    await openGraph(result.installed[0] ?? EXAMPLE_NAMES[0] ?? "");
  }
  /** Select the root node a run feed entry belongs to and bring it into view. */
  function showOnCanvas(path: string) {
    const id = path.split("/")[0] ?? "";
    if (!graph.nodes.some((node) => node.id === id)) return;
    if (children.length) navigateChildren();
    setSelected(id);
    setSelection([id]);
    setFocusRequest((current) => ({ id, seq: (current?.seq ?? 0) + 1 }));
  }
  function resize(side: "left" | "right", event: React.PointerEvent) {
    event.preventDefault();
    const start = event.clientX;
    // Start from the rendered width when a smaller window has constrained it.
    const width =
      side === "left"
        ? leftWidth
        : (inspectorColumn.current?.getBoundingClientRect().width ??
          rightWidth);
    const workspaceWidth =
      event.currentTarget.parentElement?.clientWidth ?? window.innerWidth;
    // Leave 240 px for the canvas and 8 px for the two panel dividers.
    const maximum =
      side === "left" ? 380 : Math.max(300, workspaceWidth - leftWidth - 248);
    let latest = width;
    const move = (e: PointerEvent) => {
      latest = Math.max(
        side === "left" ? 190 : 300,
        Math.min(
          maximum,
          width + (side === "left" ? e.clientX - start : start - e.clientX),
        ),
      );
      (side === "left" ? setLeftWidth : setRightWidth)(latest);
    };
    const stop = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", stop);
      window.removeEventListener("pointercancel", stop);
      writeLayout(
        side === "left" ? { leftWidth: latest } : { rightWidth: latest },
      );
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", stop);
    window.addEventListener("pointercancel", stop);
  }
  closeRequested.current = () => {
    if (closePrompt.current) return;
    if (!dirtyRef.current) {
      void quitApp();
      return;
    }
    closePrompt.current = true;
    void (async () => {
      try {
        const running = !!runtimeRef.current?.running;
        const choice = await choose(
          running
            ? "This workflow has unsaved changes, and a run is in progress. Quitting cancels the run."
            : "This workflow has unsaved changes.",
          {
            title: "Quit uapz?",
            confirm: "Quit without saving",
            ...(readOnly ? {} : { alternative: "Save and quit" }),
          },
        );
        if (choice === "cancel") return;
        if (choice === "alternative") {
          try {
            await save();
          } catch (error) {
            setError(messageOf(error));
            return;
          }
        }
        await quitApp();
      } finally {
        closePrompt.current = false;
      }
    })();
  };
  function toggleLibrary() {
    setLibraryOpen((open) => {
      writeLayout({ libraryOpen: !open });
      return !open;
    });
  }
  function toggleInspector() {
    setInspectorOpen((open) => {
      writeLayout({ inspectorOpen: !open });
      return !open;
    });
  }
  const shortcuts = {
    save,
    prepareRun,
    newGraph,
    add: () => addNode(createData(addType)),
    undo,
    redo,
    copy: () => copyNodes(),
    paste: () => pasteNodes(),
    duplicate: () => duplicateNodes(),
    toggleLibrary,
    toggleInspector,
  };
  const keyboard = useRef(shortcuts);
  keyboard.current = shortcuts;
  useEffect(() => {
    const handler = (event: KeyboardEvent) => {
      if (document.querySelector("dialog[open]")) return;
      // Text fields keep their native shortcuts: undo, copy and paste there
      // act on the text, not the graph.
      const inField =
        event.target instanceof Element &&
        !!event.target.closest(
          'input,textarea,select,[contenteditable="true"]',
        );
      if (event.key === "?" && !inField && !event.metaKey && !event.ctrlKey) {
        event.preventDefault();
        setShortcutsOpen(true);
        return;
      }
      if (!(event.metaKey || event.ctrlKey)) return;
      const key = event.key.toLowerCase();
      if (key === "s") {
        event.preventDefault();
        void action(keyboard.current.save);
      }
      if (event.key === "Enter") {
        event.preventDefault();
        void action(() => keyboard.current.prepareRun(event.shiftKey));
      }
      if (event.shiftKey && key === "n") {
        event.preventDefault();
        keyboard.current.newGraph();
      }
      if (inField) return;
      if (!event.shiftKey && key === "n") {
        event.preventDefault();
        keyboard.current.add();
      }
      if (key === "z") {
        event.preventDefault();
        if (event.shiftKey) keyboard.current.redo();
        else keyboard.current.undo();
      }
      if (key === "c" && keyboard.current.copy()) event.preventDefault();
      if (key === "v" && keyboard.current.paste()) event.preventDefault();
      if (key === "d") {
        event.preventDefault();
        keyboard.current.duplicate();
      }
      if (key === "b") {
        event.preventDefault();
        keyboard.current.toggleLibrary();
      }
      if (key === "j") {
        event.preventDefault();
        keyboard.current.toggleInspector();
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [action]);

  const matches = (category: string, name: string) =>
    `${category} ${name}`.toLowerCase().includes(query.toLowerCase().trim());
  const libraryRow = (
    key: string,
    label: string,
    onClick: () => void,
    color?: string,
    onDelete?: () => void,
    disabled = readOnly,
    trailing: "add" | "open" = "add",
    drag?: LibraryItem,
    actions?: { label: string; icon: ReactNode; run: () => void }[],
  ) => (
    <div className="library-row" key={key}>
      <button
        className="library-item"
        onClick={onClick}
        disabled={disabled}
        title={drag ? `${label} · drag onto the canvas` : label}
        draggable={!!drag && !disabled}
        onDragStart={(event) => {
          if (!drag) return;
          event.dataTransfer.setData(LIBRARY_DRAG_TYPE, JSON.stringify(drag));
          event.dataTransfer.effectAllowed = "copy";
        }}
      >
        {color ? <i style={{ background: color }} /> : <FolderOpen size={15} />}
        <span>{label}</span>
        {trailing === "open" ? (
          <ArrowUpRight size={12} className="row-action" />
        ) : (
          <Plus size={12} className="row-action" />
        )}
      </button>
      {(onDelete || actions?.length) && (
        <span className="row-actions">
          {actions?.map((item) => (
            <button
              key={item.label}
              className="icon-button"
              aria-label={item.label}
              title={item.label}
              onClick={item.run}
              disabled={disabled}
            >
              {item.icon}
            </button>
          ))}
          {onDelete && (
            <button
              className="icon-button delete-library"
              aria-label={`Delete ${label}`}
              title={`Delete ${label}`}
              onClick={onDelete}
              disabled={disabled}
            >
              <Trash2 size={13} />
            </button>
          )}
        </span>
      )}
    </div>
  );
  const groups: { label: string; items: ReactNode[]; open?: boolean }[] =
    library
      ? [
          {
            label: "Workflows",
            open: true,
            items: library.graphs
              .filter((name) => matches("workflows", name))
              .map((name) =>
                libraryRow(
                  `graph:${name}`,
                  name,
                  () => void action(() => openGraph(name)),
                  undefined,
                  () => void action(() => deleteGraph(name)),
                  occupied || readOnly,
                  "open",
                  undefined,
                  [
                    {
                      label: `Rename ${name}`,
                      icon: <Pencil size={13} />,
                      run: () => void action(() => renameGraph(name)),
                    },
                    {
                      label: `Duplicate ${name}`,
                      icon: <Files size={13} />,
                      run: () => void action(() => duplicateGraph(name)),
                    },
                  ],
                ),
              ),
          },
          {
            label: "Nodes",
            open: true,
            items: (
              [...Object.keys(labels), "database-write"] as Exclude<
                CreateType,
                "blank"
              >[]
            )
              .filter((type) =>
                matches(
                  "nodes",
                  type === "database-write" ? "Database (write)" : labels[type],
                ),
              )
              .map((type) =>
                libraryRow(
                  type,
                  type === "database-write" ? "Database (write)" : labels[type],
                  () => addFromLibrary({ kind: "type", type }),
                  nodeColor({ id: "", x: 0, y: 0, data: createData(type) }),
                  undefined,
                  undefined,
                  "add",
                  { kind: "type", type },
                ),
              ),
          },
          {
            label: "Custom nodes",
            items: library.kinds
              .filter((kind) => matches("custom nodes", kind.name))
              .map((kind) =>
                libraryRow(
                  `kind:${kind.name}`,
                  kind.name,
                  () => addFromLibrary({ kind: "kind", ...kind }),
                  kind.color,
                  () =>
                    void action(() =>
                      removeLibrary(
                        "delete_kind",
                        { name: kind.name },
                        kind.name,
                      ),
                    ),
                  undefined,
                  "add",
                  { kind: "kind", name: kind.name, color: kind.color },
                ),
              ),
          },
          {
            label: "Agents",
            items: library.agents
              .filter((agent) => matches("agents", agent.name))
              .map((agent) =>
                libraryRow(
                  `agent:${agent.name}`,
                  agent.name,
                  () => addFromLibrary({ kind: "agent", name: agent.name }),
                  "#8b5cf6",
                  () =>
                    void action(() =>
                      removeLibrary(
                        "delete_preset",
                        { type: "agent", name: agent.name },
                        agent.name,
                      ),
                    ),
                  undefined,
                  "add",
                  { kind: "agent", name: agent.name },
                ),
              ),
          },
          {
            label: "Prompts",
            items: library.prompts
              .filter((name) => matches("prompts", name))
              .map((name) =>
                libraryRow(
                  `prompt:${name}`,
                  name,
                  () => void action(() => usePrompt(name)),
                  "#a78bfa",
                  () =>
                    void action(() =>
                      removeLibrary("delete_prompt", { name }, name),
                    ),
                ),
              ),
          },
          {
            label: "Databases",
            items: library.databases
              .filter((database) => matches("databases", database.name))
              .map((database) =>
                libraryRow(
                  `database:${database.name}`,
                  database.name,
                  () =>
                    addFromLibrary({ kind: "database", name: database.name }),
                  database.config.read_only ? "#0ea5e9" : "#ef4444",
                  () =>
                    void action(() =>
                      removeLibrary(
                        "delete_preset",
                        { type: "database", name: database.name },
                        database.name,
                      ),
                    ),
                  undefined,
                  "add",
                  { kind: "database", name: database.name },
                ),
              ),
          },
          {
            label: "Scripts",
            items: library.scripts
              .filter((name) => matches("scripts", name))
              .map((name) =>
                libraryRow(
                  `script:${name}`,
                  name,
                  () => addFromLibrary({ kind: "script", name }),
                  "#f59e0b",
                  undefined,
                  undefined,
                  "add",
                  { kind: "script", name },
                ),
              ),
          },
          {
            label: "Data",
            items: library.data
              .filter((name) => matches("data", name))
              .map((name) =>
                libraryRow(
                  `data:${name}`,
                  name,
                  () => addFromLibrary({ kind: "data", name }),
                  "#22c55e",
                  undefined,
                  undefined,
                  "add",
                  { kind: "data", name },
                ),
              ),
          },
        ]
      : [];

  return (
    <div
      className="workspace"
      style={
        {
          "--library-width": libraryOpen ? `${leftWidth}px` : "0px",
          "--inspector-width": `${rightWidth}px`,
        } as CSSProperties
      }
    >
      <header className="project-header">
        <div className="wordmark">
          <Boxes size={19} />
          uapz
        </div>
        <div className="project-title">
          <input
            aria-label="Workflow name"
            value={graph.name}
            disabled={readOnly || running || !!form}
            onChange={(event) =>
              updateGraph({ ...graph, name: event.target.value })
            }
          />
          <span className="save-state">
            {dirty
              ? "Unsaved changes"
              : graph.nodes.length
                ? "Saved workflow"
                : "New workflow"}
            {safeGraphName(graph.name) !== graph.name.trim() && (
              <span
                className="save-as"
                title="Names become file stems: letters, digits, dots, dashes and underscores."
              >
                {" "}
                · saves as {safeGraphName(graph.name)}
              </span>
            )}
          </span>
        </div>
        <div className="header-actions">
          {readOnly && (
            <button onClick={() => navigateChildren()}>
              <ArrowLeft size={14} />
              Return to {occupied || activeChild.snapshot ? "run" : "workflow"}
            </button>
          )}
          <button onClick={newGraph} disabled={occupied}>
            New
          </button>
          <button
            onClick={() => void action(save)}
            disabled={readOnly || !library || saving}
          >
            <Save size={14} />
            Save
          </button>
          {running ? (
            <button
              className="danger-button"
              onClick={() => void action(cancelRun)}
              disabled={runtime?.cancelling}
            >
              <Square size={12} />
              {runtime?.cancelling ? "Cancelling…" : "Cancel run"}
            </button>
          ) : (
            <>
              <button
                onClick={() => void action(() => prepareRun(true))}
                disabled={
                  occupied ||
                  readOnly ||
                  !graph.nodes.length ||
                  !library ||
                  problemCount > 0
                }
                aria-label="Run workflow fresh"
                title="Run the whole workflow without reusing cached outputs (⌘⇧↵)"
              >
                Run fresh
              </button>
              <button
                className="primary"
                onClick={() => void action(() => prepareRun())}
                disabled={
                  occupied ||
                  readOnly ||
                  !graph.nodes.length ||
                  !library ||
                  problemCount > 0
                }
                aria-label={
                  problemCount
                    ? `Run workflow, ${problemCount} problem${problemCount === 1 ? "" : "s"} to fix first`
                    : undefined
                }
                title={
                  problemCount
                    ? `${problemCount} problem${problemCount === 1 ? "" : "s"} to fix before running. Select a marked node to see them.`
                    : "Run every end node of the workflow (⌘↵)"
                }
              >
                <Play size={14} />
                Run workflow
                {problemCount > 0 && (
                  <span className="problem-badge" aria-hidden="true">
                    {problemCount}
                  </span>
                )}
              </button>
            </>
          )}
        </div>
      </header>
      {error && (
        <div className="error-banner" role="alert">
          <span>{error}</span>
          <button aria-label="Dismiss error" onClick={() => setError(null)}>
            <X size={16} />
          </button>
        </div>
      )}
      <main className="workspace-body">
        <aside className="library" hidden={!libraryOpen}>
          <div className="library-heading">
            <strong>Library</strong>
            <button
              className="icon-button"
              aria-label="Refresh library"
              onClick={() =>
                void action(async () => {
                  await refreshLibrary();
                  setNotice("Library refreshed");
                })
              }
            >
              <RefreshCw size={15} />
            </button>
          </div>
          <label className="search">
            <Search size={15} />
            <input
              aria-label="Search library"
              placeholder="Search library…"
              value={query}
              onChange={(event) => setQuery(event.target.value)}
            />
            {query && (
              <button aria-label="Clear search" onClick={() => setQuery("")}>
                <X size={13} />
              </button>
            )}
          </label>
          {readOnly && (
            <p className="library-hint">Return to the parent to add nodes.</p>
          )}
          <div className="library-sections">
            {!library && <p className="muted">Connecting to your workspace…</p>}
            {groups
              .filter((group) => !query || group.items.length)
              .map((group) => (
                <details
                  key={`${group.label}:${!!query}`}
                  open={!!query || group.open}
                >
                  <summary>
                    {group.label}
                    <span>{group.items.length}</span>
                  </summary>
                  {group.items.length ? (
                    group.items
                  ) : (
                    <div className="library-empty">
                      <p>No {group.label.toLowerCase()} saved yet.</p>
                      {group.label === "Workflows" && !readOnly && (
                        <button
                          type="button"
                          onClick={() => void action(installExamples)}
                        >
                          Install example workflows
                        </button>
                      )}
                    </div>
                  )}
                </details>
              ))}
            {query && groups.every((group) => !group.items.length) && (
              <p className="library-empty">No matching library items.</p>
            )}
          </div>
          <button
            className="kind-button"
            onClick={() => setKindEditor((value) => !value)}
            disabled={readOnly}
          >
            <Plus size={14} />
            Custom node kind
          </button>
          {kindEditor && (
            <form
              className="kind-form"
              onSubmit={(event) => {
                event.preventDefault();
                void action(async () => {
                  await request("save_kind", {
                    kind: { name: kindName, color: kindColor },
                  });
                  setKindEditor(false);
                  setKindName("");
                  await refreshLibrary();
                });
              }}
            >
              <input
                aria-label="Kind name"
                placeholder="Kind name"
                value={kindName}
                onChange={(event) => setKindName(event.target.value)}
                required
              />
              <input
                aria-label="Kind color"
                type="color"
                value={kindColor}
                onChange={(event) => setKindColor(event.target.value)}
              />
              <button type="submit">Save kind</button>
            </form>
          )}
          <div className="workspace-path" title={library?.workspace}>
            <FolderOpen size={13} />
            <span>{library?.workspace || "Local workspace"}</span>
          </div>
        </aside>
        <div
          className="panel-divider"
          role="separator"
          aria-label="Resize library"
          hidden={!libraryOpen}
          onPointerDown={(event) => resize("left", event)}
        />
        <section className="canvas-column">
          <div className="canvas-toolbar">
            <button
              className="icon-button panel-toggle"
              aria-label={libraryOpen ? "Hide library" : "Show library"}
              aria-pressed={libraryOpen}
              title={`${libraryOpen ? "Hide" : "Show"} the library (⌘B)`}
              onClick={toggleLibrary}
            >
              <PanelLeft size={15} />
            </button>
            <nav aria-label="Workflow breadcrumbs">
              <button onClick={() => navigateChildren()}>
                {graph.name || "untitled"}
              </button>
              {children.map((child, index) => (
                <span key={child.path}>
                  <ChevronRight size={13} />
                  <button onClick={() => navigateChildren(index + 1)}>
                    {child.label}
                  </button>
                </span>
              ))}
            </nav>
            {activeChild && (
              <span className="provenance">
                {activeChild.snapshot ? "Run snapshot" : "Saved workflow"} ·
                Read only
              </span>
            )}
            <div className="canvas-tools">
              {!readOnly && (
                <>
                  <select
                    aria-label="New node type"
                    value={addType}
                    onChange={(event) =>
                      setAddType(event.target.value as CreateType)
                    }
                  >
                    <option value="blank">Blank</option>
                    {Object.entries(labels).map(([type, label]) => (
                      <option value={type} key={type}>
                        {label}
                      </option>
                    ))}
                    <option value="database-write">Database (write)</option>
                  </select>
                  <button onClick={() => addNode(createData(addType))}>
                    <Plus size={14} />
                    Add node
                  </button>
                </>
              )}
              {!readOnly && (
                <>
                  <button
                    className="icon-button"
                    aria-label="Undo"
                    title="Undo (⌘Z)"
                    disabled={!undoStack.current.length}
                    data-history={historyVersion}
                    onClick={undo}
                  >
                    <Undo2 size={14} />
                  </button>
                  <button
                    className="icon-button"
                    aria-label="Redo"
                    title="Redo (⌘⇧Z)"
                    disabled={!redoStack.current.length}
                    onClick={redo}
                  >
                    <Redo2 size={14} />
                  </button>
                </>
              )}
              <button
                onClick={() => setFrameSignal((signal) => signal + 1)}
                disabled={!activeGraph.nodes.length}
              >
                Fit graph
              </button>
              <span className="graph-count">
                {activeGraph.nodes.length} nodes · {activeGraph.edges.length}{" "}
                connections
              </span>
              <button
                className="icon-button panel-toggle"
                aria-label={inspectorOpen ? "Hide inspector" : "Show inspector"}
                aria-pressed={inspectorOpen}
                title={`${inspectorOpen ? "Hide" : "Show"} the inspector (⌘J)`}
                onClick={toggleInspector}
              >
                <PanelRight size={15} />
              </button>
            </div>
          </div>
          <div className="canvas-host" ref={canvasHost}>
            <Canvas
              key={
                activeChild?.path || `root:${library?.workspace || ""}:${epoch}`
              }
              graph={activeGraph}
              selected={activeSelected}
              selection={readOnly ? [] : selection}
              onSelect={selectNode}
              onSelectionChange={changeSelection}
              onChange={updateGraph}
              onViewportChange={(view) => {
                if (readOnly)
                  setChildren((items) =>
                    items.map((item, index) =>
                      index === items.length - 1
                        ? { ...item, graph: { ...item.graph, view } }
                        : item,
                    ),
                  );
                else setGraph((current) => ({ ...current, view }));
              }}
              onOpenWorkflow={(id) => void action(() => openChild(id))}
              onAdd={(position) => addNode(createData(addType), position)}
              onDropLibrary={(payload, position) => {
                const item = parseLibraryItem(payload);
                if (item) addFromLibrary(item, position);
              }}
              onRunNode={(id, fresh) =>
                void action(() => prepareRun(fresh, id))
              }
              onCopy={(ids) => copyNodes(ids)}
              onDuplicate={(ids) => duplicateNodes(ids)}
              onPaste={(position) => pasteNodes(position)}
              canPaste={clipboardSize > 0}
              readOnly={readOnly}
              workflowGraphs={workflowGraphs}
              marks={marks}
              problems={problems}
              focus={focusRequest}
              frameSignal={frameSignal}
            />
            {!activeGraph.nodes.length && (
              <div className="empty-canvas">
                <Boxes size={28} />
                <h1>
                  {readOnly ? "This workflow is empty" : "Build a workflow"}
                </h1>
                <p>
                  {readOnly
                    ? "The child graph has no nodes. Return to the parent or open another workflow."
                    : "Add a node from the library or double-click the canvas, then connect its output to the next step."}
                </p>
                {!readOnly && (
                  <div className="empty-actions">
                    <button onClick={() => addNode(defaultNodeData("input"))}>
                      Start with a Run input
                    </button>
                    {library &&
                      !EXAMPLE_NAMES.every((name) =>
                        library.graphs.includes(name),
                      ) && (
                        <button
                          onClick={() => void action(installExamples)}
                          title="Copy the bundled example workflows into this workspace and open the first one"
                        >
                          Install example workflows
                        </button>
                      )}
                  </div>
                )}
              </div>
            )}
          </div>
        </section>
        <div
          className="panel-divider"
          role="separator"
          aria-label="Resize inspector"
          hidden={!inspectorOpen}
          onPointerDown={(event) => resize("right", event)}
        />
        <aside
          className="inspector-column"
          ref={inspectorColumn}
          hidden={!inspectorOpen}
        >
          <div
            className="workspace-panel-tabs"
            role="tablist"
            aria-label="Workspace panel"
          >
            {(["inspector", "run"] as const).map((mode, index) => (
              <button
                key={mode}
                id={`workspace-tab-${mode}`}
                role="tab"
                aria-selected={panel === mode}
                aria-controls={`workspace-panel-${mode}`}
                tabIndex={panel === mode ? 0 : -1}
                onClick={() => setPanel(mode)}
                onKeyDown={(event) => {
                  if (
                    !["ArrowLeft", "ArrowRight", "Home", "End"].includes(
                      event.key,
                    )
                  )
                    return;
                  event.preventDefault();
                  const next =
                    event.key === "Home"
                      ? "inspector"
                      : event.key === "End"
                        ? "run"
                        : index === 0
                          ? "run"
                          : "inspector";
                  setPanel(next);
                  document.getElementById(`workspace-tab-${next}`)?.focus();
                }}
              >
                {mode === "inspector" ? "Node inspector" : "Run output"}
                {mode === "run" && running && runMatches && (
                  <span className="status-dot active" />
                )}
              </button>
            ))}
          </div>
          {!readOnly && form && (
            <section className="run-form" aria-label="Run inputs">
              <div className="section-title">
                <strong>Run inputs</strong>
                <button
                  className="icon-button"
                  aria-label="Close run inputs"
                  onClick={() => setForm(null)}
                >
                  <X size={15} />
                </button>
              </div>
              <p>Values for this execution</p>
              <form
                onSubmit={(event) => {
                  event.preventDefault();
                  void action(() => startRun(form));
                }}
              >
                {form.fields.map((field) => (
                  <label key={field.name}>
                    {field.name}
                    <small>
                      {field.config.kind}
                      {field.config.required ? " · required" : " · optional"}
                    </small>
                    {field.config.kind === "boolean" ? (
                      <select
                        aria-label={`Run input ${field.name}`}
                        value={form.inputs[field.name] || ""}
                        onChange={(event) =>
                          setForm({
                            ...form,
                            inputs: {
                              ...form.inputs,
                              [field.name]: event.target.value,
                            },
                          })
                        }
                      >
                        <option value="">Choose…</option>
                        <option value="true">true</option>
                        <option value="false">false</option>
                      </select>
                    ) : field.config.kind === "file" ? (
                      <ComboInput
                        id={`run-input-${field.name}`}
                        label={`Run input ${field.name}`}
                        value={form.inputs[field.name] || ""}
                        choices={library?.data || []}
                        placeholder="Choose or type a path inside data/"
                        onChange={(value) =>
                          setForm({
                            ...form,
                            inputs: { ...form.inputs, [field.name]: value },
                          })
                        }
                      />
                    ) : (
                      <textarea
                        aria-label={`Run input ${field.name}`}
                        value={form.inputs[field.name] || ""}
                        rows={
                          field.config.kind === "text" ||
                          field.config.kind === "json"
                            ? 3
                            : 1
                        }
                        placeholder={field.config.default}
                        onChange={(event) =>
                          setForm({
                            ...form,
                            inputs: {
                              ...form.inputs,
                              [field.name]: event.target.value,
                            },
                          })
                        }
                      />
                    )}
                  </label>
                ))}
                {form.error && (
                  <p className="field-error" role="alert">
                    {form.error}
                  </p>
                )}
                <button className="primary" type="submit" disabled={busy}>
                  {busy ? "Starting…" : "Start run"}
                </button>
              </form>
            </section>
          )}
          {!readOnly && runtime?.review && (
            <ReviewPanel
              runtime={runtime}
              onDecision={(decision) =>
                void action(() => reviewDecision(decision))
              }
              onCancel={() => void action(cancelRun)}
            />
          )}
          <div
            className="workspace-panel-body"
            role="tabpanel"
            id="workspace-panel-run"
            aria-labelledby="workspace-tab-run"
            hidden={panel !== "run"}
          >
            <RunPanel
              graph={graph}
              runtime={runMatches && !historical ? runtime : null}
              run={currentRun}
              onSelectNode={showOnCanvas}
            />
          </div>
          <div
            className="workspace-panel-body"
            role="tabpanel"
            id="workspace-panel-inspector"
            aria-labelledby="workspace-tab-inspector"
            hidden={panel !== "inspector"}
          >
            <Inspector
              scopeKey={`${epoch}:${activeChild?.path || ""}:${historical ? currentRun?.id || "" : ""}`}
              graph={activeGraph}
              node={activeNode}
              readOnly={readOnly}
              onChange={updateNode}
              onGraphChange={updateGraph}
              workflowGraphs={workflowGraphs}
              library={library || undefined}
              issues={
                !readOnly && validation?.key === validationKey
                  ? validation.issues
                  : undefined
              }
              results={
                readOnly || historical ? childRun?.nodes : remembered.nodes
              }
              activeRun={childRun}
              history={readOnly ? (childRun ? [childRun] : []) : history}
              running={running && runMatches}
              log={liveLog}
              stream={liveStream}
              onRun={(fresh) =>
                void action(() => prepareRun(fresh, selected || undefined))
              }
              onDelete={() => {
                if (selected && !readOnly) {
                  updateGraph(deleteNodes(graph, [selected]));
                  setSelected(null);
                  setSelection([]);
                }
              }}
              onOpenHistory={(id) => {
                if (occupied || readOnly) return;
                const run = history.find((run) => run.id === id);
                if (run) {
                  historicalRef.current = true;
                  setShownRun(run);
                  setHistorical(true);
                  setPanel("run");
                  setNotice(`Showing run ${run.id}`);
                }
              }}
              onOpenWorkflow={(id) => void action(() => openChild(id))}
              onLibraryChanged={() =>
                void action(async () => {
                  await refreshLibrary();
                })
              }
              outputContext={
                outputRun
                  ? {
                      graph: outputRun.graph,
                      runId: outputRun.id,
                      ...(childPath ? { callPath: childPath } : {}),
                    }
                  : undefined
              }
            />
            {!readOnly && library && (
              <div className="history-tools">
                <button
                  disabled={running}
                  onClick={() =>
                    void action(async () => {
                      const count = await request("clear_cache");
                      setNotice(`Cleared ${count} cached outputs`);
                    })
                  }
                >
                  Clear cache
                </button>
                <button
                  disabled={running || !history.length}
                  onClick={() =>
                    void action(async () => {
                      if (
                        !(await confirmAction(
                          `Clear run history for ${graph.name}?`,
                          { title: "Clear history", confirm: "Clear" },
                        ))
                      )
                        return;
                      await request("clear_history", { graph: graph.name });
                      setHistory([]);
                      setShownRun(null);
                      setAttachedRun(null);
                      setRemembered({ nodes: {}, origins: {} });
                      setNotice("History cleared");
                    })
                  }
                >
                  Clear history
                </button>
              </div>
            )}
          </div>
        </aside>
      </main>
      {confirmation && (
        <ConfirmDialog
          title={confirmation.title}
          message={confirmation.message}
          confirm={confirmation.confirm}
          alternative={confirmation.alternative}
          input={confirmation.input}
          onDecision={(choice, value) => {
            confirmation.resolve(choice, value);
            setConfirmation(null);
          }}
        />
      )}
      {shortcutsOpen && (
        <ShortcutsDialog onClose={() => setShortcutsOpen(false)} />
      )}
      <footer className="status-bar">
        <span className={`status-dot${running ? " active" : ""}`} />
        <span role="status">
          {runtime?.review
            ? `${runtime.review.node} · Waiting for review`
            : running
              ? "Workflow is running"
              : form
                ? "Waiting for run inputs"
                : notice}
        </span>
        {readOnly && (runtime?.review || form) && (
          <button onClick={() => navigateChildren()}>
            {runtime?.review ? "Review changes" : "Return to run inputs"}
            <ChevronRight size={13} />
          </button>
        )}
        <select
          className="theme-select"
          aria-label="Theme"
          value={theme}
          onChange={(event) => {
            const next = event.target.value as Theme;
            setTheme(next);
            writeLayout({ theme: next });
          }}
        >
          <option value="system">System theme</option>
          <option value="dark">Dark</option>
          <option value="light">Light</option>
        </select>
        <button
          className="status-help"
          aria-label="Keyboard shortcuts"
          title="Every shortcut (?)"
          onClick={() => setShortcutsOpen(true)}
        >
          <CircleHelp size={13} />
          ⌘S Save · ⌘↵ Run · ⌘Z Undo · ? all shortcuts
        </button>
      </footer>
    </div>
  );
}

function ConfirmDialog({
  title,
  message,
  confirm,
  alternative,
  input,
  onDecision,
}: {
  title: string;
  message: string;
  confirm: string;
  alternative?: string;
  input?: { label: string; initial: string };
  onDecision: (choice: Choice, value: string) => void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [value, setValue] = useState(input?.initial ?? "");
  useEffect(() => {
    dialog.current?.showModal();
  }, []);
  return (
    <dialog
      ref={dialog}
      className="confirm-dialog"
      aria-labelledby="confirm-title"
      aria-describedby="confirm-message"
      onCancel={(event) => {
        event.preventDefault();
        onDecision("cancel", value);
      }}
    >
      <form
        onSubmit={(event) => {
          event.preventDefault();
          onDecision("confirm", value);
        }}
      >
        <h2 id="confirm-title">{title}</h2>
        <p id="confirm-message">{message}</p>
        {input && (
          <input
            aria-label={input.label}
            value={value}
            autoFocus
            onChange={(event) => setValue(event.target.value)}
          />
        )}
        <div>
          <button
            type="button"
            autoFocus={!input}
            onClick={() => onDecision("cancel", value)}
          >
            Cancel
          </button>
          <button
            type="submit"
            className={alternative ? "danger-button" : "primary"}
            disabled={!!input && !value.trim()}
          >
            {confirm}
          </button>
          {alternative && (
            <button
              type="button"
              className="primary"
              onClick={() => onDecision("alternative", value)}
            >
              {alternative}
            </button>
          )}
        </div>
      </form>
    </dialog>
  );
}

function ShortcutsDialog({ onClose }: { onClose: () => void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    dialog.current?.showModal();
  }, []);
  return (
    <dialog
      ref={dialog}
      className="confirm-dialog shortcuts-dialog"
      aria-labelledby="shortcuts-title"
      onCancel={(event) => {
        event.preventDefault();
        onClose();
      }}
    >
      <h2 id="shortcuts-title">Keyboard shortcuts</h2>
      <dl>
        {SHORTCUTS.map(([keys, what]) => (
          <div key={keys}>
            <dt>
              <kbd>{keys}</kbd>
            </dt>
            <dd>{what}</dd>
          </div>
        ))}
      </dl>
      <div>
        <button className="primary" autoFocus onClick={onClose}>
          Close
        </button>
      </div>
    </dialog>
  );
}

function ReviewPanel({
  runtime,
  onDecision,
  onCancel,
}: {
  runtime: RuntimeState;
  onDecision: (decision: "apply" | "reject") => void;
  onCancel: () => void;
}) {
  const [fileIndex, setFileIndex] = useState(0);
  const review = runtime.review!;
  useEffect(() => setFileIndex(0), [review.id]);
  const index = Math.max(0, Math.min(fileIndex, review.files.length - 1));
  const file = review.files[index];
  return (
    <section className="review-panel" aria-label="Review file changes">
      <div className="section-title">
        <strong>Review changes</strong>
        <span>
          {review.files.length} file{review.files.length === 1 ? "" : "s"}
        </span>
      </div>
      <p>{review.node}</p>
      {file && (
        <>
          <div className="review-files">
            <button
              type="button"
              aria-label="Previous file"
              disabled={index === 0}
              onClick={() => setFileIndex(index - 1)}
            >
              ‹
            </button>
            <select
              aria-label="Review file"
              value={index}
              onChange={(event) => setFileIndex(Number(event.target.value))}
            >
              {review.files.map((file, position) => (
                <option value={position} key={file.path}>
                  {file.path}
                </option>
              ))}
            </select>
            <button
              type="button"
              aria-label="Next file"
              disabled={index >= review.files.length - 1}
              onClick={() => setFileIndex(index + 1)}
            >
              ›
            </button>
          </div>
          <p>
            File {index + 1} of {review.files.length} ·{" "}
            {file.before_bytes ?? "New file"} → {file.after_bytes} bytes
          </p>
          <pre className="review-diff">
            {file.diff.split("\n").map((line, index) => (
              <span
                key={index}
                className={
                  line.startsWith("+")
                    ? "diff-add"
                    : line.startsWith("-")
                      ? "diff-remove"
                      : ""
                }
              >
                {line}
                {"\n"}
              </span>
            ))}
          </pre>
        </>
      )}
      <div className="review-actions">
        <button
          className="primary"
          disabled={runtime.cancelling}
          onClick={() => onDecision("apply")}
        >
          Apply changes
        </button>
        <button
          disabled={runtime.cancelling}
          onClick={() => onDecision("reject")}
        >
          Reject
        </button>
        <button disabled={runtime.cancelling} onClick={onCancel}>
          Cancel run
        </button>
      </div>
    </section>
  );
}
