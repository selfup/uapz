import { compareNodeOutputs, sameNodeConfig } from "../inspectorState";
import NodeOutputView, { type NodeOutputContext } from "../NodeOutputView";
import type { NodeRun, Run } from "../types";

export type InspectorNodeResult = NodeRun;
export type InspectorRun = Run;

type OutputContext = { graph: string; runId: string; callPath?: string };

function outputContextFor(
  context: OutputContext | undefined,
  node: string,
): NodeOutputContext | undefined {
  return context ? { ...context, node } : undefined;
}

export default function PreviousOutput({
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
              <NodeOutputView
                key={name}
                title={name}
                outputName={name}
                result={output}
                variant="inspector"
                loadMode="manual"
                context={outputContextFor(
                  { graph: run.graph, runId: run.id },
                  node,
                )}
              />
            ),
          )}
        </details>
      )}
    </section>
  );
}

export function runTime(id: string) {
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
