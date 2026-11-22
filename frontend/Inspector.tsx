import { useEffect, useRef, useState } from "react";
import { request } from "./api";
import type {
  Field,
  GraphDoc,
  Issue,
  Library,
  NodeDoc,
  NodeData,
  NodeRun,
  NodeType,
  Run,
} from "./types";
import {
  defaultNodeData,
  nodeLabel,
  NODE_COLORS as COLORS,
  NODE_LABELS,
  typedData,
  type WorkflowGraphs,
} from "./graph";
import NodeOutputView, {
  type NodeOutputContext,
} from "./NodeOutputView";
import { errorText } from "./outputMedia";
import NodeConfigForm from "./inspector/NodeConfigForm";
import { Select, Text } from "./inspector/fields";
import Publication from "./inspector/Publication";
import PreviousOutput, { runTime } from "./inspector/PreviousOutput";
import { sameNodeConfig } from "./inspectorState";
import "./inspector.css";

type Config = NodeData;
type Agent = Extract<NodeData, { type: "agent" }>;
type Database = Extract<NodeData, { type: "database" }>;
type Workflow = Extract<NodeData, { type: "workflow" }>;
export type InspectorIssue = Issue;
export type InspectorNodeResult = NodeRun;
export type InspectorRun = Run;
export type InspectorLibrary = Pick<
  Library,
  "agents" | "databases" | "prompts" | "graphs" | "scripts" | "data"
