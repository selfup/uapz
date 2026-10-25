import { useEffect, useId, useRef, useState, type ReactNode } from "react";
import { request } from "./api";
import type { GraphDoc, NodeDoc, NodeData } from "./types";
import {
  isNamedWorkflow,
  publishOutput,
  workflowPorts,
  type WorkflowGraphs,
} from "./graph";
import { compareNodeOutputs, sameNodeConfig } from "./inspectorState";
import "./inspector.css";

type Auth = { kind: "none" } | { kind: "env"; var: string };
type Agent = {
  type: "agent";
  provider: string;
  endpoint: string;
  model_type: string;
  model: string;
  auth: Auth;
  prompt: string;
  message?: string;
  context_size: number;
  temperature: number;
  output: string;
};
type Database = {
  type: "database";
  engine: string;
  path: string;
  url_env: string;
  input: string;
  input_table: string;
  query: string;
  read_only: boolean;
};
type Binding =
  { kind: "value"; value: string } | { kind: "output"; node: string };
type Input = {
  type: "input";
  kind: string;
  default: string;
  required: boolean;
};
type Workflow = {
  type: "workflow";
  graph: string;
  output: string;
  inputs: Record<string, Binding>;
};
type Script = {
  type: "script";
  runtime: string;
  path: string;
  args: string[];
  timeout_secs: number;
};
type Data = { type: "data"; path: string; format: string };
type Directory = {
  type: "directory";
  path: string;
  include: string[];
  exclude: string[];
  gitignore: boolean;
  contents: boolean;
  max_files: number;
  max_bytes: number;
};
type File = {
  type: "file";
  path: string;
  mode: string;
  unfence: boolean;
  bundle: boolean;
  review?: boolean;
};
type Config =
  Agent | Database | Input | Workflow | Script | Data | Directory | File;
type Field = { node: string; name: string; config: Omit<Input, "type"> };
export type InspectorIssue = { field: string; message: string; node?: string };
export type InspectorNodeResult = {
  state: string;
  data?: NodeData | null;
  label?: string | null;
  saved?: string | null;
  preview?: string;
  complete?: boolean;
  error?: string | null;
  key?: string | null;
  named_outputs?: Record<string, InspectorNodeResult>;
};
export type InspectorRun = {
  id: string;
  graph: string;
  targets: string[];
  elapsed_ms: number;
  outcome: { kind: string; error?: string };
  nodes: Record<string, InspectorNodeResult>;
  log: string[];
  inputs?: Record<string, string>;
  workflows?: Record<string, GraphDoc>;
  calls?: Record<string, { graph: GraphDoc; run: InspectorRun }>;
};
export type InspectorLibrary = {
  agents: { name: string; config: Omit<Agent, "type"> }[];
  databases: { name: string; config: Omit<Database, "type"> }[];
  prompts: string[];
  graphs: string[];
  scripts: string[];
  data: string[];
};
export type InspectorProps = {
  graph: GraphDoc;
  node: NodeDoc | null;
  readOnly?: boolean;
  onChange: (node: NodeDoc) => void;
  onPublish?: (handle: string, name: string) => void;
  onGraphChange?: (graph: GraphDoc) => void;
  onRun?: (fresh: boolean) => void;
  onDelete?: () => void;
  workflowGraphs?: WorkflowGraphs;
  outputContext?: OutputContext;
  library?: InspectorLibrary;
  issues?: InspectorIssue[];
  results?: Record<string, InspectorNodeResult>;
  activeRun?: InspectorRun | null;
  history?: InspectorRun[];
  running?: boolean;
  log?: string[];
  stream?: { id: string; text: string } | null;
  onOpenHistory?: (id: string) => void;
  onOpenWorkflow?: (nodeId: string) => void;
  onLibraryChanged?: () => void;
};
type OutputContext = { graph: string; runId: string; callPath?: string };
type FullOutput = {
  label: string;
  text: string | null;
  base64: string | null;
  saved: string | null;
  complete: boolean;
  media_type?: string | null;
};

const EMPTY_LIBRARY: InspectorLibrary = {
  agents: [],
  databases: [],
  prompts: [],
  graphs: [],
  scripts: [],
  data: [],
};
const LABELS: Record<Config["type"], string> = {
  input: "Run input",
  workflow: "Workflow",
  agent: "Agent",
  database: "Database",
  script: "Script",
  data: "Data",
  directory: "Directory",
  file: "Write file",
};
const COLORS: Record<Config["type"], string> = {
  input: "#06b6d4",
  workflow: "#6366f1",
  agent: "#8b5cf6",
  database: "#0ea5e9",
  script: "#f59e0b",
  data: "#22c55e",
  directory: "#14b8a6",
  file: "#ef4444",
};
const PROVIDERS = [
  ["ollama", "Ollama"],
  ["lmstudio", "LM Studio"],
  ["llamacpp", "llama.cpp"],
  ["openrouter", "OpenRouter"],
  ["anthropic", "Anthropic"],
  ["openai", "OpenAI"],
  ["google", "Google"],
  ["mistral", "Mistral"],
  ["custom", "Custom · OpenAI compatible"],
];
const PROVIDER_DEFAULTS: Record<
  string,
  { endpoint: string; model: string; env: string }
> = {
  ollama: { endpoint: "http://localhost:11434", model: "llama3.1", env: "" },
  lmstudio: { endpoint: "http://localhost:1234/v1", model: "", env: "" },
  llamacpp: { endpoint: "http://localhost:8080/v1", model: "", env: "" },
  openrouter: {
    endpoint: "https://openrouter.ai/api/v1",
    model: "",
    env: "OPENROUTER_API_KEY",
  },
  anthropic: {
    endpoint: "https://api.anthropic.com/v1",
    model: "claude-opus-5-5",
    env: "ANTHROPIC_API_KEY",
  },
  openai: {
    endpoint: "https://api.openai.com/v1",
    model: "",
    env: "OPENAI_API_KEY",
  },
  google: {
    endpoint: "https://generativelanguage.googleapis.com/v1beta",
    model: "",
    env: "GEMINI_API_KEY",
  },
  mistral: {
    endpoint: "https://api.mistral.ai/v1",
    model: "",
    env: "MISTRAL_API_KEY",
  },
  custom: { endpoint: "", model: "", env: "" },
};
const DEFAULTS: Record<Config["type"], Config> = {
  agent: {
    type: "agent",
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
  },
  database: {
    type: "database",
    engine: "sqlite",
    path: "",
    url_env: "",
    input: "",
    input_table: "input",
    query: "",
    read_only: true,
  },
  input: { type: "input", kind: "text", default: "", required: true },
  workflow: { type: "workflow", graph: "", output: "", inputs: {} },
  script: {
    type: "script",
    runtime: "python",
    path: "",
    args: [],
    timeout_secs: 60,
  },
  data: { type: "data", path: "", format: "csv" },
  directory: {
    type: "directory",
    path: ".",
    include: [],
    exclude: [],
    gitignore: true,
    contents: true,
    max_files: 200,
    max_bytes: 262144,
  },
  file: {
    type: "file",
    path: "",
    mode: "create",
    unfence: true,
    bundle: false,
    review: false,
  },
};

