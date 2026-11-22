import { useEffect, useState } from "react";
import type { GraphDoc, NodeDoc } from "../types";
import {
  isNamedWorkflow,
  publishOutput,
  workflowPorts,
  type WorkflowGraphs,
} from "../graph";
import { errorText } from "../outputMedia";
import { Select, Text } from "./fields";

const errorMessage = errorText;

export default function Publication({
  graph,
  node,
  workflowGraphs = {},
  onGraphChange,
}: {
  graph: GraphDoc;
  node: NodeDoc;
  workflowGraphs?: WorkflowGraphs;
  onGraphChange: (graph: GraphDoc) => void;
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
      onGraphChange(publishOutput(graph, handle, next, workflowGraphs));
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
            options={ports.map((port) => [port.handle, port.label] as const)}
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