>;
export type InspectorProps = {
  graph: GraphDoc;
  node: NodeDoc | null;
  readOnly?: boolean;
  scopeKey?: string | number;
  onChange: (node: NodeDoc) => void;
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

function outputContextFor(
  context: OutputContext | undefined,
  node: string,
): NodeOutputContext | undefined {
  return context ? { ...context, node } : undefined;
}

const EMPTY_LIBRARY: InspectorLibrary = {
  agents: [],
  databases: [],
  prompts: [],
  graphs: [],
  scripts: [],
  data: [],
};
const DEFAULTS = {
  input: defaultNodeData("input"),
  workflow: defaultNodeData("workflow"),
  agent: defaultNodeData("agent"),
  acp: defaultNodeData("acp"),
  database: defaultNodeData("database"),
  script: defaultNodeData("script"),
  data: defaultNodeData("data"),
  directory: defaultNodeData("directory"),
  file: defaultNodeData("file"),
} as { [K in NodeType]: Extract<Config, { type: K }> };
const NODE_TYPES = [
  "agent",
  "acp",
  "database",
  "input",
  "workflow",
  "script",
  "data",
  "directory",
  "file",
] as const satisfies readonly NodeType[];

function isWritableNode(data: Config): boolean {
  return (
    data.type === "file" ||
    ((data.type === "database" || data.type === "acp") && !data.read_only)
  );
}
function nextNodeColor(
  currentColor: string | null | undefined,
  original: Config,
  data: Config,
): string {
  const writable = isWritableNode(data);
  const defaultOrWrite =
    !currentColor ||
    currentColor === COLORS[data.type] ||
    currentColor === "#ef4444";
  const toggledReadOnly =
    (original.type === "database" || original.type === "acp") &&
    data.type === original.type &&
    original.read_only !== data.read_only;
  if (!currentColor || (toggledReadOnly && defaultOrWrite)) {
    return writable ? "#ef4444" : COLORS[data.type];
  }
  return currentColor;
}

const errorMessage = errorText;
/** Typed config for the selected node; blank or unknown shapes are null. */
function nodeConfig(node: NodeDoc | null): Config | null {
  return node ? typedData(node) ?? null : null;
}
function bareConfig<T extends Config>(data: T) {
  const { type: _type, ...config } = data;
  return config;
}

export default function Inspector(props: InspectorProps) {
  const {
    graph,
    node,
    readOnly = false,
    onChange,
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
    revision: number;
    text: string;
  } | null>(null);
  const [child, setChild] = useState<{
    name: string;
    graph: GraphDoc;
    fields: Field[];
  } | null>(null);
  const [childError, setChildError] = useState("");
  const [models, setModels] = useState<{
    key: string;
    names: string[];
  } | null>(null);
  const [acpOptions, setAcpOptions] = useState<{
    key: string;
    agent: string | null;
    models: string[];
    modes: string[];
  } | null>(null);
  const config = nodeConfig(node);
  const serialized = JSON.stringify(config);
  const selectedId = node?.id || "";
  const scope = JSON.stringify([
    props.scopeKey ?? graph.name,
    selectedId,
    readOnly,
  ]);
  const draftKey = scope;
  const draft = drafts[draftKey] || {};
  const setDraft = (patch: Partial<typeof draft>) =>
    setDrafts((previous) => ({
      ...previous,
      [draftKey]: { ...previous[draftKey], ...patch },
    }));
  const revisionState = useRef({ scope, serialized, revision: 0 });
  if (
    revisionState.current.scope !== scope ||
    revisionState.current.serialized !== serialized
  )
    revisionState.current = {
      scope,
      serialized,
      revision: revisionState.current.revision + 1,
    };
  const revision = revisionState.current.revision;
  const operation = useRef(0);
  const latest = useRef({ graph, node, readOnly, onChange, revision });
  latest.current = { graph, node, readOnly, onChange, revision };
  const isCurrent = () => latest.current.revision === revision;
  const workflowName = config?.type === "workflow" ? config.graph : "";
  const capturedGraph = props.workflowGraphs?.[selectedId];
  const capturedFingerprint = JSON.stringify(capturedGraph);

  useEffect(() => {
    // When App supplies issues (including []), trust that source. Local
    // validate_node remains for child/snapshot inspection without graph issues.
    if (props.issues || !config) {
      setLocalIssues([]);
      return;
    }
    let active = true;
    const timeout = window.setTimeout(() => {
      request("validate_node", {
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
  }, [serialized, node?.name, props.issues, scope]);

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
        : request("load_graph", { name: workflowName });
    source
      .then(async (doc) => {
        const call = nodeConfig(node);
        const targets =
          call?.type === "workflow" && call.output ? [call.output] : undefined;
        const fields = await request("workflow_fields", {
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
    scope,
  ]);

  useEffect(() => {
    setNotice(null);
    setBusy(false);
    operation.current += 1;
  }, [scope]);
  const update = <T extends Config>(
    original: T,
    patch: Partial<Omit<NoInfer<T>, "type">>,
  ) => {
    if (!node || !config || readOnly || config.type !== original.type) return;
    const data = { ...original, ...patch };
    onChange({
      ...node,
      data,
      color: nextNodeColor(node.color, original, data),
    });
  };
  const updateAsync = <T extends Config>(
    original: T,
    patch: Partial<Omit<NoInfer<T>, "type">>,
  ) => {
    const current = latest.current;
    if (current.readOnly || !isCurrent() || !current.node) return false;
    const target = current.node;
    const targetConfig = nodeConfig(target);
    if (!targetConfig || targetConfig.type !== original.type) return false;
    const data = { ...original, ...patch };
    current.onChange({
      ...target,
      data,
      color: nextNodeColor(target.color, original, data),
    });
    return true;
  };
  async function perform(
    action: (isActive: () => boolean) => Promise<unknown>,
    success?: string,
  ) {
    const token = ++operation.current;
    setBusy(true);
    setNotice(null);
    const stillSelected = () => operation.current === token && isCurrent();
    try {
      await action(stillSelected);
      if (success && stillSelected())
        setNotice({ text: success, error: false });
    } catch (error) {
      if (stillSelected())
        setNotice({ text: errorMessage(error), error: true });
    } finally {
      if (operation.current === token) setBusy(false);
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
  const previousResult = previousRun?.nodes[selectedId];
  const activeCall = activeRun?.calls?.[selectedId];
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

  return (
    <aside className="inspector" aria-label="Node inspector">
      <header className="ins-header">
        <div>
          <span className="ins-eyebrow">
            {readOnly ? "CHILD GRAPH · INSPECT" : "NODE INSPECTOR"}
          </span>
          <h2>{node ? nodeLabel(node) : "Your workspace"}</h2>
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
              const nextTab = tabs[next];
              if (nextTab) {
                setTab(nextTab);
                document.getElementById(`ins-tab-${nextTab}`)?.focus();
              }
            }}
          >
            {item.charAt(0).toUpperCase() + item.slice(1)}
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
              <fieldset key={scope} className="ins-form" disabled={readOnly}>
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
                      <span
                        style={{
                          background: isWritableNode(config)
                            ? "#ef4444"
                            : COLORS[config.type],
                        }}
                      />
                      {config.type === "database" && !config.read_only
                        ? "Database (write)"
                        : config.type === "acp" && !config.read_only
                          ? "ACP Agent (write)"
                          : NODE_LABELS[config.type]}
                    </div>
                    <NodeConfigForm
                      data={config}
                      graph={graph}
                      selectedId={selectedId}
                      readOnly={readOnly}
                      busy={busy}
                      revision={revision}
                      draft={draft}
                      setDraft={setDraft}
                      library={library}
                      models={models}
                      setModels={setModels}
                      acpOptions={acpOptions}
                      setAcpOptions={setAcpOptions}
                      schema={schema}
                      setSchema={setSchema}
                      child={child}
                      childError={childError}
                      upstream={upstream}
                      results={results}
                      activeRun={activeRun}
                      onOpenWorkflow={onOpenWorkflow}
                      onLibraryChanged={onLibraryChanged}
                      update={update}
                      updateAsync={updateAsync}
                      perform={perform}
                    />
                  </>
                ) : (
                  <section className="ins-empty">
                    <h3>Give this node a purpose</h3>
                    <p>Choose the kind of work it should do.</p>
                    <div className="ins-type-grid">
                      {NODE_TYPES.map((type) => (
                        <button
                          type="button"
                          key={type}
                          onClick={() =>
                            onChange({
                              ...node,
                              data: structuredClone(DEFAULTS[type]),
                              color: node.color || COLORS[type],
                            })
                          }
                        >
                          <span style={{ background: COLORS[type] }} />
                          {NODE_LABELS[type]}
                        </button>
                      ))}
                      <button
                        type="button"
                        onClick={() =>
                          onChange({
                            ...node,
                            data: {
                              ...structuredClone(DEFAULTS.database),
                              read_only: false,
                            },
                            color: node.color || COLORS.file,
                          })
                        }
                      >
                        <span style={{ background: COLORS.file }} />
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
                  {onGraphChange && (
                    <Publication
                      graph={graph}
                      node={node}
                      workflowGraphs={props.workflowGraphs}
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
                            ).map((item) => [item.name, item.name] as const),
                          ]}
                          onChange={(name) => {
                            const agent =
                              config.type === "agent"
                                ? library.agents.find(
                                    (item) => item.name === name,
                                  )
                                : undefined;
                            const database =
                              config.type === "database"
                                ? library.databases.find(
                                    (item) => item.name === name,
                                  )
                                : undefined;
                            const data: NodeData | undefined = agent
                              ? { ...agent.config, type: "agent" }
                              : database
                                ? { ...database.config, type: "database" }
                                : undefined;
                            if (data) {
                              onChange({
                                ...node,
                                kind: name,
                                data,
                              });
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
                              if (config.type === "agent")
                                await request("save_preset", {
                                  type: "agent",
                                  name: draft.preset || "",
                                  config: bareConfig(config),
                                });
                              else
                                await request("save_preset", {
                                  type: "database",
                                  name: draft.preset || "",
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
                        {NODE_TYPES.map((type) => (
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
                                data: structuredClone(DEFAULTS[type]),
                                color: node.color || COLORS[type],
                              })
                            }
                          >
                            <span style={{ background: COLORS[type] }} />
                            {NODE_LABELS[type]}
                          </button>
                        ))}
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
                              },
                              color: node.color || COLORS.file,
                            })
                          }
                        >
                          <span style={{ background: COLORS.file }} />
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
              !sameNodeConfig(currentResult.data, config) && (
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
              <NodeOutputView
                key={`${activeRun?.id}-${selectedId}`}
                result={currentResult}
                variant="inspector"
                loadMode="manual"
                context={outputContextFor(
                  props.outputContext ||
                    (activeRun
                      ? { graph: activeRun.graph, runId: activeRun.id }
                      : undefined),
                  selectedId,
                )}
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
            {currentResult && previousRun && previousResult && node && (
              <PreviousOutput
                current={currentResult}
                previous={previousResult}
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
                {activeCall && (
                  <p className="ins-help">
                    {Object.keys(activeCall.run.nodes).length} steps ·{" "}
                    {activeCall.run.outcome.kind === "ok"
                      ? "Completed"
                      : activeCall.run.outcome.kind}
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
            {history.map((run) => {
              const historyResult = run.nodes[selectedId];
              return (
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
                    {run.outcome.kind === "failed" && (
                      <p className="ins-error">{run.outcome.error}</p>
                    )}
                    {!readOnly && onOpenHistory && (
                      <button
                        type="button"
                        onClick={() => onOpenHistory(run.id)}
                      >
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
                    {node && historyResult ? (
                      <NodeOutputView
                        key={node.id}
                        result={historyResult}
                        title={`${node.name || node.id} output`}
                        variant="inspector"
                        loadMode="manual"
                        context={outputContextFor(
                          readOnly && props.outputContext
                            ? props.outputContext
                            : { graph: run.graph, runId: run.id },
                          node.id,
                        )}
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
              );
            })}
          </div>
        )}
      </div>
    </aside>
  );
}

