import type {
  Binding,
  Field,
  GraphDoc,
  Library,
  NodeData,
  NodeDoc,
  NodeRun,
  Run,
} from "../types";
import { request } from "../api";
import {
  EnvStatus,
  Lines,
  NumberField,
  Select,
  Text,
  Toggle,
} from "./fields";

type Config = NodeData;
type Agent = Extract<NodeData, { type: "agent" }>;
type Database = Extract<NodeData, { type: "database" }>;
type Workflow = Extract<NodeData, { type: "workflow" }>;
type InspectorLibrary = Pick<
  Library,
  "agents" | "databases" | "prompts" | "graphs" | "scripts" | "data"
>;

const PROVIDERS: readonly (readonly [Agent["provider"], string])[] = [
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
  Agent["provider"],
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

function bareConfig<T extends Config>(data: T) {
  const { type: _type, ...config } = data;
  return config;
}

export type NodeConfigFormProps = {
  data: Config;
  graph: GraphDoc;
  selectedId: string;
  readOnly: boolean;
  busy: boolean;
  revision: number;
  draft: { preset?: string; prompt?: string; sql?: string; agent?: string };
  setDraft: (patch: {
    preset?: string;
    prompt?: string;
    sql?: string;
    agent?: string;
  }) => void;
  library: InspectorLibrary;
  models: { key: string; names: string[] } | null;
  setModels: (value: { key: string; names: string[] } | null) => void;
  schema: { revision: number; text: string } | null;
  setSchema: (value: { revision: number; text: string } | null) => void;
  child: { name: string; graph: GraphDoc; fields: Field[] } | null;
  childError: string;
  upstream: {
    handle: string;
    alias: string;
    label: string;
    source: NodeDoc | undefined;
  }[];
  results?: Record<string, NodeRun>;
  activeRun?: Run | null;
  onOpenWorkflow?: (nodeId: string) => void;
  onLibraryChanged?: () => void;
  update: <T extends Config>(
    original: T,
    patch: Partial<Omit<NoInfer<T>, "type">>,
  ) => void;
  updateAsync: <T extends Config>(
    original: T,
    patch: Partial<Omit<NoInfer<T>, "type">>,
  ) => boolean;
  perform: (
    action: (isActive: () => boolean) => Promise<unknown>,
    success?: string,
  ) => Promise<void>;
};

export default function NodeConfigForm(props: NodeConfigFormProps) {
  const {
    data,
    graph,
    selectedId,
    readOnly,
    busy,
    revision,
    draft,
    setDraft,
    library,
    models,
    setModels,
    schema,
    setSchema,
    child,
    childError,
    upstream,
    results,
    activeRun,
    onOpenWorkflow,
    onLibraryChanged,
    update,
    updateAsync,
    perform,
  } = props;

function renderAgent(data: Agent) {
  const local = ["ollama", "lmstudio", "llamacpp"].includes(data.provider);
  const modelKey = `${data.provider}|${data.endpoint.trim()}`;
  const listed = models?.key === modelKey ? models : null;
  const providerName =
    PROVIDERS.find(([id]) => id === data.provider)?.[1] ?? data.provider;
  const modelOptions: (readonly [Agent["model_type"], string])[] = [
    ["generative", "Generative"],
    ...(data.provider === "anthropic"
      ? []
      : [["embedding", "Embedding"] as const]),
    ...(["openai", "custom"].includes(data.provider)
      ? [["image", "Image"] as const]
      : []),
  ];
  const outputOptions: (readonly [Agent["output"], string])[] =
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
          update(data, {
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
        onChange={(endpoint) => update(data, { endpoint })}
        help="Leave empty for the provider’s default API endpoint."
        mono
      />
      <div className="ins-two">
        <Select
          label="Model type"
          value={data.model_type}
          options={modelOptions}
          onChange={(model_type) =>
            update(data, {
              model_type,
              output: model_type === "generative" ? "text" : model_type,
            })
          }
        />
        <Text
          label="Model"
          value={data.model}
          choices={listed?.names}
          onChange={(model) => update(data, { model })}
          help={
            listed &&
            data.model.trim() &&
            !listed.names.includes(data.model.trim())
              ? `Not among the models ${providerName} listed.`
              : undefined
          }
        />
      </div>
      {!readOnly && (
        <div className="ins-button-row ins-models">
          <button
            type="button"
            disabled={busy}
            title="Ask the provider which model ids it offers"
            onClick={() =>
              void perform(async (isActive) => {
                const names = await request("list_models", {
                  config: bareConfig(data),
                });
                if (isActive()) setModels({ key: modelKey, names });
              }, "Models listed")
            }
          >
            Fetch models
          </button>
          {listed && (
            <span className="ins-help">
              {listed.names.length} model
              {listed.names.length === 1 ? "" : "s"} from {providerName};
              choose one in the Model field.
            </span>
          )}
        </div>
      )}
      {!local && (
        <>
          <Text
            label="API key environment variable"
            value={data.auth.kind === "env" ? data.auth.var : ""}
            onChange={(variable) =>
              update(data, {
                auth: variable
                  ? { kind: "env", var: variable }
                  : { kind: "none" },
              })
            }
            placeholder={PROVIDER_DEFAULTS[data.provider]?.env}
            help="Only the variable name is saved. The key stays in your shell."
            mono
          />
          {data.auth.kind === "env" && <EnvStatus name={data.auth.var} />}
        </>
      )}
      <Text
        label="System prompt"
        value={data.prompt}
        multiline
        rows={7}
        onChange={(prompt) => update(data, { prompt })}
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
                ...library.prompts.map((name) => [name, name] as const),
              ]}
              onChange={(name) => {
                if (name)
                  void perform(async (isActive) => {
                    const prompt = await request("load_prompt", {
                      name,
                    });
                    if (isActive() && updateAsync(data, { prompt }))
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
                      name: draft.prompt || "",
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
        onChange={(message) => update(data, { message })}
        help="Empty sends all inputs together. Place {{name}} here to direct an input into the message; other inputs follow."
      />
      <div className="ins-two">
        <NumberField
          label="Context tokens"
          value={data.context_size}
          min={1}
          onChange={(context_size) => update(data, { context_size })}
        />
        <NumberField
          label="Temperature"
          value={data.temperature}
          step={0.1}
          onChange={(temperature) => update(data, { temperature })}
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
        onChange={(output) => update(data, { output })}
      />
      {upstream.length > 0 && (
        <section className="ins-inputs">
          <h3>Connected inputs</h3>
          {upstream.map((input) => {
            const references = (template: string) =>
              [...template.matchAll(/\{\{\s*([^{}]+?)\s*\}\}/g)].some(
                (match) =>
                  [input.alias, input.handle].includes(
                    match[1]?.trim() || "",
                  ),
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
        onChange={(engine) => update(data, { engine })}
      />
      {["sqlite", "duckdb"].includes(data.engine) ? (
        <Text
          label="Database file"
          value={data.path}
          choices={library.data}
          onChange={(path) => update(data, { path })}
          help="Relative to data/."
          mono
        />
      ) : (
        <>
          <Text
            label="Connection URL environment variable"
            value={data.url_env}
            onChange={(url_env) => update(data, { url_env })}
            help="The connection URL and its password stay in your shell."
            mono
          />
          <EnvStatus name={data.url_env} />
        </>
      )}
      <div className="ins-two">
        <Text
          label="Input data"
          value={data.input}
          choices={library.data}
          onChange={(input) => update(data, { input })}
          help="Optional file from data/."
        />
        <Text
          label="Input table"
          value={data.input_table}
          onChange={(input_table) => update(data, { input_table })}
        />
      </div>
      <Text
        label="Query"
        multiline
        mono
        rows={8}
        value={data.query}
        onChange={(query) => update(data, { query })}
        help="One statement. Leave empty to use SQL arriving on the wire."
      />
      <Toggle
        label="Read-only queries"
        value={data.read_only}
        onChange={(read_only) => update(data, { read_only })}
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
                void perform(async (isActive) => {
                  const text = await request("describe_schema", {
                    config: bareConfig(data),
                  });
                  if (isActive()) setSchema({ revision, text });
                })
              }
            >
              Inspect schema
            </button>
            {schema?.revision === revision && (
              <pre className="ins-output">
                {schema.text || "No tables found."}
              </pre>
            )}
            <Select
              label="Saved agent"
              value={draft.agent || library.agents[0]?.name || ""}
              options={library.agents
                .filter((agent) => agent.config.model_type === "generative")
                .map((agent) => [agent.name, agent.name] as const)}
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
                void perform(async (isActive) => {
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
                  const query = await request("write_sql", {
                    agent: agent.config,
                    config: bareConfig(data),
                    question: draft.sql || "",
                  });
                  if (isActive()) updateAsync(data, { query });
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
            .map((name) => [name, name] as const),
        ]}
        onChange={(name) =>
          update(data, { graph: name, output: "", inputs: {} })
        }
      />
      <Select
        label="Outputs"
        value={data.output}
        options={[
          ["", "Published outputs"],
          ...(child?.graph.nodes || []).map(
            (item) =>
              [item.id, `Node result · ${item.name || item.id}`] as const,
          ),
        ]}
        onChange={(output) => update(data, { output })}
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
              update(data, { inputs });
            };
            return (
              <div className="ins-mapping" key={field.name}>
                <Select
                  label={field.name}
                  value={source}
                  options={[
                    ["default", "Use default"],
                    ["value", "Literal value"],
                    ...upstream.map(
                      (input) =>
                        [input.handle, `Output · ${input.label}`] as const,
                    ),
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
            onChange={(kind) => update(data, { kind })}
          />
          <Text
            label="Default value"
            multiline
            value={data.default}
            onChange={(value) => update(data, { default: value })}
            help="Used when the run does not supply a value. File paths are relative to data/."
          />
          <Toggle
            label="A value is required"
            value={data.required}
            onChange={(required) => update(data, { required })}
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
            onChange={(runtime) => update(data, { runtime })}
          />
          <Text
            label="Script file"
            value={data.path}
            choices={library.scripts}
            onChange={(path) => update(data, { path })}
            help="Relative to scripts/."
            mono
          />
          <Lines
            label="Arguments"
            value={data.args}
            onChange={(args) => update(data, { args })}
            help="One argument per line. Spaces inside a line stay in that argument."
          />
          <NumberField
            label="Timeout in seconds"
            value={data.timeout_secs}
            min={1}
            onChange={(timeout_secs) => update(data, { timeout_secs })}
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
            onChange={(path) => update(data, { path })}
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
            onChange={(format) => update(data, { format })}
          />
        </>
      );
    case "directory":
      return (
        <>
          <Text
            label="Directory"
            value={data.path}
            onChange={(path) => update(data, { path })}
            help="Relative to the workspace; . reads the workspace itself. Hidden entries and symlinks are skipped."
            mono
          />
          <Lines
            label="Include patterns"
            value={data.include}
            onChange={(include) => update(data, { include })}
            help="One glob per line, for example src/**/*.rs. Empty includes every file."
          />
          <Lines
            label="Exclude patterns"
            value={data.exclude}
            onChange={(exclude) => update(data, { exclude })}
          />
          <Toggle
            label="Respect .gitignore files"
            value={data.gitignore}
            onChange={(gitignore) => update(data, { gitignore })}
          />
          <Toggle
            label="Include file contents"
            value={data.contents}
            onChange={(contents) => update(data, { contents })}
          />
          <div className="ins-two">
            <NumberField
              label="Maximum files"
              value={data.max_files}
              min={1}
              onChange={(max_files) => update(data, { max_files })}
            />
            <NumberField
              label="Content budget · bytes"
              value={data.max_bytes}
              min={1}
              onChange={(max_bytes) => update(data, { max_bytes })}
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
            onChange={(path) => update(data, { path })}
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
            onChange={(mode) => update(data, { mode })}
          />
          <Toggle
            label="Write a bundle of files"
            value={data.bundle}
            onChange={(bundle) => update(data, { bundle })}
            help="Split === path === sections into files under the output directory. All paths are checked before writing."
          />
          <Toggle
            label="Review changes before writing"
            value={Boolean(data.review)}
            onChange={(review) => update(data, { review })}
            help="Pause for a diff of every file. Apply the batch to continue or reject it."
          />
          <Toggle
            label="Strip an outer code fence"
            value={data.unfence}
            onChange={(unfence) => update(data, { unfence })}
            help="Also end the text with a newline."
          />
        </>
      );
  }
}


  return renderForm(data);
}
