import type { GraphDoc, Mark, Run } from "./types";

export const emptyGraph = (): GraphDoc => ({
  version: 1,
  name: "untitled",
  view: { pan_x: 60, pan_y: 60, zoom: 1 },
  nodes: [],
  edges: [],
  outputs: {},
});
export function normalizeGraph(graph: GraphDoc): GraphDoc {
  return {
    ...graph,
    outputs: graph.outputs || {},
    view: {
      pan_x: graph.view?.pan_x ?? 0,
      pan_y: graph.view?.pan_y ?? 0,
      zoom: graph.view?.zoom ?? 1,
    },
  };
}
export function callAt(
  run: Run | null | undefined,
  path: string,
): { graph: GraphDoc; run: Run } | undefined {
  let call: { graph: GraphDoc; run: Run } | undefined;
  for (const id of path.split("/")) {
    if (!id || !run?.calls || !Object.hasOwn(run.calls, id)) return;
    call = run.calls[id];
    if (!call) return;
    run = call.run;
  }
  return call;
}
/** Local interfaces from immutable captures, including children that never ran. */
export function childInterfaces(
  workflows: Record<string, GraphDoc>,
  path = "",
  run?: Run | null,
): Record<string, GraphDoc> {
  const prefix = path ? `${path}/` : "";
  const result = new Map<string, GraphDoc>();
  for (const [key, graph] of Object.entries(workflows)) {
    if (!key.startsWith(prefix)) continue;
    const id = key.slice(prefix.length);
    if (id && !id.includes("/")) result.set(id, graph);
  }
  for (const [key, call] of Object.entries(run?.calls || {}))
    if (!result.has(key)) result.set(key, call.graph);
  return Object.fromEntries(result);
}
export function localMarks(
  marks: Record<string, Mark>,
  path = "",
): Record<string, Mark> {
  const prefix = path ? `${path}/` : "";
  return Object.fromEntries(
    Object.entries(marks)
      .filter(
        ([key]) =>
          key.startsWith(prefix) && !key.slice(prefix.length).includes("/"),
      )
      .map(([key, value]) => [key.slice(prefix.length), value]),
  );
}
export function runMarks(run?: Run | null): Record<string, Mark> {
  return Object.fromEntries(
    Object.entries(run?.nodes || {}).map(([id, node]) => [id, node.state]),
  );
}
export function firstSelection(
  graph: GraphDoc,
  run?: Run | null,
): string | null {
  return (
    [
      ...(run?.targets || []),
      ...Object.values(graph.outputs || {}).map((value) => value.split(".")[0]),
      ...graph.nodes.map((node) => node.id),
    ].find((id) => graph.nodes.some((node) => node.id === id)) || null
  );
}
export function safeGraphName(name: string): string {
  return (
    name
      .trim()
      .replace(/[^a-zA-Z0-9_.-]+/g, "-")
      .replace(/^\.+/, "") || "untitled"
  );
}