function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}
function nodeConfig(node: NodeDoc | null): Config | null {
  const data = node?.data as Config | null | undefined;
  return data?.type && data.type in DEFAULTS ? data : null;
}
function bareConfig(data: Config) {
  const result = { ...data } as Record<string, unknown>;
  delete result.type;
  return result;
}

function FieldControl({
  label,
  help,
  children,
}: {
  label: string;
  help?: string;
  children: (id: string) => ReactNode;
}) {
  const id = useId();
  return (
    <div className="ins-field">
      <label htmlFor={id}>{label}</label>
      {children(id)}
      {help && <p className="ins-help">{help}</p>}
    </div>
  );
}
function Text({
  label,
  value,
  onChange,
  help,
  placeholder,
  multiline = false,
  mono = false,
  rows = 4,
  choices,
}: {
  label: string;
  value: string;
  onChange: (value: string) => void;
  help?: string;
  placeholder?: string;
  multiline?: boolean;
  mono?: boolean;
  rows?: number;
  choices?: string[];
}) {
  return (
    <FieldControl label={label} help={help}>
      {(id) =>
        multiline ? (
          <textarea
            id={id}
            className={mono ? "ins-mono" : ""}
            value={value}
            onChange={(event) => onChange(event.target.value)}
            placeholder={placeholder}
            rows={rows}
            spellCheck={!mono}
          />
        ) : (
          <>
            <input
              id={id}
              className={mono ? "ins-mono" : ""}
              value={value}
              onChange={(event) => onChange(event.target.value)}
              placeholder={placeholder}
              list={choices?.length ? `${id}-choices` : undefined}
              autoComplete="off"
              spellCheck={!mono}
            />
            {choices?.length ? (
              <datalist id={`${id}-choices`}>
                {choices.map((choice) => (
                  <option key={choice} value={choice} />
                ))}
              </datalist>
            ) : null}
          </>
        )
      }
    </FieldControl>
  );
}
function Select({
  label,
  value,
  onChange,
  options,
  help,
}: {
  label: string;
  value: string;
  onChange: (value: string) => void;
  options: string[][];
  help?: string;
}) {
  return (
    <FieldControl label={label} help={help}>
      {(id) => (
        <select
          id={id}
          value={value}
          onChange={(event) => onChange(event.target.value)}
        >
          {!options.some((option) => option[0] === value) && (
            <option value={value}>{value || "Choose…"}</option>
          )}
          {options.map(([key, text]) => (
            <option key={key} value={key}>
              {text}
            </option>
          ))}
        </select>
      )}
    </FieldControl>
  );
}
function Toggle({
  label,
  value,
  onChange,
  help,
}: {
  label: string;
  value: boolean;
  onChange: (value: boolean) => void;
  help?: string;
}) {
  return (
    <div className="ins-toggle-field">
      <label className="ins-toggle">
        <input
          type="checkbox"
          checked={value}
          onChange={(event) => onChange(event.target.checked)}
        />
        <span>{label}</span>
      </label>
      {help && <p className="ins-help">{help}</p>}
    </div>
  );
}
function NumberField({
  label,
  value,
  onChange,
  min = 0,
  step = 1,
  help,
}: {
  label: string;
  value: number;
  onChange: (value: number) => void;
  min?: number;
  step?: number;
  help?: string;
}) {
  const [draft, setDraft] = useState(String(value));
  useEffect(() => setDraft(String(value)), [value]);
  return (
    <FieldControl label={label} help={help}>
      {(id) => (
        <input
          id={id}
          type="number"
          value={draft}
          min={min}
          step={step}
          onBlur={() => setDraft(String(value))}
          onChange={(event) => {
            const text = event.target.value;
            setDraft(text);
            if (text !== "" && Number.isFinite(Number(text)))
              onChange(Number(text));
          }}
        />
      )}
    </FieldControl>
  );
}
function Lines({
  label,
  value,
  onChange,
  help,
}: {
  label: string;
  value: string[];
  onChange: (value: string[]) => void;
  help?: string;
}) {
  const [draft, setDraft] = useState(value.join("\n"));
  const encoded = JSON.stringify(value);
  useEffect(() => {
    setDraft((previous) =>
      JSON.stringify(previous.split("\n").filter(Boolean)) === encoded
        ? previous
        : value.join("\n"),
    );
  }, [encoded]); // Keep an unfinished last line while typing.
  return (
    <Text
      label={label}
      multiline
      rows={3}
      mono
      value={draft}
      help={help || "One item per line."}
      onChange={(text) => {
        setDraft(text);
        onChange(text.split("\n").filter(Boolean));
      }}
    />
  );
}

