import { nodeLabel } from "./graph";
import { callAt } from "./workspace";
import type { GraphDoc, Mark, NodeRun, Run, RuntimeState } from "./types";

export type RunEvent =
  | { seq: number; kind: "log"; text: string; truncated?: boolean }
  | { seq: number; kind: "node"; id: string; state: Mark }
  | { seq: number; kind: "text"; id: string; text: string; truncated?: boolean }
  | { seq: number; kind: "output"; id: string; result: NodeRun };

export type FeedEntry = RunEvent & { key: string };

export function runOutputs(
  run: Run | null | undefined,
  prefix = "",
): Map<string, NodeRun> {
  const results = new Map<string, NodeRun>();
  for (const [id, result] of Object.entries(run?.nodes || {}))
    results.set(`${prefix}${id}`, result);
  for (const [id, call] of Object.entries(run?.calls || {}))
    for (const [path, result] of runOutputs(call.run, `${prefix}${id}/`))
      results.set(path, result);
  return results;
}

/** Keep chronological chunk groups, replacing partial text with terminal results. */
export function runFeed(
  runtime: RuntimeState | null,
  run: Run | null,
): FeedEntry[] {
  const events = runtime?.events || [];
  const outputs = runOutputs(runtime?.run || run);
  const latestOutput = new Map<string, number>();
  for (const event of events)
    if (event.kind === "output") latestOutput.set(event.id, event.seq);
  const emitted = new Set(latestOutput.keys());
  const replacesStream = (result: NodeRun) =>
    result.state !== "failed" || result.preview !== undefined;
  const completed = new Set(
    [...outputs]
      .filter(([, result]) => replacesStream(result))
      .map(([id]) => id),
  );
  for (const event of events)
    if (event.kind === "output" && replacesStream(event.result))
      completed.add(event.id);
  const entries: FeedEntry[] = events
    .filter((event) =>
      event.kind === "text"
        ? !completed.has(event.id)
        : event.kind === "output"
          ? latestOutput.get(event.id) === event.seq
          : true,
    )
    .map((event) => ({ ...event, key: `event-${event.seq}` }));
  if (!events.some((event) => event.kind === "log"))
    entries.unshift(
      ...(runtime?.log || run?.log || []).map((text, index) => ({
        kind: "log" as const,
        seq: -1,
        text,
        key: `log-${index}`,
      })),
    );
  // Older history has no events; final results also recover outputs evicted from a bounded feed.
  for (const [id, result] of outputs)
    if (!emitted.has(id))
      entries.push({
        kind: "output",
        seq: -1,
        id,
        result,
        key: `output-${id}`,
      });
  if (
    runtime?.stream &&
    !completed.has(runtime.stream.id) &&
    !events.some(
      (event) => event.kind === "text" && event.id === runtime.stream?.id,
    )
  )
    entries.push({ ...runtime.stream, kind: "text", seq: -1, key: "stream" });
  if (!events.length)
    for (const [id, state] of Object.entries(runtime?.marks || {}))
      if (!outputs.has(id) && !emitted.has(id) && id !== runtime?.stream?.id)
        entries.push({ kind: "node", seq: -1, id, state, key: `mark-${id}` });
  return entries;
}

export function feedNodeName(
  root: GraphDoc,
  id: string,
  workflows: Record<string, GraphDoc>,
  run: Run | null,
): string {
  const parts = id.split("/");
  let graph: GraphDoc | undefined = root;
  return parts
    .map((part, index) => {
      const node = graph?.nodes.find((candidate) => candidate.id === part);
      const label = node ? nodeLabel(node) : part;
      const path = parts.slice(0, index + 1).join("/");
      graph = workflows[path] || callAt(run, path)?.graph;
      return label;
    })
    .join(" / ");
}
