import { describe, expect, it } from "vitest";
import type { GraphDoc } from "./types";
import {
  assignPublications,
  connectNodes,
  defaultNodeData,
  deleteNodes,
  flowToGraphPoint,
  graphToFlowPoint,
  nextNodeId,
  nodeColor,
  nodeLabel,
  publishOutput,
  sinkIds,
  visibleEdgeHandle,
  workflowPorts,
} from "./graph";

const doc = (): GraphDoc => ({
  version: 1,
  name: "parent",
  view: { pan_x: 20, pan_y: -10, zoom: 2 },
  nodes: [
    {
      id: "n2",
      x: 0,
      y: 0,
      data: { type: "workflow", graph: "child", output: "", inputs: {} },
    },
    { id: "n7", x: 300, y: 0, data: defaultNodeData("agent") },
  ],
  edges: [{ from: "n2", to: "n7" }],
  outputs: { result: "n7" },
});
const child = (outputs: Record<string, string>): GraphDoc => ({
  version: 1,
  name: "child",
  view: { pan_x: 0, pan_y: 0 },
  nodes: [],
  edges: [],
  outputs,
});

describe("named workflow ports", () => {
  it("anchors a legacy default wire to the sole available port without rewriting it", () => {
    const graph = doc();
    const before = JSON.stringify(graph);
    const ports = workflowPorts(graph, graph.nodes[0], {
      n2: child({ summary: "n1" }),
    });
    expect(visibleEdgeHandle(graph.edges[0], ports)).toBe("summary");
    expect(visibleEdgeHandle({ ...graph.edges[0], output: null }, ports)).toBe(
      "summary",
    );
    expect(JSON.stringify(graph)).toBe(before);
    expect(graph.edges[0]).not.toHaveProperty("output");
  });

  it("retains vanished endpoints but refuses new connections from them", () => {
    const graph = doc();
    graph.edges.push({ from: "n2", to: "n7", output: "old" });
    graph.outputs = { forwarded: "n2.older" };
    const workflows = { n2: child({ summary: "n1", " bad ": "n2" }) };
    const ports = workflowPorts(graph, graph.nodes[0], workflows);
    expect(ports).toEqual([
      { id: "old", missing: true },
      { id: "older", missing: true },
      { id: "summary", missing: false },
    ]);
    expect(connectNodes(graph, "n2", "n7", "old", workflows)).toBe(graph);
    expect(connectNodes(graph, "n2", "n7", undefined, workflows)).toBe(graph);
    expect(
      connectNodes(graph, "n2", "n7", "summary", workflows).edges,
    ).toContainEqual({ from: "n2", to: "n7", output: "summary" });
    expect(graph.edges).toHaveLength(2);
  });

  it("does not guess a legacy output when several outputs are available", () => {
    expect(
      visibleEdgeHandle(doc().edges[0], [
        { id: "a", missing: false },
        { id: "b", missing: false },
      ]),
    ).toBeNull();
  });

  it("keeps a published port literally named default qualified in the document", () => {
    const graph = doc();
    const changed = connectNodes(graph, "n2", "n7", "default", {
      n2: child({ default: "n1" }),
    });
    expect(changed.edges[1]).toEqual({
      from: "n2",
      to: "n7",
      output: "default",
    });
    expect(changed.edges[0]).not.toHaveProperty("output");
  });
});

describe("publications", () => {
  it("forwards named handles and replaces aliases for only that exact port", () => {
    const graph = doc();
    graph.outputs = {
      report: "n2.summary",
      rows: "n2.details",
      old: "n2.gone",
    };
    const changed = publishOutput(graph, "n2.summary", ["brief", "overview"], {
      n2: child({ summary: "n1", details: "n2" }),
    });
    expect(changed.outputs).toEqual({
      brief: "n2.summary",
      overview: "n2.summary",
      rows: "n2.details",
      old: "n2.gone",
    });
    expect(graph.outputs.report).toBe("n2.summary");
  });

  it("rejects invalid names, collisions, missing ports and excess outputs atomically", () => {
    const graph = doc();
    const before = JSON.stringify(graph);
    const workflows = { n2: child({ summary: "n1" }) };
    for (const names of [[" bad "], ["n12"], ["result"], ["twice", "twice"]]) {
      expect(() =>
        publishOutput(graph, "n2.summary", names, workflows),
      ).toThrow();
      expect(JSON.stringify(graph)).toBe(before);
    }
    expect(() =>
      assignPublications(graph, { bad: "n2.missing" }, workflows),
    ).toThrow();
    expect(() =>
      assignPublications(
        graph,
        Object.fromEntries(
          Array.from({ length: 65 }, (_, i) => [`out${i}`, "n7"]),
        ),
      ),
    ).toThrow();
    expect(JSON.stringify(graph)).toBe(before);
  });

  it("allows removing an obsolete publication while keeping unrelated stale exports", () => {
    const graph = doc();
    graph.outputs = { gone: "n2.missing", other: "n2.older" };
    expect(publishOutput(graph, "n2.missing", []).outputs).toEqual({
      other: "n2.older",
    });
  });

  it("preserves valid names that also occur on Object.prototype", () => {
    const changed = publishOutput(doc(), "n7", ["__proto__", "constructor"]);
    expect(Object.keys(changed.outputs!)).toContain("__proto__");
    expect(changed.outputs?.__proto__).toBe("n7");
    expect(changed.outputs?.constructor).toBe("n7");
  });
});

