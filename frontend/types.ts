export type NodeType =
  | "input"
  | "workflow"
  | "agent"
  | "database"
  | "script"
  | "data"
  | "directory"
  | "file";
export type InputType = "text" | "number" | "boolean" | "json" | "file";
export type Provider =
  | "ollama"
  | "lmstudio"
  | "llamacpp"
  | "openrouter"
  | "anthropic"
  | "openai"
  | "google"
  | "mistral"
  | "custom";
export type ModelType = "generative" | "embedding" | "image";
export type OutputType = "text" | "json" | "embedding" | "image";
export type Auth = { kind: "none" } | { kind: "env"; var: string };
export interface AgentConfig {
  provider: Provider;
  endpoint: string;
  model_type: ModelType;
  model: string;
  auth: Auth;
  prompt: string;
  message?: string;
  context_size: number;
  temperature: number;
  output: OutputType;
}
export interface DatabaseConfig {
  engine: "sqlite" | "duckdb" | "postgres" | "mysql";
  path: string;
  url_env: string;
  input: string;
  input_table: string;
  query: string;
  read_only: boolean;
}
export interface InputConfig {
  kind: InputType;
  default: string;
  required: boolean;
}
export type Binding =
  { kind: "value"; value: string } | { kind: "output"; node: string };
export interface WorkflowConfig {
  graph: string;
  output: string;
  inputs: Record<string, Binding>;
}
export type DataFormat =
  | "csv"
  | "tsv"
  | "json"
  | "text"
  | "markdown"
  | "image"
  | "pdf"
  | "parquet"
  | "binary";
export type NodeData =
  | ({ type: "agent" } & AgentConfig)
  | ({ type: "database" } & DatabaseConfig)
  | ({ type: "input" } & InputConfig)
  | ({ type: "workflow" } & WorkflowConfig)
  | {
      type: "script";
      runtime: "python" | "shell" | "node";
      path: string;
      args: string[];
      timeout_secs: number;
    }
  | { type: "data"; path: string; format: DataFormat }
  | {
      type: "directory";
      path: string;
      include: string[];
      exclude: string[];
      gitignore: boolean;
      contents: boolean;
      max_files: number;
      max_bytes: number;
    }
  | {
      type: "file";
      path: string;
      mode: "create" | "overwrite" | "append";
      unfence: boolean;
      bundle: boolean;
      review?: boolean;
    };
export interface NodeDoc {
  id: string;
  x: number;
  y: number;
  name?: string | null;
  kind?: string | null;
  color?: string | null;
  data?: NodeData | Record<string, never> | null;
}
export interface EdgeDoc {
  from: string;
  to: string;
  output?: string | null;
}
export interface GraphDoc {
  version: number;
  name: string;
  view: { pan_x: number; pan_y: number; zoom?: number };
  nodes: NodeDoc[];
  edges: EdgeDoc[];
  outputs?: Record<string, string>;
}
export interface NodeKind {
  name: string;
  color: string;
}
export interface Preset<T> {
  name: string;
  config: T;
}
export interface Option {
  value: string;
  label: string;
}
export interface Catalog {
  nodes: { type: NodeType; label: string; color: string; data: NodeData }[];
  providers: (Option & { endpoint: string; model: string; env_var: string })[];
  model_types: Option[];
  outputs: Option[];
  engines: Option[];
  runtimes: Option[];
  formats: Option[];
  write_modes: Option[];
  input_types: Option[];
}
export interface Library {
  workspace: string;
  graphs: string[];
  kinds: NodeKind[];
  agents: Preset<AgentConfig>[];
  databases: Preset<DatabaseConfig>[];
  prompts: string[];
  scripts: string[];
  data: string[];
  catalog: Catalog;
}
export interface Field {
  node: string;
  name: string;
  config: InputConfig;
}
export interface Issue {
  node?: string;
  field: string;
  message: string;
}
export type Mark = "pending" | "running" | "ran" | "reused" | "failed";
export interface NodeRun {
  state: "ran" | "reused" | "failed";
  data?: NodeData;
  label?: string;
  saved?: string;
  key?: string;
  preview?: string;
  complete?: boolean;
  error?: string;
  named_outputs?: Record<string, NodeRun>;
}
export interface Run {
  id: string;
  graph: string;
  targets: string[];
  elapsed_ms: number;
  outcome:
    { kind: "ok" } | { kind: "failed"; error: string } | { kind: "cancelled" };
  inputs?: Record<string, string>;
  nodes: Record<string, NodeRun>;
  log: string[];
  calls?: Record<string, { graph: GraphDoc; run: Run }>;
  workflows?: Record<string, GraphDoc>;
}
export interface FileReview {
  path: string;
  before_bytes: number | null;
  after_bytes: number;
  diff: string;
}
export interface Review {
  id: string;
  node: string;
  files: FileReview[];
}
export interface RuntimeState {
  events?: import("./runFeed").RunEvent[];
  events_dropped?: number;
  run_id: string;
  running: boolean;
  cancelling: boolean;
  graph: GraphDoc;
  targets: string[];
  marks: Record<string, Mark>;
  log: string[];
  stream: { id: string; text: string } | null;
  workflows: Record<string, GraphDoc>;
  review: Review | null;
  run: Run | null;
  error: string | null;
}
export interface PreparedRun {
  targets: string[];
  fields: Field[];
  inputs: Record<string, string>;
}
export interface RunForm extends PreparedRun {
  graph: GraphDoc;
  fresh: boolean;
  error?: string;
}
export const isNodeData = (value: NodeDoc["data"]): value is NodeData =>
  !!value &&
  typeof value === "object" &&
  Object.hasOwn(value, "type") &&
  [
    "input",
    "workflow",
    "agent",
    "database",
    "script",
    "data",
    "directory",
    "file",
  ].some((type) => type === value.type);
