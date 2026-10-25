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
});
