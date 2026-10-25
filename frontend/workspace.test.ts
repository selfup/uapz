import { describe, expect, it } from "vitest";
import {
  callAt,
  childInterfaces,
  emptyGraph,
  firstSelection,
  localMarks,
  normalizeGraph,
} from "./workspace";
import type { Run } from "./types";

describe("child workflow navigation", () => {
  it("keeps unstarted captures and qualifies paths without confusing siblings", () => {
    const child = { ...emptyGraph(), name: "child" };
    const leaf = { ...emptyGraph(), name: "leaf" };
    const unrelated = { ...emptyGraph(), name: "unrelated" };
    const captures = {
      n1: child,
      "n1/n2": leaf,
      "n10/n2": unrelated,
      "n1/n2/n3": unrelated,
    };
    expect(childInterfaces(captures, "n1")).toEqual({ n2: leaf });
    expect(
      localMarks(
        { "n1/n2": "running", "n10/n2": "failed", "n1/n2/n3": "ran" },
        "n1",
      ),
    ).toEqual({ n2: "running" });
  });
  it("supports older history with only nested call records", () => {
    const graph = emptyGraph();
    const leaf: Run = {
      id: "leaf",
      graph: "leaf",
      targets: [],
      elapsed_ms: 0,
      outcome: { kind: "ok" },
      nodes: {},
      log: [],
    };
    const parent: Run = {
      ...leaf,
      id: "parent",
      calls: {
        n1: { graph, run: { ...leaf, calls: { n2: { graph, run: leaf } } } },
      },
    };
    expect(callAt(parent, "n1/n2")?.run.id).toBe("leaf");
    expect(callAt(parent, "n1/missing")).toBeUndefined();
    expect(childInterfaces({}, "", parent)).toEqual({ n1: graph });
  });
  it("prefers captured interfaces while recovering uncaptured children from history", () => {
    const captured = { ...emptyGraph(), name: "captured before execution" };
    const historical = { ...emptyGraph(), name: "historical call" };
    const childRun: Run = {
      id: "child",
      graph: "parent",
      targets: [],
      elapsed_ms: 0,
      outcome: { kind: "ok" },
      nodes: {},
      log: [],
    };
    const run: Run = {
      ...childRun,
      id: "parent",
      calls: {
        n2: { graph: historical, run: childRun },
        n3: { graph: historical, run: childRun },
      },
    };
    expect(
      childInterfaces(
        { "n1/n2": captured, "n1/n4": captured, "n10/n2": historical },
        "n1",
        run,
      ),
    ).toEqual({ n2: captured, n3: historical, n4: captured });
  });
  it("restores old view scale and selects owners of forwarded published ports", () => {
    const graph = {
      ...emptyGraph(),
      view: { pan_x: 25, pan_y: 90 },
      nodes: [{ id: "n5", x: 0, y: 0 }],
      outputs: { summary: "n5.summary" },
    };
    expect(normalizeGraph(graph).view).toEqual({
      pan_x: 25,
      pan_y: 90,
      zoom: 1,
    });
    expect(firstSelection(graph)).toBe("n5");
    expect(graph.view).not.toHaveProperty("zoom");
  });
  it("skips deleted history targets and output owners when choosing a visible node", () => {
    const graph = {
      ...emptyGraph(),
      nodes: [
        { id: "first", x: 0, y: 0 },
        { id: "published", x: 0, y: 0 },
        { id: "target", x: 0, y: 0 },
      ],
      outputs: { stale: "deleted.text", summary: "published.summary" },
    };
    const run: Run = {
      id: "old-run",
      graph: "old-graph",
      targets: ["deleted", "target"],
      elapsed_ms: 0,
      outcome: { kind: "ok" },
      nodes: {},
      log: [],
    };
    expect(firstSelection(graph, run)).toBe("target");
    expect(firstSelection(graph, { ...run, targets: ["deleted"] })).toBe(
      "published",
    );
    expect(
      firstSelection({ ...graph, outputs: { stale: "deleted.text" } }),
    ).toBe("first");
    expect(firstSelection(emptyGraph(), run)).toBeNull();
  });
});
