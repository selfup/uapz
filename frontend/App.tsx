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
  Boxes,
  ChevronRight,
  CircleHelp,
  FolderOpen,
  Play,
  Plus,
  RefreshCw,
  Save,
  Search,
  Square,
  Trash2,
  X,
} from "lucide-react";
import Canvas from "./Canvas";
import Inspector from "./Inspector";
import RunPanel from "./RunPanel";
import { request } from "./api";
import {
  defaultNodeData,
  deleteNodes,
  nextNodeId,
  nodeColor,
  nodeLabel,
  typedData,
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

export default function App() {
  const [library, setLibrary] = useState<Library | null>(null);
  const [graph, setGraph] = useState<GraphDoc>(emptyGraph);
  const [selected, setSelected] = useState<string | null>(null);
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
    message: string;
    resolve: (accepted: boolean) => void;
  } | null>(null);
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
  const [leftWidth, setLeftWidth] = useState(246);
  const [rightWidth, setRightWidth] = useState(370);
  const inspectorColumn = useRef<HTMLElement>(null);
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
            setAttachedRun(state.run_id);
            setPanel("run");
            setDirty(true);
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

  function updateGraph(next: GraphDoc) {
    if (readOnly) return;
    setGraph(next);
    setDirty(true);
  }
  function selectNode(id: string | null) {
    if (id && !occupied) setPanel("inspector");
    if (readOnly)
      setChildren((items) =>
        items.map((item, index) =>
          index === items.length - 1 ? { ...item, selected: id } : item,
        ),
      );
    else setSelected(id);
  }
  function updateNode(node: NodeDoc) {
    if (readOnly || epoch !== documentEpoch.current) return;
    setGraph((current) => ({
      ...current,
      nodes: current.nodes.map((item) => (item.id === node.id ? node : item)),
    }));
    setDirty(true);
  }
  function addNode(
    data?: NodeData,
    position?: { x: number; y: number },
    kind?: { name: string; color: string },
  ) {
    if (readOnly) return;
    const id = nextNodeId(graph);
    const zoom = graph.view.zoom || 1;
    const node: NodeDoc = {
      id,
      x:
        position?.x ??
        (80 - graph.view.pan_x) / zoom + (graph.nodes.length % 4) * 28,
      y:
        position?.y ??
        (100 - graph.view.pan_y) / zoom + (graph.nodes.length % 4) * 28,
      data: data || {},
      ...(kind ? { kind: kind.name, color: kind.color } : {}),
    };
    if (data && !kind) node.color = nodeColor(node);
    updateGraph({ ...graph, nodes: [...graph.nodes, node] });
    setSelected(id);
    setPanel("inspector");
  }
  function confirmAction(message: string) {
    return new Promise<boolean>((resolve) =>
      setConfirmation({ message, resolve }),
    );
  }
  async function discardDraft() {
    return (
      !dirty ||
      (await confirmAction("Discard the unsaved changes to this workflow?"))
    );
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
    setGraph(emptyGraph());
    setSelected(null);
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
    setSelected(firstSelection(loaded));
    setChildren([]);
    setShownRun(null);
    setAttachedRun(null);
    setDirty(false);
    setNotice(`Opened ${name}`);
    setPanel("inspector");
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
        if (unchanged) setGraph(saved);
        setDirty(!unchanged);
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
    if (!(await confirmAction(`Delete ${name} from the library?`))) return;
    if (method === "delete_preset") {
      if (!args.type) throw new Error("Choose a preset type to delete.");
      await request(method, { name: args.name, type: args.type });
    } else await request(method, { name: args.name });
    await refreshLibrary();
    setNotice(`Deleted ${name}`);
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
    const move = (e: PointerEvent) =>
      (side === "left" ? setLeftWidth : setRightWidth)(
        Math.max(
          side === "left" ? 190 : 300,
          Math.min(
            maximum,
            width + (side === "left" ? e.clientX - start : start - e.clientX),
          ),
        ),
      );
    const stop = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", stop);
      window.removeEventListener("pointercancel", stop);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", stop);
    window.addEventListener("pointercancel", stop);
  }
  const keyboard = useRef({
    save,
    prepareRun,
    newGraph,
    add: () => addNode(createData(addType)),
  });
  keyboard.current = {
    save,
    prepareRun,
    newGraph,
    add: () => addNode(createData(addType)),
  };
  useEffect(() => {
    const handler = (event: KeyboardEvent) => {
      if (document.querySelector("dialog[open]")) return;
      if (!(event.metaKey || event.ctrlKey)) return;
      if (event.key.toLowerCase() === "s") {
        event.preventDefault();
        void action(keyboard.current.save);
      }
      if (event.key === "Enter") {
        event.preventDefault();
        void action(() => keyboard.current.prepareRun());
      }
      if (event.shiftKey && event.key.toLowerCase() === "n") {
        event.preventDefault();
        keyboard.current.newGraph();
      }
      if (
        !event.shiftKey &&
        event.key.toLowerCase() === "n" &&
        !(
          event.target instanceof Element &&
          event.target.closest('input,textarea,select,[contenteditable="true"]')
        )
      ) {
        event.preventDefault();
        keyboard.current.add();
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
  ) => (
    <div className="library-row" key={key}>
      <button
        className="library-item"
        onClick={onClick}
        disabled={disabled}
        title={label}
      >
        {color ? <i style={{ background: color }} /> : <FolderOpen size={15} />}
        <span>{label}</span>
        <Plus size={12} className="row-action" />
      </button>
      {onDelete && (
        <button
          className="icon-button delete-library"
          aria-label={`Delete ${label}`}
          onClick={onDelete}
          disabled={readOnly}
        >
          <Trash2 size={13} />
        </button>
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
                  undefined,
                  occupied,
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
                  () => addNode(createData(type)),
                  nodeColor({
                    id: "",
                    x: 0,
                    y: 0,
                    data: createData(type),
                  }),
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
                  () => addNode(undefined, undefined, kind),
                  kind.color,
                  () =>
                    void action(() =>
                      removeLibrary(
                        "delete_kind",
                        { name: kind.name },
                        kind.name,
                      ),
                    ),
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
                  () =>
                    addNode({ type: "agent", ...agent.config }, undefined, {
                      name: agent.name,
                      color: "#8b5cf6",
                    }),
                  "#8b5cf6",
                  () =>
                    void action(() =>
                      removeLibrary(
                        "delete_preset",
                        { type: "agent", name: agent.name },
                        agent.name,
                      ),
                    ),
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
                    addNode(
                      { type: "database", ...database.config },
                      undefined,
                      {
                        name: database.name,
                        color: database.config.read_only
                          ? "#0ea5e9"
                          : "#ef4444",
                      },
                    ),
                  database.config.read_only ? "#0ea5e9" : "#ef4444",
                  () =>
                    void action(() =>
                      removeLibrary(
                        "delete_preset",
                        { type: "database", name: database.name },
                        database.name,
                      ),
                    ),
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
                  () =>
                    addNode({
                      type: "script",
                      runtime: /\.(m?js|cjs)$/i.test(name)
                        ? "node"
                        : /\.py$/i.test(name)
                          ? "python"
                          : "shell",
                      path: name,
                      args: [],
                      timeout_secs: 60,
                    }),
                  "#f59e0b",
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
                  () => {
                    const ext = name.split(".").at(-1)?.toLowerCase();
                    const format =
                      ext === "csv"
                        ? "csv"
                        : ext === "tsv"
                          ? "tsv"
                          : ["json", "ndjson", "jsonl"].includes(ext || "")
                            ? "json"
                            : ["md", "markdown"].includes(ext || "")
                              ? "markdown"
                              : [
                                    "png",
                                    "jpg",
                                    "jpeg",
                                    "gif",
                                    "webp",
                                    "bmp",
                                    "tiff",
                                    "svg",
                                  ].includes(ext || "")
                                ? "image"
                                : ext === "pdf"
                                  ? "pdf"
                                  : ext === "parquet"
                                    ? "parquet"
                                    : ["txt", "text", "log"].includes(ext || "")
                                      ? "text"
                                      : "binary";
                    addNode({ type: "data", path: name, format });
                  },
                  "#22c55e",
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
          "--library-width": `${leftWidth}px`,
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
            <button
              className="primary"
              onClick={() => void action(() => prepareRun())}
              disabled={occupied || readOnly || !graph.nodes.length || !library}
            >
              <Play size={14} />
              Run workflow
            </button>
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
        <aside className="library">
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
                    <p className="library-empty">
                      No {group.label.toLowerCase()} saved yet.
                    </p>
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
          onPointerDown={(event) => resize("left", event)}
        />
        <section className="canvas-column">
          <div className="canvas-toolbar">
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
            </div>
          </div>
          <div className="canvas-host">
            <Canvas
              key={
                activeChild?.path || `root:${library?.workspace || ""}:${epoch}`
              }
              graph={activeGraph}
              selected={activeSelected}
              onSelect={selectNode}
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
              readOnly={readOnly}
              workflowGraphs={workflowGraphs}
              marks={marks}
              frameSignal={frameSignal}
            />
            {!activeGraph.nodes.length && (
              <div className="empty-canvas">
                <Boxes size={28} />
                <h1>Build a workflow</h1>
                <p>
                  Add a node from the library, then connect its output to the
                  next step.
                </p>
                <button onClick={() => addNode(defaultNodeData("input"))}>
                  Start with a Run input
                </button>
              </div>
            )}
          </div>
        </section>
        <div
          className="panel-divider"
          role="separator"
          aria-label="Resize inspector"
          onPointerDown={(event) => resize("right", event)}
        />
        <aside className="inspector-column" ref={inspectorColumn}>
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
                        placeholder={
                          field.config.kind === "file"
                            ? "Path inside data/"
                            : field.config.default
                        }
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
          message={confirmation.message}
          onDecision={(accepted) => {
            confirmation.resolve(accepted);
            setConfirmation(null);
          }}
        />
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
        <span className="status-help">
          <CircleHelp size={13} />
          ⌘S Save · ⌘↵ Run · Drag empty space to pan
        </span>
      </footer>
    </div>
  );
}

function ConfirmDialog({
  message,
  onDecision,
}: {
  message: string;
  onDecision: (accepted: boolean) => void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
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
        onDecision(false);
      }}
    >
      <h2 id="confirm-title">Confirm change</h2>
      <p id="confirm-message">{message}</p>
      <div>
        <button autoFocus onClick={() => onDecision(false)}>
          Cancel
        </button>
        <button className="primary" onClick={() => onDecision(true)}>
          Continue
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
  const file = review.files[Math.min(fileIndex, review.files.length - 1)];
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
          <select
            aria-label="Review file"
            value={fileIndex}
            onChange={(event) => setFileIndex(Number(event.target.value))}
          >
            {review.files.map((file, index) => (
              <option value={index} key={file.path}>
                {file.path}
              </option>
            ))}
          </select>
          <p>
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
