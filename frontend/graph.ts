import type {
  DataFormat,
  EdgeDoc,
  GraphDoc,
  NodeData,
  NodeDoc,
  NodeType,
} from "./types";

export type WorkflowGraphs = Record<string, GraphDoc>;
export type Port = { id: string; missing: boolean };
export type Point = { x: number; y: number };

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

export function typedData(node: NodeDoc): NodeData | undefined {
  const data: unknown = node.data;
  if (
    !data ||
    typeof data !== "object" ||
    Array.isArray(data) ||
    !Object.hasOwn(data, "type")
  )
    return undefined;
  const type = (data as Record<string, unknown>).type;
  return typeof type === "string" && Object.hasOwn(labels, type)
    ? (data as NodeData)
    : undefined;
}

export const NODE_COLORS: Record<NodeType, string> = {
  input: "#06b6d4",
  workflow: "#6366f1",
  agent: "#8b5cf6",
  database: "#0ea5e9",
  script: "#f59e0b",
  data: "#22c55e",
  directory: "#14b8a6",
  file: "#ef4444",
};

export function nodeLabel(node: NodeDoc): string {
  const data = typedData(node);
  return (
    node.name?.trim() ||
    node.kind?.trim() ||
    (data
      ? data.type === "database" && !data.read_only
        ? "Database (write)"
        : labels[data.type]
      : "Blank node")
  );
}

