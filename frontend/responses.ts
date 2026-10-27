import {
  array,
  boolean,
  decode,
  enumeration as choice,
  integer,
  independent,
  lazy,
  nil,
  nullable,
  number,
  object,
  optional,
  record,
  refine,
  schema,
  string,
  variant,
  type Schema,
} from "./decode";
import type { Protocol, Method } from "./protocol";
import type { GraphDoc, NodeData, NodeRun, Run, RuntimeState } from "./types";
import type { RunEvent } from "./runFeed";

const strings = array(string);
// Positions are f32 in the Rust document. Reject values that would overflow
// graph-to-screen arithmetic before they reach React Flow.
const coordinate = refine(
  number,
  (value) => Math.abs(value) <= 3.4028235e38,
  "a finite f32 coordinate",
);
const values = record(string);
const auth = variant("kind", {
  none: object({ kind: choice("none") }),
  env: object({ kind: choice("env"), var: string }),
});
const agentFields = {
  provider: choice(
    "ollama",
    "lmstudio",
    "llamacpp",
    "openrouter",
    "anthropic",
    "openai",
    "google",
    "mistral",
    "custom",
  ),
  endpoint: string,
  model_type: choice("generative", "embedding", "image"),
  model: string,
  auth,
  prompt: string,
  message: optional(string),
  context_size: integer,
  temperature: number,
  output: choice("text", "json", "embedding", "image"),
};
const databaseFields = {
  engine: choice("sqlite", "duckdb", "postgres", "mysql"),
  path: string,
  url_env: string,
  input: string,
  input_table: string,
  query: string,
  read_only: boolean,
};
const inputFields = {
  kind: choice("text", "number", "boolean", "json", "file"),
  default: string,
  required: boolean,
};
const binding = variant("kind", {
  value: object({ kind: choice("value"), value: string }),
  output: object({ kind: choice("output"), node: string }),
});
export const nodeDataSchema: Schema<NodeData> = variant("type", {
  agent: object({ type: choice("agent"), ...agentFields }),
  database: object({ type: choice("database"), ...databaseFields }),
  input: object({ type: choice("input"), ...inputFields }),
  workflow: object({
    type: choice("workflow"),
    graph: string,
    output: string,
    inputs: record(binding),
  }),
  script: object({
    type: choice("script"),
    runtime: choice("python", "shell", "node"),
    path: string,
    args: strings,
    timeout_secs: integer,
  }),
  data: object({
    type: choice("data"),
    path: string,
    format: choice(
      "csv",
      "tsv",
      "json",
      "text",
      "markdown",
      "image",
      "pdf",
      "parquet",
      "binary",
    ),
  }),
  directory: object({
    type: choice("directory"),
    path: string,
    include: strings,
    exclude: strings,
    gitignore: boolean,
    contents: boolean,
    max_files: integer,
    max_bytes: integer,
  }),
  file: object({
    type: choice("file"),
    path: string,
    mode: choice("create", "overwrite", "append"),
    unfence: boolean,
    bundle: boolean,
    review: optional(boolean),
  }),
});
const nodeDataOrBlank = schema<NodeData | Record<string, never>>(
  (value, path, context) => {
    if (
      value &&
      typeof value === "object" &&
      !Array.isArray(value) &&
      [Object.prototype, null].includes(Object.getPrototypeOf(value)) &&
      Object.keys(value).length === 0
    )
      return {};
    return nodeDataSchema.read(value, path, context);
  },
);
const graphShape = object({
  version: integer,
  name: string,
  view: object({
    pan_x: coordinate,
    pan_y: coordinate,
    zoom: optional(
      refine(
        number,
        (zoom) => zoom >= 0.15 && zoom <= 2.5,
        "zoom between 0.15 and 2.5",
      ),
    ),
  }),
  nodes: array(
    object({
      id: refine(string, (id) => id.length > 0, "a nonempty node id"),
      x: coordinate,
      y: coordinate,
      name: optional(nullable(string)),
      kind: optional(nullable(string)),
      color: optional(nullable(string)),
      data: optional(nullable(nodeDataOrBlank)),
    }),
  ),
  edges: array(
    object({ from: string, to: string, output: optional(nullable(string)) }),
  ),
  outputs: optional(values),
});
const graph: Schema<GraphDoc> = refine(
  graphShape,
  (doc) => new Set(doc.nodes.map((node) => node.id)).size === doc.nodes.length,
  "unique node ids",
);
const field = object({
  node: string,
  name: string,
  config: object(inputFields),
});
const issue = object({
  node: optional(string),
  field: string,
  message: string,
});
const kind = object({ name: string, color: string });
const option = { value: string, label: string };
const options = array(object(option));
const library = object({
  workspace: string,
  graphs: strings,
  kinds: array(kind),
  agents: array(object({ name: string, config: object(agentFields) })),
  databases: array(object({ name: string, config: object(databaseFields) })),
  prompts: strings,
  scripts: strings,
  data: strings,
  catalog: object({
    nodes: array(
      object({
        type: choice(
          "agent",
          "database",
          "input",
          "workflow",
          "script",
          "data",
          "directory",
          "file",
        ),
        label: string,
        color: string,
        data: nodeDataSchema,
      }),
    ),
    providers: array(
      object({ ...option, endpoint: string, model: string, env_var: string }),
    ),
    model_types: options,
    outputs: options,
    engines: options,
    runtimes: options,
    formats: options,
    write_modes: options,
    input_types: options,
  }),
});
const nodeRun: Schema<NodeRun> = object({
  state: choice("ran", "reused", "failed"),
  data: optional(nodeDataSchema),
  label: optional(string),
  saved: optional(string),
  key: optional(string),
  preview: optional(string),
  complete: optional(boolean),
  error: optional(string),
  named_outputs: optional(record(lazy(() => nodeRun))),
});
const run: Schema<Run> = object({
  id: string,
  graph: string,
  targets: strings,
  elapsed_ms: integer,
  outcome: variant("kind", {
    ok: object({ kind: choice("ok") }),
    cancelled: object({ kind: choice("cancelled") }),
    failed: object({ kind: choice("failed"), error: string }),
  }),
  inputs: optional(values),
  nodes: record(nodeRun),
  log: strings,
  calls: optional(record(object({ graph, run: lazy(() => run) }))),
  workflows: optional(record(graph)),
});
const mark = choice("pending", "running", "ran", "reused", "failed");
const event: Schema<RunEvent> = variant("kind", {
  log: object({
    kind: choice("log"),
    seq: integer,
    text: string,
    truncated: optional(boolean),
  }),
  node: object({ kind: choice("node"), seq: integer, id: string, state: mark }),
  text: object({
    kind: choice("text"),
    seq: integer,
    id: string,
    text: string,
    truncated: optional(boolean),
  }),
  output: object({
    kind: choice("output"),
    seq: integer,
    id: string,
    result: nodeRun,
  }),
});
const runtime: Schema<RuntimeState> = object({
  run_id: string,
  running: boolean,
  cancelling: boolean,
  graph,
  targets: strings,
  marks: record(mark),
  log: strings,
  stream: nullable(object({ id: string, text: string })),
  workflows: record(graph),
  review: nullable(
    object({
      id: string,
      node: string,
      files: array(
        object({
          path: string,
          before_bytes: nullable(integer),
          after_bytes: integer,
          diff: string,
        }),
      ),
    }),
  ),
  run: nullable(run),
  error: nullable(string),
  events: optional(
    refine(
      array(event),
      (events) =>
        events.every(
          (item, index) =>
            index === 0 || item.seq > (events[index - 1]?.seq ?? Infinity),
        ),
      "strictly increasing event sequence numbers",
    ),
  ),
  events_dropped: optional(integer),
});
const base64 = refine(
  string,
  (value) => {
    if (value.length % 4 !== 0 || /[^A-Za-z\d+/=]/.test(value)) return false;
    const padding = value.endsWith("==") ? 2 : value.endsWith("=") ? 1 : 0;
    return value.indexOf("=") === (padding ? value.length - padding : -1);
  },
  "base64 data",
);
const output = refine(
  object({
    label: string,
    text: nullable(string),
    base64: nullable(base64),
    saved: nullable(string),
    complete: boolean,
    media_type: optional(nullable(string)),
    limited: optional(boolean),
    bytes: optional(integer),
  }),
  (data) =>
    !(data.text !== null && data.base64 !== null) &&
    (data.text !== null || data.base64 !== null || data.limited === true) &&
    !(data.limited && data.complete),
  "a consistent output payload",
);

