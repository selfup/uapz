import type {
  AgentConfig,
  DatabaseConfig,
  Field,
  GraphDoc,
  Issue,
  Library,
  NodeData,
  NodeKind,
  PreparedRun,
  Run,
  RuntimeState,
} from "./types";

export interface FullOutput {
  label: string;
  text: string | null;
  base64: string | null;
  saved: string | null;
  complete: boolean;
  media_type?: string | null;
  limited?: boolean;
  bytes?: number;
}

type Call<A, R> = { args: A; result: R };
type RunArgs = {
  doc: GraphDoc;
  targets?: string[];
  inputs?: Record<string, string>;
};

/** Mirrors AppService::request; callers cannot select an unrelated response type. */
export interface Protocol {
  bootstrap: Call<undefined, Library>;
  load_graph: Call<{ name: string }, GraphDoc>;
  save_graph: Call<
    { doc: GraphDoc },
    { name: string; markdown: string; json: string }
  >;
  save_kind: Call<{ kind: NodeKind }, string>;
  delete_kind: Call<{ name: string }, null>;
  save_preset: Call<
    { name: string } & (
      | { type: "agent"; config: AgentConfig }
      | { type: "database"; config: DatabaseConfig }
    ),
    string
  >;
  delete_preset: Call<{ name: string; type: "agent" | "database" }, null>;
  save_prompt: Call<{ name: string; text: string }, string>;
  load_prompt: Call<{ name: string }, string>;
  delete_prompt: Call<{ name: string }, null>;
  validate_node: Call<{ data: NodeData; name?: string }, Issue[]>;
  validate_graph: Call<
    { doc: GraphDoc },
    { issues: Issue[]; fields: Field[]; sinks: string[] }
  >;
  workflow_fields: Call<{ doc: GraphDoc; targets?: string[] }, Field[]>;
  prepare_run: Call<RunArgs, PreparedRun>;
  start_run: Call<RunArgs & { fresh?: boolean }, { run_id: string }>;
  runtime_state: Call<undefined, RuntimeState | null>;
  cancel_run: Call<{ run_id: string }, null>;
  review: Call<
    { run_id: string; review_id: string; decision: "apply" | "reject" },
    null
  >;
  history: Call<{ graph: string }, Run[]>;
  output: Call<
    {
      graph: string;
      run_id: string;
      node: string;
      call_path?: string;
      output?: string;
      max_bytes?: number;
    },
    FullOutput
  >;
  clear_history: Call<{ graph: string }, number>;
  clear_cache: Call<undefined, number>;
  describe_schema: Call<{ config: DatabaseConfig }, string>;
  write_sql: Call<
    { config: DatabaseConfig; agent: AgentConfig; question: string },
    string
  >;
}
export type Method = keyof Protocol;