export default function Inspector(props: InspectorProps) {
  const {
    graph,
    node,
    readOnly = false,
    onChange,
    onPublish,
    onGraphChange,
    onRun,
    onDelete,
    activeRun,
    results,
    history = [],
    running = false,
    stream,
    onOpenHistory,
    onOpenWorkflow,
    onLibraryChanged,
  } = props;
  const library = props.library || EMPTY_LIBRARY;
  const [tab, setTab] = useState<"configure" | "results" | "history">(
    "configure",
  );
  const [localIssues, setLocalIssues] = useState<InspectorIssue[]>([]);
  const [notice, setNotice] = useState<{ text: string; error: boolean } | null>(
    null,
  );
  const [busy, setBusy] = useState(false);
  const [drafts, setDrafts] = useState<
    Record<
      string,
      { preset?: string; prompt?: string; sql?: string; agent?: string }
    >
  >({});
  const [schema, setSchema] = useState<{
    node: string;
    graph: string;
    text: string;
  } | null>(null);
  const [child, setChild] = useState<{
    name: string;
    graph: GraphDoc;
    fields: Field[];
  } | null>(null);
  const [childError, setChildError] = useState("");
  const config = nodeConfig(node);
  const serialized = JSON.stringify(config);
  const selectedId = node?.id || "";
  const draftKey = `${graph.name}:${selectedId}`;
  const draft = drafts[draftKey] || {};
  const setDraft = (patch: Partial<typeof draft>) =>
    setDrafts((previous) => ({
      ...previous,
      [draftKey]: { ...previous[draftKey], ...patch },
    }));
  const latest = useRef({ graph, node, readOnly, onChange });
  latest.current = { graph, node, readOnly, onChange };
  const workflowName = config?.type === "workflow" ? config.graph : "";
  const capturedGraph = props.workflowGraphs?.[selectedId];
  const capturedFingerprint = JSON.stringify(capturedGraph);

  useEffect(() => {
    if (props.issues || !config) {
      setLocalIssues([]);
      return;
    }
    let active = true;
    const timeout = window.setTimeout(() => {
      request<InspectorIssue[]>("validate_node", {
        data: config,
        name: node?.name || "",
      })
        .then((issues) => {
          if (active) setLocalIssues(issues);
        })
        .catch((error) => {
          if (active)
            setLocalIssues([
              { field: "validation", message: errorMessage(error) },
            ]);
        });
    }, 180);
    return () => {
      active = false;
      window.clearTimeout(timeout);
    };
  }, [serialized, node?.name, props.issues]);

  useEffect(() => {
    setChild(null);
    setChildError("");
    if (!workflowName) return;
    let active = true;
    const source = capturedGraph
      ? Promise.resolve(capturedGraph)
      : readOnly
        ? Promise.reject(
            new Error(
              "The child graph is unavailable in this inspection context.",
            ),
          )
        : request<GraphDoc>("load_graph", { name: workflowName });
    source
      .then(async (doc) => {
        const call = nodeConfig(node);
        const targets =
          call?.type === "workflow" && call.output ? [call.output] : undefined;
        const fields = await request<Field[]>("workflow_fields", {
          doc,
          ...(targets ? { targets } : {}),
        });
        if (active) setChild({ name: workflowName, graph: doc, fields });
      })
      .catch((error) => {
        if (active) setChildError(errorMessage(error));
      });
    return () => {
      active = false;
    };
  }, [
    workflowName,
    config?.type === "workflow" ? config.output : "",
    selectedId,
    capturedFingerprint,
    readOnly,
  ]);

  useEffect(() => {
    setNotice(null);
  }, [selectedId, graph.name]);
  const update = (patch: Record<string, unknown>) => {
    if (!node || !config || readOnly) return;
    const data = { ...config, ...patch } as Config;
    const writable =
      data.type === "file" || (data.type === "database" && !data.read_only);
    onChange({
      ...node,
      data: data as NodeData,
      color: writable ? "#ef4444" : COLORS[data.type],
    });
  };
  const updateAsync = (patch: Record<string, unknown>) => {
    const current = latest.current;
    if (
      current.readOnly ||
      current.graph.name !== graph.name ||
      !node ||
      !config
    )
      return;
    const target = current.graph.nodes.find(
      (candidate) => candidate.id === node.id,
    );
    const targetConfig = nodeConfig(target || null);
    if (!target || !targetConfig || targetConfig.type !== config.type) return;
    const data = { ...targetConfig, ...patch } as Config;
    const writable =
      data.type === "file" || (data.type === "database" && !data.read_only);
    current.onChange({
      ...target,
      data: data as NodeData,
      color: writable ? "#ef4444" : COLORS[data.type],
    });
  };
  async function perform(action: () => Promise<unknown>, success?: string) {
    setBusy(true);
    setNotice(null);
    const stillSelected = () =>
      latest.current.graph.name === graph.name &&
      latest.current.node?.id === selectedId;
    try {
      await action();
      if (success && stillSelected())
        setNotice({ text: success, error: false });
    } catch (error) {
      if (stillSelected())
        setNotice({ text: errorMessage(error), error: true });
    } finally {
      setBusy(false);
    }
  }
  const issues = (props.issues || localIssues).filter(
    (issue) => !issue.node || issue.node === selectedId,
  );
  const currentResult = results?.[selectedId] || activeRun?.nodes[selectedId];
  const previousRun = history.find(
    (run) =>
      run.id !== activeRun?.id &&
      run.id !== props.outputContext?.runId &&
      run.nodes[selectedId],
  );
  const upstream = [
    ...new Map(
      graph.edges
        .filter((edge) => edge.to === selectedId)
        .map((edge) => {
          const source = graph.nodes.find(
            (candidate) => candidate.id === edge.from,
          );
          const handle = edge.output
            ? `${edge.from}.${edge.output}`
            : edge.from;
          const alias = source?.name?.trim()
            ? `${source.name.trim()}${edge.output ? `.${edge.output}` : ""}`
            : handle;
          return [
            handle,
            {
              handle,
              alias,
              label: `${source?.name || edge.from}${edge.output ? ` · ${edge.output}` : ""}`,
              source,
            },
          ] as const;
        }),
    ).values(),
  ].sort((left, right) => left.handle.localeCompare(right.handle));

  function renderAgent(data: Agent) {
    const local = ["ollama", "lmstudio", "llamacpp"].includes(data.provider);
    const modelOptions = [
      ["generative", "Generative"],
      ...(data.provider === "anthropic" ? [] : [["embedding", "Embedding"]]),
      ...(["openai", "custom"].includes(data.provider)
        ? [["image", "Image"]]
        : []),
    ];
    const outputOptions =
      data.model_type === "embedding"
        ? [["embedding", "Embedding"]]
        : data.model_type === "image"
          ? [["image", "Image"]]
          : [
              ["text", "Text"],
              ["json", "JSON"],
            ];
    return (
      <>
        <Select
          label="Provider"
          value={data.provider}
          options={PROVIDERS}
          onChange={(provider) => {
            const defaults = PROVIDER_DEFAULTS[provider];
            update({
              provider,
              model: defaults.model,
              endpoint: "",
              auth: defaults.env
                ? { kind: "env", var: defaults.env }
                : { kind: "none" },
              model_type: "generative",
              output: "text",
            });
          }}
        />
        <Text
          label="Endpoint"
          value={data.endpoint}
          placeholder={PROVIDER_DEFAULTS[data.provider]?.endpoint}
          onChange={(endpoint) => update({ endpoint })}
          help="Leave empty for the provider’s default API endpoint."
          mono
        />
        <div className="ins-two">
          <Select
            label="Model type"
            value={data.model_type}
            options={modelOptions}
            onChange={(model_type) =>
              update({
                model_type,
                output: model_type === "generative" ? "text" : model_type,
              })
            }
          />
          <Text
            label="Model"
            value={data.model}
            onChange={(model) => update({ model })}
          />
        </div>
        {!local && (
          <Text
            label="API key environment variable"
            value={data.auth.kind === "env" ? data.auth.var : ""}
            onChange={(variable) =>
              update({
                auth: variable
                  ? { kind: "env", var: variable }
                  : { kind: "none" },
              })
            }
            placeholder={PROVIDER_DEFAULTS[data.provider]?.env}
            help="Only the variable name is saved. The key stays in your shell."
            mono
          />
        )}
        <Text
          label="System prompt"
          value={data.prompt}
          multiline
          rows={7}
          onChange={(prompt) => update({ prompt })}
          help="Use {{name}} to place a named upstream node’s output."
        />
        {!readOnly && (
          <details className="ins-details">
            <summary>Saved prompts</summary>
            <div className="ins-details-body">
              <Select
                label="Load prompt"
                value=""
                options={[
                  ["", "Choose a saved prompt…"],
                  ...library.prompts.map((name) => [name, name]),
                ]}
                onChange={(name) => {
                  if (name)
                    void perform(async () => {
                      const prompt = await request<string>("load_prompt", {
                        name,
                      });
                      updateAsync({ prompt });
                      setDraft({ prompt: name });
                    }, "Prompt loaded");
                }}
              />
              <div className="ins-inline">
                <Text
                  label="Prompt name"
                  value={draft.prompt || ""}
                  onChange={(prompt) => setDraft({ prompt })}
                />
                <button
                  type="button"
                  disabled={
                    busy || !draft.prompt?.trim() || !data.prompt.trim()
                  }
                  onClick={() =>
                    void perform(async () => {
                      await request("save_prompt", {
                        name: draft.prompt,
                        text: data.prompt,
                      });
                      onLibraryChanged?.();
                    }, "Prompt saved")
                  }
                >
                  Save prompt
                </button>
              </div>
            </div>
          </details>
        )}
        <Text
          label="Message"
          value={data.message || ""}
          multiline
          rows={4}
          onChange={(message) => update({ message })}
          help="Empty sends all inputs together. Place {{name}} here to direct an input into the message; other inputs follow."
        />
        <div className="ins-two">
          <NumberField
            label="Context tokens"
            value={data.context_size}
            min={1}
            onChange={(context_size) => update({ context_size })}
          />
          <NumberField
            label="Temperature"
            value={data.temperature}
            step={0.1}
            onChange={(temperature) => update({ temperature })}
            help={
              data.provider === "anthropic"
                ? "This provider does not receive sampling parameters."
                : undefined
            }
          />
        </div>
        <Select
          label="Output format"
          value={data.output}
          options={outputOptions}
          onChange={(output) => update({ output })}
        />
        {upstream.length > 0 && (
          <section className="ins-inputs">
            <h3>Connected inputs</h3>
            {upstream.map((input) => {
              const references = (template: string) =>
                [...template.matchAll(/\{\{\s*([^{}]+?)\s*\}\}/g)].some(
                  (match) =>
                    [input.alias, input.handle].includes(match[1].trim()),
                );
              const inPrompt = references(data.prompt);
              const inMessage = references(data.message || "");
              const previous =
                results?.[input.source?.id || ""] ||
                activeRun?.nodes[input.source?.id || ""];
              const port = input.handle.split(".")[1];
              const output = port ? previous?.named_outputs?.[port] : previous;
              return (
                <div key={input.handle}>
                  <span className="ins-port-dot" />
                  <span
                    title={output?.error || output?.label || "Not run yet"}
                  >{`{{${input.alias}}}`}</span>
                  <small>
                    {inPrompt && inMessage
                      ? "Prompt + message"
                      : inPrompt
                        ? "Prompt"
                        : inMessage
                          ? "Message"
                          : "Appended to message"}
                  </small>
                </div>
              );
            })}
          </section>
        )}
      </>
    );
  }

  function renderDatabase(data: Database) {
    return (
      <>
        <Select
          label="Engine"
          value={data.engine}
          options={[
            ["sqlite", "SQLite"],
            ["duckdb", "DuckDB"],
            ["postgres", "PostgreSQL"],
            ["mysql", "MySQL"],
          ]}
          onChange={(engine) => update({ engine })}
        />
        {["sqlite", "duckdb"].includes(data.engine) ? (
          <Text
            label="Database file"
            value={data.path}
            choices={library.data}
            onChange={(path) => update({ path })}
            help="Relative to data/."
            mono
          />
        ) : (
          <Text
            label="Connection URL environment variable"
            value={data.url_env}
            onChange={(url_env) => update({ url_env })}
            help="The connection URL and its password stay in your shell."
            mono
          />
        )}
        <div className="ins-two">
          <Text
            label="Input data"
            value={data.input}
            choices={library.data}
            onChange={(input) => update({ input })}
            help="Optional file from data/."
          />
          <Text
            label="Input table"
            value={data.input_table}
            onChange={(input_table) => update({ input_table })}
          />
        </div>
        <Text
          label="Query"
          multiline
          mono
          rows={8}
          value={data.query}
          onChange={(query) => update({ query })}
          help="One statement. Leave empty to use SQL arriving on the wire."
        />
        <Toggle
          label="Read-only queries"
          value={data.read_only}
          onChange={(read_only) => update({ read_only })}
          help={
            data.read_only
              ? "Only SELECT-style statements can run."
              : "Writes are allowed. This node appears red on the canvas."
          }
        />
        {!readOnly && (
          <details className="ins-details">
            <summary>Schema & SQL assistant</summary>
            <div className="ins-details-body">
              <button
                type="button"
                disabled={busy}
                onClick={() =>
                  void perform(async () => {
                    const text = await request<string>("describe_schema", {
                      config: bareConfig(data),
                    });
                    setSchema({ node: selectedId, graph: graph.name, text });
                  })
                }
              >
                Inspect schema
              </button>
              {schema?.node === selectedId && schema.graph === graph.name && (
                <pre className="ins-output">
                  {schema.text || "No tables found."}
                </pre>
              )}
              <Select
                label="Saved agent"
                value={draft.agent || library.agents[0]?.name || ""}
                options={library.agents
                  .filter((agent) => agent.config.model_type === "generative")
                  .map((agent) => [agent.name, agent.name])}
                onChange={(agent) => setDraft({ agent })}
              />
              <Text
                label="What should the query do?"
                value={draft.sql || ""}
                onChange={(sql) => setDraft({ sql })}
                multiline
                rows={3}
              />
              {!library.agents.length && (
                <p className="ins-help">
                  Save an agent preset to write queries with it.
                </p>
              )}
              <button
                type="button"
                disabled={
                  busy ||
                  !draft.sql?.trim() ||
                  !library.agents.some(
                    (agent) => agent.config.model_type === "generative",
                  )
                }
                onClick={() =>
                  void perform(async () => {
                    const agent =
                      library.agents.find(
                        (item) =>
                          item.name === draft.agent &&
                          item.config.model_type === "generative",
                      ) ||
                      library.agents.find(
                        (item) => item.config.model_type === "generative",
                      );
                    if (!agent)
                      throw new Error("Choose a generative agent preset.");
                    const query = await request<string>("write_sql", {
                      agent: agent.config,
                      config: bareConfig(data),
                      question: draft.sql,
                    });
                    updateAsync({ query });
                  }, "Query ready to review")
                }
              >
                {busy ? "Working…" : "Write query"}
              </button>
              <p className="ins-help">
                The agent receives your request and schema. Review its query
                above before running.
              </p>
            </div>
          </details>
        )}
      </>
    );
  }

  function renderWorkflow(data: Workflow) {
    const outputs = child?.graph.outputs || {};
    return (
      <>
        <Select
          label="Saved workflow"
          value={data.graph}
          options={[
            ["", "Choose a saved graph…"],
            ...library.graphs
              .filter((name) => name !== graph.name)
              .map((name) => [name, name]),
          ]}
          onChange={(name) => update({ graph: name, output: "", inputs: {} })}
        />
        <Select
          label="Outputs"
          value={data.output}
          options={[
            ["", "Published outputs"],
            ...(child?.graph.nodes || []).map((item) => [
              item.id,
              `Node result · ${item.name || item.id}`,
            ]),
          ]}
          onChange={(output) => update({ output })}
          help="Published outputs create labeled ports. Selecting a node produces one output."
        />
        {data.output === "" && Object.keys(outputs).length > 0 && (
          <div className="ins-chips">
            {Object.keys(outputs).map((name) => (
              <span key={name}>{name}</span>
            ))}
          </div>
        )}
        <button
          type="button"
          disabled={!data.graph || !onOpenWorkflow}
          onClick={() => onOpenWorkflow?.(selectedId)}
        >
          Open child graph <span aria-hidden="true">↗</span>
        </button>
        {childError && (
          <p role="alert" className="ins-error">
            {childError}
          </p>
        )}
        {child && child.fields.length > 0 && (
          <section className="ins-mappings">
            <h3>Input mappings</h3>
            {child.fields.map((field) => {
              const binding = data.inputs[field.name];
              const source = !binding
                ? "default"
                : binding.kind === "value"
                  ? "value"
                  : binding.node;
              const setBinding = (binding?: Binding) => {
                const inputs = { ...data.inputs };
                if (binding) inputs[field.name] = binding;
                else delete inputs[field.name];
                update({ inputs });
              };
              return (
                <div className="ins-mapping" key={field.name}>
                  <Select
                    label={field.name}
                    value={source}
                    options={[
                      ["default", "Use default"],
                      ["value", "Literal value"],
                      ...upstream.map((input) => [
                        input.handle,
                        `Output · ${input.label}`,
                      ]),
                    ]}
                    onChange={(value) =>
                      setBinding(
                        value === "default"
                          ? undefined
                          : value === "value"
                            ? { kind: "value", value: "" }
                            : { kind: "output", node: value },
                      )
                    }
                    help={`${field.config.kind} · ${field.config.required ? "required" : "optional"}`}
                  />
                  {binding?.kind === "value" ? (
                    <Text
                      label={`${field.name} value`}
                      value={binding.value}
                      multiline
                      rows={2}
                      onChange={(value) => setBinding({ kind: "value", value })}
                    />
                  ) : (
                    source === "default" && (
                      <p className="ins-help">
                        {field.config.default
                          ? `Default: ${field.config.default}`
                          : "No default value"}
                      </p>
                    )
                  )}
                </div>
              );
            })}
          </section>
        )}
      </>
    );
  }

  function renderForm(data: Config) {
    switch (data.type) {
      case "agent":
        return renderAgent(data);
      case "database":
        return renderDatabase(data);
      case "workflow":
        return renderWorkflow(data);
      case "input":
        return (
          <>
            <Select
              label="Input type"
              value={data.kind}
              options={[
                ["text", "Text"],
                ["number", "Number"],
                ["boolean", "Boolean"],
                ["json", "JSON"],
                ["file", "File from data/"],
              ]}
              onChange={(kind) => update({ kind })}
            />
            <Text
              label="Default value"
              multiline
              value={data.default}
              onChange={(value) => update({ default: value })}
              help="Used when the run does not supply a value. File paths are relative to data/."
            />
            <Toggle
              label="A value is required"
              value={data.required}
              onChange={(required) => update({ required })}
            />
            <p className="ins-help">
              The node’s name becomes its field name in the run form. Wire it to
              the nodes that use this value.
            </p>
          </>
        );
      case "script":
        return (
          <>
            <Select
              label="Runtime"
              value={data.runtime}
              options={[
                ["python", "Python · uv"],
                ["shell", "Shell"],
                ["node", "Node.js"],
              ]}
              onChange={(runtime) => update({ runtime })}
            />
            <Text
              label="Script file"
              value={data.path}
              choices={library.scripts}
              onChange={(path) => update({ path })}
              help="Relative to scripts/."
              mono
            />
            <Lines
              label="Arguments"
              value={data.args}
              onChange={(args) => update({ args })}
              help="One argument per line. Spaces inside a line stay in that argument."
            />
            <NumberField
              label="Timeout in seconds"
              value={data.timeout_secs}
              min={1}
              onChange={(timeout_secs) => update({ timeout_secs })}
            />
          </>
        );
      case "data":
        return (
          <>
            <Text
              label="File"
              value={data.path}
              choices={library.data}
              onChange={(path) => update({ path })}
              help="Relative to data/."
              mono
            />
            <Select
              label="Format"
              value={data.format}
              options={[
                ["csv", "CSV"],
                ["tsv", "TSV"],
                ["json", "JSON"],
                ["text", "Text"],
                ["markdown", "Markdown"],
                ["image", "Image"],
                ["pdf", "PDF"],
                ["parquet", "Parquet"],
                ["binary", "Binary"],
              ]}
              onChange={(format) => update({ format })}
            />
          </>
        );
      case "directory":
        return (
          <>
            <Text
              label="Directory"
              value={data.path}
              onChange={(path) => update({ path })}
              help="Relative to the workspace; . reads the workspace itself. Hidden entries and symlinks are skipped."
              mono
            />
            <Lines
              label="Include patterns"
              value={data.include}
              onChange={(include) => update({ include })}
              help="One glob per line, for example src/**/*.rs. Empty includes every file."
            />
            <Lines
              label="Exclude patterns"
              value={data.exclude}
              onChange={(exclude) => update({ exclude })}
            />
            <Toggle
              label="Respect .gitignore files"
              value={data.gitignore}
              onChange={(gitignore) => update({ gitignore })}
            />
            <Toggle
              label="Include file contents"
              value={data.contents}
              onChange={(contents) => update({ contents })}
            />
            <div className="ins-two">
              <NumberField
                label="Maximum files"
                value={data.max_files}
                min={1}
                onChange={(max_files) => update({ max_files })}
              />
              <NumberField
                label="Content budget · bytes"
                value={data.max_bytes}
                min={1}
                onChange={(max_bytes) => update({ max_bytes })}
              />
            </div>
            <p className="ins-help">
              Contents stop when the budget is spent. Keep this within the
              downstream agent’s context window.
            </p>
          </>
        );
      case "file":
        return (
          <>
            <Text
              label={data.bundle ? "Output directory" : "Output file"}
              value={data.path}
              onChange={(path) => update({ path })}
              help="Relative to the workspace. Missing directories are created. App storage folders are reserved."
              mono
            />
            <Select
              label="If a file exists"
              value={data.mode}
              options={[
                ["create", "Refuse to overwrite"],
                ["overwrite", "Overwrite"],
                ["append", "Append"],
              ]}
              onChange={(mode) => update({ mode })}
            />
            <Toggle
              label="Write a bundle of files"
              value={data.bundle}
              onChange={(bundle) => update({ bundle })}
              help="Split === path === sections into files under the output directory. All paths are checked before writing."
            />
            <Toggle
              label="Review changes before writing"
              value={Boolean(data.review)}
              onChange={(review) => update({ review })}
              help="Pause for a diff of every file. Apply the batch to continue or reject it."
            />
            <Toggle
              label="Strip an outer code fence"
              value={data.unfence}
              onChange={(unfence) => update({ unfence })}
              help="Also end the text with a newline."
            />
          </>
        );
    }
  }

  return (
    <aside className="inspector" aria-label="Node inspector">
      <header className="ins-header">
        <div>
          <span className="ins-eyebrow">
            {readOnly ? "CHILD GRAPH · INSPECT" : "NODE INSPECTOR"}
          </span>
          <h2>
            {node
              ? node.name || (config ? LABELS[config.type] : "Untyped node")
              : "Your workspace"}
          </h2>
        </div>
        {node && <span className="ins-node-id">{node.id}</span>}
      </header>
      <div className="ins-tabs" role="tablist" aria-label="Inspector view">
        {(["configure", "results", "history"] as const).map((item) => (
          <button
            key={item}
            type="button"
            role="tab"
            aria-selected={tab === item}
            aria-controls={`ins-${item}`}
            id={`ins-tab-${item}`}
            tabIndex={tab === item ? 0 : -1}
            onClick={() => setTab(item)}
            onKeyDown={(event) => {
              if (
                !["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)
              )
                return;
              event.preventDefault();
              const tabs = ["configure", "results", "history"] as const;
              const index = tabs.indexOf(tab);
              const next =
                event.key === "Home"
                  ? 0
                  : event.key === "End"
                    ? 2
                    : (index + (event.key === "ArrowRight" ? 1 : 2)) % 3;
              setTab(tabs[next]);
              document.getElementById(`ins-tab-${tabs[next]}`)?.focus();
            }}
          >
            {item[0].toUpperCase() + item.slice(1)}
            {item === "results" && running && <span className="ins-live-dot" />}
          </button>
        ))}
      </div>
      <div
        className="ins-scroll"
        role="tabpanel"
        id={`ins-${tab}`}
        aria-labelledby={`ins-tab-${tab}`}
      >
        {notice && (
          <p
            className={notice.error ? "ins-notice ins-error" : "ins-notice"}
            role={notice.error ? "alert" : "status"}
          >
            {notice.text}
          </p>
        )}
        {node && !readOnly && config && (onRun || onDelete) && (
          <div className="ins-node-actions">
            {onRun && (
              <>
                <button
                  className="ins-run-button"
                  type="button"
                  disabled={running || issues.length > 0}
                  onClick={() => onRun(false)}
                >
                  Run node <span aria-hidden="true">↗</span>
                </button>
                <button
                  type="button"
                  title="Run without reusing cached outputs"
                  disabled={running || issues.length > 0}
                  onClick={() => onRun(true)}
                >
                  Run fresh
                </button>
              </>
            )}
            {onDelete && (
              <button
                type="button"
                className="ins-delete-button"
                aria-label="Delete selected node"
                title="Delete node"
                disabled={running}
                onClick={onDelete}
              >
                ×
              </button>
            )}
          </div>
        )}
        {tab === "configure" &&
          (node ? (
            <>
              <fieldset className="ins-form" disabled={readOnly}>
                <Text
                  label="Name"
                  value={node.name || ""}
                  onChange={(name) => onChange({ ...node, name })}
                  placeholder="Give this node a name"
                  help="Use this name in downstream {{name}} placeholders."
                />
                {config ? (
                  <>
                    <div className="ins-section-label">
                      <span style={{ background: COLORS[config.type] }} />
                      {LABELS[config.type]}
                    </div>
                    {renderForm(config)}
                  </>
                ) : (
                  <section className="ins-empty">
                    <h3>Give this node a purpose</h3>
                    <p>Choose the kind of work it should do.</p>
                    <div className="ins-type-grid">
                      {(Object.keys(DEFAULTS) as Config["type"][]).map(
                        (type) => (
                          <button
                            type="button"
                            key={type}
                            onClick={() =>
                              onChange({
                                ...node,
                                data: structuredClone(
                                  DEFAULTS[type],
                                ) as NodeData,
                                color: COLORS[type],
                              })
                            }
                          >
                            <span style={{ background: COLORS[type] }} />
                            {LABELS[type]}
                          </button>
                        ),
                      )}
                      <button
                        type="button"
                        onClick={() =>
                          onChange({
                            ...node,
                            data: {
                              ...structuredClone(DEFAULTS.database),
                              read_only: false,
                            } as NodeData,
                            color: "#ef4444",
                          })
                        }
                      >
                        <span style={{ background: "#ef4444" }} />
                        Database (write)
                      </button>
                    </div>
                  </section>
                )}
              </fieldset>
              {readOnly && config?.type === "workflow" && (
                <button
                  type="button"
                  disabled={!onOpenWorkflow}
                  onClick={() => onOpenWorkflow?.(selectedId)}
                >
                  Open child graph ↗
                </button>
              )}
              {issues.length > 0 && (
                <div className="ins-validation" role="status">
                  <h3>Check configuration</h3>
                  <ul>
                    {issues.map((issue, index) => (
                      <li key={`${issue.field}-${index}`}>
                        <strong>{issue.field}</strong> {issue.message}
                      </li>
                    ))}
                  </ul>
                </div>
              )}
              {config && !readOnly && (
                <>
                  {(onGraphChange || onPublish) && (
                    <Publication
                      graph={graph}
                      node={node}
                      workflowGraphs={props.workflowGraphs}
                      onPublish={onPublish}
                      onGraphChange={onGraphChange}
                    />
                  )}
                  {(config.type === "agent" || config.type === "database") && (
                    <details className="ins-details">
                      <summary>Reusable {config.type} preset</summary>
                      <div className="ins-details-body">
                        <Select
                          label="Load preset"
                          value=""
                          options={[
                            ["", "Choose a preset…"],
                            ...(config.type === "agent"
                              ? library.agents
                              : library.databases
                            ).map((item) => [item.name, item.name]),
                          ]}
                          onChange={(name) => {
                            const preset = (
                              config.type === "agent"
                                ? library.agents
                                : library.databases
                            ).find((item) => item.name === name);
                            if (preset) {
                              update({ ...preset.config });
                              setDraft({ preset: name });
                            }
                          }}
                        />
                        <Text
                          label="Preset name"
                          value={draft.preset || ""}
                          onChange={(preset) => setDraft({ preset })}
                        />
                        <button
                          type="button"
                          disabled={
                            busy || !draft.preset?.trim() || issues.length > 0
                          }
                          onClick={() =>
                            void perform(async () => {
                              await request("save_preset", {
                                type: config.type,
                                name: draft.preset,
                                config: bareConfig(config),
                              });
                              onLibraryChanged?.();
                            }, "Preset saved")
                          }
                        >
                          Save {config.type}
                        </button>
                      </div>
                    </details>
                  )}
                  <details className="ins-details">
                    <summary>Node options</summary>
                    <div className="ins-details-body">
                      <p className="ins-help">
                        Replace this node’s configuration with a different kind
                        of step.
                      </p>
                      <div className="ins-type-grid">
                        {(Object.keys(DEFAULTS) as Config["type"][]).map(
                          (type) => (
                            <button
                              type="button"
                              key={type}
                              disabled={
                                running ||
                                (type === config.type &&
                                  !(
                                    config.type === "database" &&
                                    !config.read_only
                                  ))
                              }
                              onClick={() =>
                                onChange({
                                  ...node,
                                  data: structuredClone(
                                    DEFAULTS[type],
                                  ) as NodeData,
                                  color: COLORS[type],
                                })
                              }
                            >
                              <span style={{ background: COLORS[type] }} />
                              {LABELS[type]}
                            </button>
                          ),
                        )}
                        <button
                          type="button"
                          disabled={
                            running ||
                            (config.type === "database" && !config.read_only)
                          }
                          onClick={() =>
                            onChange({
                              ...node,
                              data: {
                                ...structuredClone(DEFAULTS.database),
                                read_only: false,
                              } as NodeData,
                              color: "#ef4444",
                            })
                          }
                        >
                          <span style={{ background: "#ef4444" }} />
                          Database (write)
                        </button>
                      </div>
                      <button
                        type="button"
                        disabled={running}
                        onClick={() => onChange({ ...node, data: {} })}
                      >
                        Make blank
                      </button>
                    </div>
                  </details>
                </>
              )}
            </>
          ) : (
            <div className="ins-empty">
              <div className="ins-empty-symbol" aria-hidden="true">
                ⌘
              </div>
              <h3>A place for every step</h3>
              <p>
                Select a node to configure its work, inspect its output, or
                revisit an earlier run.
              </p>
              <p className="ins-help">
                Connect nodes from an output port to an input port to build your
                workflow.
              </p>
            </div>
          ))}
        {tab === "results" && (
          <div className="ins-results">
            {running && (
              <p className="ins-run-status">
                <span className="ins-live-dot" />
                Execution in progress
              </p>
            )}
            {node &&
              currentResult?.data &&
              !sameNodeConfig(
                currentResult.data,
                config as NodeData | null,
              ) && (
                <p className="ins-notice">
                  This node has changed since this result was produced.
                </p>
              )}
            {node && stream?.id === node.id ? (
              <section>
                <h3>Streaming output</h3>
                <pre className="ins-output" aria-live="off">
                  {stream.text || "Waiting for the first token…"}
                </pre>
              </section>
            ) : currentResult ? (
              <Output
                key={`${activeRun?.id}-${selectedId}`}
                result={currentResult}
                node={selectedId}
                context={
                  props.outputContext ||
                  (activeRun
                    ? { graph: activeRun.graph, runId: activeRun.id }
                    : undefined)
                }
              />
            ) : (
              <div className="ins-empty">
                <h3>
                  {node
                    ? "No result for this node yet"
                    : "Select a node to inspect its result"}
                </h3>
                <p>Run your graph to see outputs and activity here.</p>
              </div>
            )}
            {currentResult && previousRun && node && (
              <PreviousOutput
                current={currentResult}
                previous={previousRun.nodes[node.id]}
                run={previousRun}
                node={node.id}
              />
            )}
            {node && config?.type === "workflow" && (
              <section className="ins-child-result">
                <div className="ins-section-heading">
                  <h3>Child workflow</h3>
                  <span className="ins-count">{config.graph}</span>
                </div>
                {activeRun?.calls?.[node.id] && (
                  <p className="ins-help">
                    {Object.keys(activeRun.calls[node.id].run.nodes).length}{" "}
                    steps ·{" "}
                    {activeRun.calls[node.id].run.outcome.kind === "ok"
                      ? "Completed"
                      : activeRun.calls[node.id].run.outcome.kind}
                  </p>
                )}
                <button
                  type="button"
                  disabled={!onOpenWorkflow || !config.graph}
                  onClick={() => onOpenWorkflow?.(node.id)}
                >
                  Open child graph ↗
                </button>
              </section>
            )}
            {(props.log || activeRun?.log || []).length > 0 && (
              <details className="ins-details" open={running}>
                <summary>
                  Execution log{" "}
                  <span className="ins-count">
                    {(props.log || activeRun?.log || []).length}
                  </span>
                </summary>
                <pre className="ins-output ins-log">
                  {(props.log || activeRun?.log || []).join("\n")}
                </pre>
              </details>
            )}
          </div>
        )}
        {tab === "history" && (
          <div className="ins-history">
            <div className="ins-section-heading">
              <h3>Previous runs</h3>
              <span className="ins-count">{history.length}</span>
            </div>
            {!history.length && (
              <div className="ins-empty">
                <h3>A record of your work</h3>
                <p>
                  Finished runs appear here with their inputs, outputs, and
                  execution log.
                </p>
              </div>
            )}
            {history.map((run) => (
              <details
                className={`ins-run ${activeRun?.id === run.id ? "is-active" : ""}`}
                key={run.id}
              >
                <summary>
                  <span
                    className={`ins-outcome ins-outcome-${run.outcome.kind}`}
                  >
                    {run.outcome.kind === "ok"
                      ? "✓"
                      : run.outcome.kind === "cancelled"
                        ? "−"
                        : "!"}
                  </span>
                  <span className="ins-run-heading">
                    <strong>{runTime(run.id)}</strong>
                    <small>
                      {run.targets.length} target
                      {run.targets.length === 1 ? "" : "s"} ·{" "}
                      {(run.elapsed_ms / 1000).toFixed(1)}s ·{" "}
                      {run.outcome.kind === "ok"
                        ? "Completed"
                        : run.outcome.kind}
                    </small>
                  </span>
                </summary>
                <div className="ins-run-body">
                  {run.outcome.error && (
                    <p className="ins-error">{run.outcome.error}</p>
                  )}
                  {!readOnly && onOpenHistory && (
                    <button type="button" onClick={() => onOpenHistory(run.id)}>
                      Inspect this run
                    </button>
                  )}
                  {run.inputs && Object.keys(run.inputs).length > 0 && (
                    <details className="ins-details">
                      <summary>Run inputs</summary>
                      <dl>
                        {Object.entries(run.inputs).map(([name, value]) => (
                          <div key={name}>
                            <dt>{name}</dt>
                            <dd>{value}</dd>
                          </div>
                        ))}
                      </dl>
                    </details>
                  )}
                  {node && run.nodes[node.id] ? (
                    <Output
                      key={node.id}
                      result={run.nodes[node.id]}
                      title={`${node.name || node.id} output`}
                      node={node.id}
                      context={
                        readOnly && props.outputContext
                          ? props.outputContext
                          : { graph: run.graph, runId: run.id }
                      }
                    />
                  ) : (
                    <p className="ins-help">
                      {node
                        ? "This node did not take part in this run."
                        : "Select a node to see what it produced."}
                    </p>
                  )}
                </div>
              </details>
            ))}
          </div>
        )}
      </div>
    </aside>
  );
}

function Publication({
  graph,
  node,
  workflowGraphs = {},
  onPublish,
  onGraphChange,
}: {
  graph: GraphDoc;
  node: NodeDoc;
  workflowGraphs?: WorkflowGraphs;
  onPublish?: (handle: string, name: string) => void;
  onGraphChange?: (graph: GraphDoc) => void;
}) {
  const named = isNamedWorkflow(node);
  const ports = named
    ? workflowPorts(graph, node, workflowGraphs).map((port) => ({
        handle: `${node.id}.${port.id}`,
        label: `${port.id}${port.missing ? " · unavailable" : ""}`,
        missing: port.missing,
      }))
    : [{ handle: node.id, label: "Node result", missing: false }];
  const [handle, setHandle] = useState(ports[0]?.handle || node.id);
  const existing = Object.entries(graph.outputs || {})
    .filter(([, target]) => target === handle)
    .map(([name]) => name)
    .join("\n");
  const [names, setNames] = useState(existing);
  const [error, setError] = useState("");
  useEffect(() => {
    setHandle(ports[0]?.handle || node.id);
    setError("");
  }, [node.id]);
  const portFingerprint = ports.map((port) => port.handle).join("|");
  useEffect(() => {
    if (!ports.some((port) => port.handle === handle))
      setHandle(ports[0]?.handle || node.id);
  }, [portFingerprint, handle, node.id]);
  useEffect(() => {
    setNames(existing);
    setError("");
  }, [handle, existing]);
  function apply(next: string[]) {
    try {
      const doc = publishOutput(graph, handle, next, workflowGraphs);
      if (onGraphChange) onGraphChange(doc);
      else onPublish?.(handle, next[0] || "");
      setError("");
    } catch (error) {
      setError(errorMessage(error));
    }
  }
  return (
    <details className="ins-details">
      <summary>Published outputs</summary>
      <div className="ins-details-body">
        {named && (
          <Select
            label="Output port"
            value={handle}
            options={ports.map((port) => [port.handle, port.label])}
            onChange={setHandle}
          />
        )}
        {ports.length ? (
          <>
            <Text
              label="Published names"
              multiline
              rows={2}
              value={names}
              onChange={setNames}
              help="One alias per line. Another workflow can use these named outputs."
            />
            <div className="ins-button-row">
              <button
                type="button"
                disabled={
                  names === existing ||
                  !names.trim() ||
                  ports.find((port) => port.handle === handle)?.missing
                }
                onClick={() =>
                  apply(
                    names
                      .split("\n")
                      .map((name) => name.trim())
                      .filter(Boolean),
                  )
                }
              >
                Save names
              </button>
              {existing && (
                <button type="button" onClick={() => apply([])}>
                  Unpublish
                </button>
              )}
            </div>
          </>
        ) : (
          <p className="ins-help">
            The child graph has no published output ports.
          </p>
        )}
        {error && (
          <p className="ins-error" role="alert">
            {error}
          </p>
        )}
      </div>
    </details>
  );
}
function PreviousOutput({
  current,
  previous,
  run,
  node,
}: {
  current: InspectorNodeResult;
  previous: InspectorNodeResult;
  run: InspectorRun;
  node: string;
}) {
  const comparison = compareNodeOutputs(current, previous);
  return (
    <section className="ins-previous-result">
      <div className="ins-section-heading">
        <h3>Previous output</h3>
        <span className="ins-count">{runTime(run.id)}</span>
      </div>
      <p className="ins-help">{previous.label || "No output"}</p>
      {previous.error ? (
        <p className="ins-error">Failed then: {previous.error}</p>
      ) : (
        <>
          {comparison === "same" && (
            <p className="ins-help">Same output as now.</p>
          )}
          {comparison === "preview" && (
            <p className="ins-help">
              Same preview as now. The complete outputs may differ.
            </p>
          )}
          {comparison !== "same" && previous.preview && (
            <pre className="ins-output">{previous.preview}</pre>
          )}
          {comparison !== "same" && previous.preview && !previous.complete && (
            <p className="ins-help">Previous preview truncated.</p>
          )}
          {comparison === "unknown" && (
            <p className="ins-help">
              The stored previews do not contain enough information to compare
              these outputs.
            </p>
          )}
        </>
      )}
      {!sameNodeConfig(current.data, previous.data) && (
        <p className="ins-help">The node was configured differently then.</p>
      )}
      {Object.entries(previous.named_outputs || {}).length > 0 && (
        <details className="ins-details">
          <summary>Previous named outputs</summary>
          {Object.entries(previous.named_outputs || {}).map(
            ([name, output]) => (
              <Output
                key={name}
                title={name}
                result={output}
                node={node}
                outputName={name}
                context={{ graph: run.graph, runId: run.id }}
              />
            ),
          )}
        </details>
      )}
    </section>
  );
}

function Output({
  result,
  title = "Node result",
  context,
  node,
  outputName,
}: {
  result: InspectorNodeResult;
  title?: string;
  context?: OutputContext;
  node?: string;
  outputName?: string;
}) {
  const identity = JSON.stringify([
    context?.graph,
    context?.runId,
    context?.callPath,
    node,
    outputName,
    result.key,
    result.preview,
    result.label,
    result.error,
    result.complete,
  ]);
  const identityRef = useRef(identity);
  identityRef.current = identity;
  const [loaded, setLoaded] = useState<{
    identity: string;
    data: FullOutput;
  } | null>(null);
  const full = loaded?.identity === identity ? loaded.data : null;
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  useEffect(() => {
    setLoaded(null);
    setBusy(false);
    setError("");
  }, [identity]);
  async function load() {
    if (!context || !node) return;
    setBusy(true);
    setError("");
    try {
      const data = await request<FullOutput>("output", {
        graph: context.graph,
        run_id: context.runId,
        node,
        ...(context.callPath ? { call_path: context.callPath } : {}),
        ...(outputName ? { output: outputName } : {}),
      });
      if (identityRef.current === identity) setLoaded({ identity, data });
    } catch (error) {
      if (identityRef.current === identity) setError(errorMessage(error));
    } finally {
      if (identityRef.current === identity) setBusy(false);
    }
  }
  const text = full?.text ?? result.preview;
  const mediaType =
    full?.media_type ||
    (/jpe?g/i.test(full?.label || "")
      ? "image/jpeg"
      : /webp/i.test(full?.label || "")
        ? "image/webp"
        : "image/png");
  const image =
    full?.base64 &&
    (full.media_type?.startsWith("image/") ||
      /image|png|jpeg|jpg|webp/i.test(full.label));
  return (
    <section className="ins-result">
      <div className="ins-section-heading">
        <h3>{title}</h3>
        <span className={`ins-result-state ins-state-${result.state}`}>
          {result.state}
        </span>
      </div>
      {result.error ? (
        <p className="ins-error">{result.error}</p>
      ) : (
        <>
          {result.label && <p className="ins-help">{result.label}</p>}
          {image ? (
            <img
              className="ins-image-output"
              src={`data:${mediaType};base64,${full.base64}`}
              alt={`${title} generated image`}
            />
          ) : text ? (
            <pre className="ins-output">{text}</pre>
          ) : (
            <p className="ins-help">
              {result.label ? "Binary output" : "No text output."}
            </p>
          )}
          {!full && text && !result.complete && (
            <p className="ins-help">Preview truncated.</p>
          )}
          {full && !full.complete && (
            <p className="ins-help">
              Only a saved preview is available. The full output is no longer in
              the cache.
            </p>
          )}
          {context && node && !result.complete && !full && (
            <button type="button" disabled={busy} onClick={() => void load()}>
              {busy ? "Loading…" : "View full output"}
            </button>
          )}
          {full?.base64 && !image && (
            <a
              className="ins-download"
              download={result.saved?.split("/").pop() || "output.bin"}
              href={`data:${full.media_type || "application/octet-stream"};base64,${full.base64}`}
            >
              Download output
            </a>
          )}
          {error && (
            <p className="ins-error" role="alert">
              {error}
            </p>
          )}
          {result.saved && (
            <p className="ins-saved">
              <span>Saved to</span>
              <code>{result.saved}</code>
            </p>
          )}
        </>
      )}
      {Object.entries(result.named_outputs || {}).map(([name, output]) => (
        <Output
          key={name}
          title={name}
          result={output}
          context={context}
          node={node}
          outputName={name}
        />
      ))}
    </section>
  );
}
function runTime(id: string) {
  const match = /^(\d{4})(\d{2})(\d{2})T(\d{2})(\d{2})(\d{2})(\d{3})Z$/.exec(
    id,
  );
  if (!match) return id;
  const date = new Date(
    `${match[1]}-${match[2]}-${match[3]}T${match[4]}:${match[5]}:${match[6]}.${match[7]}Z`,
  );
  return Number.isNaN(date.getTime())
    ? id
    : date.toLocaleString(undefined, {
        month: "short",
        day: "numeric",
        hour: "numeric",
        minute: "2-digit",
        second: "2-digit",
      });
}