const responses: { [M in Method]: Schema<Protocol[M]["result"]> } = {
  bootstrap: library,
  load_graph: graph,
  save_graph: object({ name: string, markdown: string, json: string }),
  save_kind: string,
  delete_kind: nil,
  save_preset: string,
  delete_preset: nil,
  save_prompt: string,
  load_prompt: string,
  delete_prompt: nil,
  validate_node: array(issue),
  validate_graph: object({
    issues: array(issue),
    fields: array(field),
    sinks: strings,
  }),
  workflow_fields: array(field),
  prepare_run: object({
    targets: strings,
    fields: array(field),
    inputs: values,
  }),
  start_run: object({ run_id: refine(string, (id) => !!id, "a run id") }),
  runtime_state: nullable(runtime),
  cancel_run: nil,
  review: nil,
  history: array(independent(run)),
  output,
  clear_history: integer,
  clear_cache: integer,
  describe_schema: string,
  write_sql: string,
  list_models: strings,
  env_var_set: boolean,
  delete_graph: nil,
  rename_graph: object({ name: string }),
  duplicate_graph: object({ name: string }),
  reveal: nil,
  install_examples: object({ installed: strings, skipped: strings }),
};

export function decodeResponse<M extends Method>(
  method: M,
  value: unknown,
): Protocol[M]["result"] {
  try {
    return decode(responses[method], value);
  } catch (error) {
    throw new Error(
      `Invalid ${method} response: ${error instanceof Error ? error.message : "invalid data"}`,
    );
  }
}
