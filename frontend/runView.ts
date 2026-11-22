import type { GraphDoc, Mark, NodeRun, Run, RuntimeState } from "./types";
import { callAt, localMarks, runMarks } from "./workspace";

export type RunViewChild = {
  path: string;
};

export type RunViewInput = {
  runtime: RuntimeState | null;
  attachedRun: string | null;
  shownRun: Run | null;
  historical: boolean;
  running: boolean;
  activeChild?: RunViewChild;
  activeSelected: string | null;
  readOnly: boolean;
  remembered: {
    nodes: Record<string, NodeRun>;
    origins: Record<string, Run>;
  };
};

export type RunView = {
  runMatches: boolean;
  currentRun: Run | null;
  snapshotContext: boolean;
  captured: Record<string, GraphDoc>;
  childPath: string;
  childRun: Run | null;
  outputRun: Run | null;
  marks: Record<string, Mark>;
  liveLog: string[];
  liveStream: { id: string; text: string } | null;
  outputContext:
    | { graph: string; runId: string; callPath?: string }
    | undefined;
};

/** Derive the run panel / inspector view from session + navigation state. */
export function deriveRunView({
  runtime,
  attachedRun,
  shownRun,
  historical,
  running,
  activeChild,
  activeSelected,
  readOnly,
  remembered,
}: RunViewInput): RunView {
  const runMatches = !!runtime && runtime.run_id === attachedRun;
  const currentRun = shownRun || (runMatches ? runtime?.run : null) || null;
  const snapshotContext = (runMatches && running) || !!currentRun;
  const captured =
    runMatches && running ? runtime.workflows : currentRun?.workflows || {};
  const childPath = activeChild?.path || "";
  const childRun = activeChild
    ? callAt(currentRun, activeChild.path)?.run || null
    : currentRun;
  const outputRun =
    !readOnly && !historical && activeSelected
      ? remembered.origins[activeSelected] || currentRun
      : currentRun;
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
  const outputContext = outputRun
    ? {
        graph: outputRun.graph,
        runId: outputRun.id,
        ...(childPath ? { callPath: childPath } : {}),
      }
    : undefined;
  return {
    runMatches,
    currentRun,
    snapshotContext,
    captured,
    childPath,
    childRun,
    outputRun,
    marks,
    liveLog,
    liveStream,
    outputContext,
  };
}