describe("graph mutations", () => {
  it("removes incident wires and all node/port aliases when deleting a node", () => {
    const graph = doc();
    graph.outputs = { report: "n2.summary", default: "n2", kept: "n7" };
    const changed = deleteNodes(graph, ["n2"]);
    expect(changed.nodes.map((node) => node.id)).toEqual(["n7"]);
    expect(changed.edges).toEqual([]);
    expect(changed.outputs).toEqual({ kept: "n7" });
    expect(graph.nodes).toHaveLength(2);
  });

  it("blocks every document mutation on a read-only child", () => {
    const graph = doc();
    const before = JSON.stringify(graph);
    expect(deleteNodes(graph, ["n2"], true)).toBe(graph);
    expect(connectNodes(graph, "n7", "n2", undefined, {}, true)).toBe(graph);
    expect(() => assignPublications(graph, {}, {}, true)).toThrow("read-only");
    expect(() => publishOutput(graph, "n7", ["new"], {}, true)).toThrow(
      "read-only",
    );
    expect(JSON.stringify(graph)).toBe(before);
  });

  it("deduplicates default edges and computes stable ids and sinks", () => {
    const graph = doc();
    graph.edges = [{ from: "n7", to: "n2", output: null }];
    expect(connectNodes(graph, "n7", "n2")).toBe(graph);
    expect(connectNodes(graph, "n7", "n7")).toBe(graph);
    expect(nextNodeId(graph)).toBe("n8");
    expect(sinkIds(graph)).toEqual(["n2"]);
  });

  it("uses saved labels before type names without changing template names", () => {
    const node = {
      id: "n1",
      x: 0,
      y: 0,
      kind: "Name Every City",
      data: defaultNodeData("agent"),
    };
    expect(nodeLabel(node)).toBe("Name Every City");
    expect(nodeLabel({ ...node, name: "cities" })).toBe("cities");
    expect(nodeLabel({ ...node, name: "  " })).toBe("Name Every City");
    expect(nodeLabel({ ...node, kind: "  " })).toBe("Agent");
    expect(node).not.toHaveProperty("name");
  });

  it("keeps saved typed-node colors and falls back to type colors", () => {
    expect(
      nodeColor({
        id: "n1",
        x: 0,
        y: 0,
        color: "#ab67cd",
        data: defaultNodeData("agent"),
      }),
    ).toBe("#ab67cd");
    expect(
      nodeColor({
        id: "n1",
        x: 0,
        y: 0,
        color: "invalid",
        data: defaultNodeData("data"),
      }),
    ).toBe("#22c55e");
    expect(
      nodeColor({ id: "n1", x: 0, y: 0, data: defaultNodeData("agent") }),
    ).toBe("#8b5cf6");
    expect(
      nodeColor({ id: "n1", x: 0, y: 0, data: defaultNodeData("file") }),
    ).toBe("#ef4444");
    const db = defaultNodeData("database");
    if (db.type !== "database")
      throw new Error("Expected a database configuration");
    expect(
      nodeColor({ id: "n1", x: 0, y: 0, data: { ...db, read_only: false } }),
    ).toBe("#ef4444");
  });

  it("round trips canvas coordinates through saved pan and zoom, including old views", () => {
    const graph = doc();
    const point = { x: 127, y: -34 };
    expect(
      flowToGraphPoint(graphToFlowPoint(point, graph.view), graph.view),
    ).toEqual(point);
    expect(flowToGraphPoint({ x: 10, y: 20 }, { pan_x: 5, pan_y: 3 })).toEqual({
      x: 5,
      y: 17,
    });
  });
});
