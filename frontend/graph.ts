import type { EdgeDoc, GraphDoc, NodeData, NodeDoc, NodeType } from "./types";

export type WorkflowGraphs = Record<string, GraphDoc>;
export type Port = { id: string; missing: boolean };
export type Point = { x: number; y: number };

export function typedData(node: NodeDoc): NodeData | undefined {
  return node.data && "type" in node.data ? (node.data as NodeData) : undefined;
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

const fileName = (path: string) =>
  path.trim().split("/").filter(Boolean).pop() || "no file";
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
const title = (value: string) =>
  displayNames[value] || value.charAt(0).toUpperCase() + value.slice(1);

export function nodeSummary(node: NodeDoc): string {
  const data = typedData(node);
  if (!data) return node.kind || "Choose a node type in the inspector";
  switch (data.type) {
    case "input":
      return `${data.kind === "file" ? "File from data/" : title(data.kind)} · ${data.required ? "required" : "optional"}`;
    case "workflow":
      return data.graph
        ? `${data.graph} → ${data.output || "published outputs"}`
        : "choose a saved graph";
    case "agent":
      return `${title(data.provider)} · ${data.model.trim() || "no model"}`;
    case "database":
      return `${title(data.engine)} · ${["sqlite", "duckdb"].includes(data.engine) ? fileName(data.path) : data.url_env.trim() || "no connection"}${data.read_only ? "" : " · write"}`;
    case "script":
      return `${title(data.runtime)} · ${fileName(data.path)}`;
    case "data":
      return `${title(data.format)} · ${fileName(data.path)}`;
    case "directory":
      return `${data.path.trim() || "."}${data.include[0] ? ` · ${data.include[0]}` : ""} · ${data.contents ? "contents" : "listing"}`;
    case "file":
      return `${data.bundle ? `${data.path.trim() || "."}/ · sections` : fileName(data.path)} · ${data.mode === "create" ? "create only" : data.mode}`;
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
  const ids = new Set(graph.nodes.map((node) => node.id));
  let n =
    Math.max(
      0,
      ...graph.nodes.map((node) =>
        /^n\d+$/.test(node.id) ? Number(node.id.slice(1)) : 0,
      ),
    ) + 1;
  while (ids.has(`n${n}`)) n++;
  return `n${n}`;
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
  const available = Object.keys(workflowGraphs[node.id]?.outputs || {}).filter(
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
  return available.length === 1 ? available[0].id : null;
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
      Object.entries(graph.outputs || {}).filter(
        ([, target]) => !deleted.has(target.split(".")[0]),
      ),
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
    if (error && graph.outputs?.[name] !== handle) throw new Error(error);
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