export function nodeColor(node: NodeDoc): string {
  if (/^#[\da-f]{6}$/i.test(node.color || "")) return node.color!;
  const data = typedData(node);
  if (data)
    return data.type === "file" || (data.type === "database" && !data.read_only)
      ? "#ef4444"
      : NODE_COLORS[data.type];
  return "#9ca3af";
}

const text = (value: unknown): string =>
  typeof value === "string" ? value : "";
const fileName = (path: unknown) =>
  text(path).trim().split("/").filter(Boolean).pop() || "no file";
const displayNames: Record<string, string> = {
  json: "JSON",
  csv: "CSV",
  jsonl: "JSONL",
  tsv: "TSV",
  pdf: "PDF",
  sqlite: "SQLite",
  duckdb: "DuckDB",
  postgres: "PostgreSQL",
  mysql: "MySQL",
  ollama: "Ollama",
  lmstudio: "LM Studio",
  llamacpp: "llama.cpp",
  openrouter: "OpenRouter",
  openai: "OpenAI",
  anthropic: "Anthropic",
  google: "Google",
  mistral: "Mistral",
  python: "Python (uv)",
  shell: "Shell",
  node: "Node.js",
};
const title = (value: unknown): string => {
  const name = text(value);
  const known = Object.hasOwn(displayNames, name)
    ? displayNames[name]
    : undefined;
  return known ?? (name.charAt(0).toUpperCase() + name.slice(1) || "Unknown");
};

export function nodeSummary(node: NodeDoc): string {
  const data = typedData(node);
  if (!data) return node.kind || "Choose a node type in the inspector";
  switch (data.type) {
    case "input":
      return `${data.kind === "file" ? "File from data/" : title(data.kind)} · ${data.required ? "required" : "optional"}`;
    case "workflow":
      return text(data.graph)
        ? `${text(data.graph)} → ${text(data.output) || "published outputs"}`
        : "choose a saved graph";
    case "agent":
      return `${title(data.provider)} · ${text(data.model).trim() || "no model"}`;
    case "database":
      return `${title(data.engine)} · ${["sqlite", "duckdb"].includes(data.engine) ? fileName(data.path) : text(data.url_env).trim() || "no connection"}${data.read_only ? "" : " · write"}`;
    case "script":
      return `${title(data.runtime)} · ${fileName(data.path)}`;
    case "data":
      return `${title(data.format)} · ${fileName(data.path)}`;
    case "directory": {
      const include = Array.isArray(data.include) ? text(data.include[0]) : "";
      return `${text(data.path).trim() || "."}${include ? ` · ${include}` : ""} · ${data.contents ? "contents" : "listing"}`;
    }
    case "file":
      return `${data.bundle ? `${text(data.path).trim() || "."}/ · sections` : fileName(data.path)} · ${data.mode === "create" ? "create only" : text(data.mode) || "unknown mode"}`;
  }
}

export function defaultNodeData(type: NodeType): NodeData {
  switch (type) {
    case "input":
      return { type, kind: "text", default: "", required: true };
    case "workflow":
      return { type, graph: "", output: "", inputs: {} };
    case "agent":
      return {
        type,
        provider: "ollama",
        endpoint: "",
        model_type: "generative",
        model: "llama3.1",
        auth: { kind: "none" },
        prompt: "",
        message: "",
        context_size: 8192,
        temperature: 0.7,
        output: "text",
      };
    case "database":
      return {
        type,
        engine: "sqlite",
        path: "",
        url_env: "",
        input: "",
        input_table: "input",
        query: "",
        read_only: true,
      };
    case "script":
      return { type, runtime: "python", path: "", args: [], timeout_secs: 60 };
    case "data":
      return { type, path: "", format: "csv" };
    case "directory":
      return {
        type,
        path: ".",
        include: [],
        exclude: [],
        gitignore: true,
        contents: true,
        max_files: 200,
        max_bytes: 262144,
      };
    case "file":
      return {
        type,
        path: "",
        mode: "create",
        unfence: true,
        bundle: false,
        review: false,
      };
  }
}

export function nextNodeId(graph: GraphDoc): string {
  const ids = new Set<string>();
  let largest = 0;
  for (const { id } of graph.nodes) {
    ids.add(id);
    if (!/^n\d+$/.test(id)) continue;
    const value = Number(id.slice(1));
    if (Number.isSafeInteger(value) && value > largest) largest = value;
  }
  if (largest < Number.MAX_SAFE_INTEGER) return `n${largest + 1}`;
  // A maximum-sized legacy ID leaves no safe successor. A gap must exist among
  // the first ids.size + 1 candidates, regardless of how large other IDs are.
  for (let value = 1; value <= ids.size + 1; value++) {
    const candidate = `n${value}`;
    if (!ids.has(candidate)) return candidate;
  }
  throw new Error("No safe node ID is available.");
}

export function isNamedWorkflow(node: NodeDoc): boolean {
  const data = typedData(node);
  return data?.type === "workflow" && data.output === "";
}

export function validOutputName(name: string): boolean {
  return /^[A-Za-z_][A-Za-z\d_-]{0,63}$/.test(name) && !/^n\d+$/.test(name);
}

/** Keep obsolete endpoints as handles so persisted wires never disappear. */
export function workflowPorts(
  graph: GraphDoc,
  node: NodeDoc,
  workflowGraphs: WorkflowGraphs = {},
): Port[] {
  if (!isNamedWorkflow(node)) return [];
  const workflow = Object.hasOwn(workflowGraphs, node.id)
    ? workflowGraphs[node.id]
    : undefined;
  const available = Object.keys(workflow?.outputs || {}).filter(
    validOutputName,
  );
  const missing = graph.edges
    .filter((edge) => edge.from === node.id && edge.output != null)
    .map((edge) => edge.output!);
  // A stale publication needs an endpoint too, even if it has no outgoing wire.
  const published = Object.values(graph.outputs || {}).flatMap((handle) =>
    handle.startsWith(`${node.id}.`) ? [handle.slice(node.id.length + 1)] : [],
  );
  return [...new Set([...available, ...missing, ...published])]
    .sort()
    .map((id) => ({ id, missing: !available.includes(id) }));
}

/** Display a legacy unqualified wire on the sole port without rewriting its output. */
export function visibleEdgeHandle(edge: EdgeDoc, ports: Port[]): string | null {
  if (edge.output != null) return edge.output;
  const available = ports.filter((port) => !port.missing);
  return available.length === 1 ? (available[0]?.id ?? null) : null;
}

export function deleteNodes(
  graph: GraphDoc,
  ids: string[],
  readOnly = false,
): GraphDoc {
  if (readOnly) return graph;
  const deleted = new Set(ids);
  return {
    ...graph,
    nodes: graph.nodes.filter((node) => !deleted.has(node.id)),
    edges: graph.edges.filter(
      (edge) => !deleted.has(edge.from) && !deleted.has(edge.to),
    ),
    outputs: Object.fromEntries(
      Object.entries(graph.outputs || {}).filter(([, target]) => {
        const dot = target.indexOf(".");
        const owner = dot === -1 ? target : target.slice(0, dot);
        return !deleted.has(owner);
      }),
    ),
  };
}

export function connectNodes(
  graph: GraphDoc,
  from: string,
  to: string,
  output?: string,
  workflowGraphs: WorkflowGraphs = {},
  readOnly = false,
): GraphDoc {
  if (readOnly || from === to || !graph.nodes.some((node) => node.id === to))
    return graph;
  const node = graph.nodes.find((node) => node.id === from);
  if (!node) return graph;
  if (
    isNamedWorkflow(node)
      ? !workflowPorts(graph, node, workflowGraphs).some(
          (port) => port.id === output && !port.missing,
        )
      : output !== undefined
  )
    return graph;
  if (
    graph.edges.some(
      (edge) =>
        edge.from === from &&
        edge.to === to &&
        (edge.output ?? undefined) === output,
    )
  )
    return graph;
  return {
    ...graph,
    edges: [
      ...graph.edges,
      { from, to, ...(output === undefined ? {} : { output }) },
    ],
  };
}

export function sinkIds(graph: GraphDoc): string[] {
  const upstream = new Set(graph.edges.map((edge) => edge.from));
  return graph.nodes
    .filter((node) => !upstream.has(node.id))
    .map((node) => node.id);
}

export function validatePublication(
  graph: GraphDoc,
  name: string,
  handle: string,
  workflowGraphs: WorkflowGraphs = {},
): string | null {
  if (!validOutputName(name))
    return "Output names must start with a letter or _, contain only letters, digits, _ or -, be at most 64 characters, and not look like n1.";
  const [id, port, extra] = handle.split(".");
  const node = graph.nodes.find((node) => node.id === id);
  if (!node || extra !== undefined)
    return "Choose an available node or output port to publish.";
  if (isNamedWorkflow(node)) {
    if (
      !workflowPorts(graph, node, workflowGraphs).some(
        (item) => item.id === port && !item.missing,
      )
    )
      return "Choose an available output port to publish.";
  } else if (port !== undefined) return "This node has only a default output.";
  return null;
}

/** Validate the whole proposed edit before returning a replacement document. Existing
 * stale exports may stay until explicitly removed, matching saved graph behavior. */
export function assignPublications(
  graph: GraphDoc,
  outputs: Record<string, string>,
  workflowGraphs: WorkflowGraphs = {},
  readOnly = false,
): GraphDoc {
  if (readOnly) throw new Error("This child graph is read-only.");
  if (Object.keys(outputs).length > 64)
    throw new Error("At most 64 published outputs are allowed.");
  for (const [name, handle] of Object.entries(outputs)) {
    const error = validatePublication(graph, name, handle, workflowGraphs);
    if (
      error &&
      (!graph.outputs ||
        !Object.hasOwn(graph.outputs, name) ||
        graph.outputs[name] !== handle)
    )
      throw new Error(error);
  }
  return { ...graph, outputs: { ...outputs } };
}

export function publishOutput(
  graph: GraphDoc,
  handle: string,
  names: string[],
  workflowGraphs: WorkflowGraphs = {},
  readOnly = false,
): GraphDoc {
  const outputs = Object.fromEntries(
    Object.entries(graph.outputs || {}).filter(
      ([, target]) => target !== handle,
    ),
  );
  for (const name of names) {
    if (Object.hasOwn(outputs, name))
      throw new Error(`Output name \`${name}\` is already published.`);
    const error = validatePublication(graph, name, handle, workflowGraphs);
    if (error) throw new Error(error);
    Object.defineProperty(outputs, name, {
      value: handle,
      enumerable: true,
      configurable: true,
      writable: true,
    });
  }
  return assignPublications(graph, outputs, workflowGraphs, readOnly);
}

export function flowToGraphPoint(point: Point, view: GraphDoc["view"]): Point {
  const zoom = view.zoom ?? 1;
  return { x: (point.x - view.pan_x) / zoom, y: (point.y - view.pan_y) / zoom };
}

export function graphToFlowPoint(point: Point, view: GraphDoc["view"]): Point {
  const zoom = view.zoom ?? 1;
  return { x: point.x * zoom + view.pan_x, y: point.y * zoom + view.pan_y };
}

/** Rendered card size in graph units, used to keep new nodes out of occupied space. */
export const CARD_SIZE = { width: 224, height: 104 };

/** The first position at or after `start` whose card would not overlap an existing
 * node, stepping diagonally so repeated adds fan out instead of stacking. */
export function freePosition(graph: GraphDoc, start: Point): Point {
  const gap = 24;
  const overlaps = (point: Point) =>
    graph.nodes.some(
      (node) =>
        point.x < node.x + CARD_SIZE.width + gap &&
        point.x + CARD_SIZE.width + gap > node.x &&
        point.y < node.y + CARD_SIZE.height + gap &&
        point.y + CARD_SIZE.height + gap > node.y,
    );
  let candidate = { x: Math.round(start.x), y: Math.round(start.y) };
  for (let step = 0; step < 64 && overlaps(candidate); step++)
    candidate = { x: candidate.x + 36, y: candidate.y + 36 };
  return candidate;
}

export function scriptRuntimeFor(name: string): "python" | "shell" | "node" {
  return /\.(m?js|cjs)$/i.test(name)
    ? "node"
    : /\.py$/i.test(name)
      ? "python"
      : "shell";
}

export function dataFormatFor(name: string): DataFormat {
  const ext = name.split(".").at(-1)?.toLowerCase() || "";
  if (ext === "csv") return "csv";
  if (ext === "tsv") return "tsv";
  if (["json", "ndjson", "jsonl"].includes(ext)) return "json";
  if (["md", "markdown"].includes(ext)) return "markdown";
  if (["png", "jpg", "jpeg", "gif", "webp", "bmp", "tiff", "svg"].includes(ext))
    return "image";
  if (ext === "pdf") return "pdf";
  if (ext === "parquet") return "parquet";
  if (["txt", "text", "log"].includes(ext)) return "text";
  return "binary";
}

/** A node name no other node uses, derived from `base` with a numbered suffix. */
export function uniqueName(base: string, taken: ReadonlySet<string>): string {
  const root = base.trim();
  if (!root) return "";
  if (!taken.has(root)) return root;
  const stem = root.replace(/-copy(-\d+)?$/, "");
  let candidate = `${stem}-copy`;
  for (let n = 2; taken.has(candidate); n++) candidate = `${stem}-copy-${n}`;
  return candidate;
}

export type Placement = { offset: Point } | { at: Point };

/** Insert copies of `nodes` (and the wires among them) with fresh ids, unique names
 * and a shared displacement, returning the new document and the inserted ids. */
export function cloneNodes(
  graph: GraphDoc,
  nodes: NodeDoc[],
  edges: EdgeDoc[],
  placement: Placement = { offset: { x: 40, y: 40 } },
): { graph: GraphDoc; ids: string[] } {
  if (!nodes.length) return { graph, ids: [] };
  const taken = new Set(
    graph.nodes.flatMap((node) =>
      node.name?.trim() ? [node.name.trim()] : [],
    ),
  );
  const origin = {
    x: Math.min(...nodes.map((node) => node.x)),
    y: Math.min(...nodes.map((node) => node.y)),
  };
  const shift =
    "at" in placement
      ? { x: placement.at.x - origin.x, y: placement.at.y - origin.y }
      : placement.offset;
  const ids = new Map<string, string>();
  const working: GraphDoc = { ...graph, nodes: [...graph.nodes] };
  for (const node of nodes) {
    const id = nextNodeId(working);
    ids.set(node.id, id);
    const name = node.name?.trim() ? uniqueName(node.name, taken) : undefined;
    if (name) taken.add(name);
    const copy: NodeDoc = {
      ...structuredClone(node),
      id,
      x: node.x + shift.x,
      y: node.y + shift.y,
    };
    if (name) copy.name = name;
    else delete copy.name;
    working.nodes.push(copy);
  }
  const copied = edges.flatMap((edge) => {
    const from = ids.get(edge.from);
    const to = ids.get(edge.to);
    return from && to ? [{ ...edge, from, to }] : [];
  });
  return {
    graph: { ...working, edges: [...graph.edges, ...copied] },
    ids: [...ids.values()],
  };
}
