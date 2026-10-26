import { describe, expect, it } from "vitest";
import { decodeResponse } from "./responses";
import { defaultNodeData } from "./graph";
import type { GraphDoc, NodeRun, Run } from "./types";

describe("native document and history decoder budgets", () => {
  it("accepts a large valid graph below the native 16 MB document limit", () => {
    const graph: GraphDoc = {
      version: 1,
      name: "large-graph",
      view: { pan_x: 0, pan_y: 0, zoom: 1 },
      nodes: Array.from({ length: 180_000 }, (_, index) => ({
        id: `n${index}`,
        x: 0,
        y: 0,
        data: {},
      })),
      edges: [],
    };
    const bytes = JSON.stringify(graph).length;
    expect(bytes).toBeGreaterThan(6_000_000);
    expect(bytes).toBeLessThan(16 * 1024 * 1024);

    const decoded = decodeResponse("load_graph", graph);
    expect(decoded.nodes).toHaveLength(180_000);
    expect(decoded.nodes[0]).toEqual(graph.nodes[0]);
    expect(decoded.nodes.at(-1)?.id).toBe("n179999");
  });

  it("accepts 200 independently bounded history records without exhausting a shared item budget", () => {
    const result: NodeRun = {
      state: "ran",
      data: defaultNodeData("agent"),
      complete: true,
    };
    const nodes = Object.fromEntries(
      Array.from({ length: 400 }, (_, index) => [`n${index}`, result]),
    );
    const history: Run[] = Array.from({ length: 200 }, (_, index) => ({
      id: new Date(Date.UTC(2026, 9, 25, 12, 0, index))
        .toISOString()
        .replace(/[-:.]/g, ""),
      graph: "large-history",
      targets: ["n399"],
      elapsed_ms: 1,
      outcome: { kind: "ok" },
      nodes,
      log: [],
    }));
    expect(JSON.stringify(history[0]).length).toBeLessThan(16 * 1024 * 1024);

    const decoded = decodeResponse("history", history);
    expect(decoded).toHaveLength(200);
    expect(decoded.every((run) => Object.keys(run.nodes).length === 400)).toBe(
      true,
    );
    expect(decoded.at(-1)?.nodes.n399).toEqual(result);
  });
});
