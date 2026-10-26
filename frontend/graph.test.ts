import { describe, expect, it } from "vitest";
import type { EdgeDoc, GraphDoc, NodeDoc } from "./types";
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
  nodeSummary,
  publishOutput,
  sinkIds,
  typedData,
  visibleEdgeHandle,
  workflowPorts,
} from "./graph";

type FixtureDoc = GraphDoc & { nodes: [NodeDoc, NodeDoc]; edges: [EdgeDoc] };
const doc = (): FixtureDoc => ({
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
  it("ignores inherited workflow entries while retaining own prototype-named ports", () => {
    const graph = doc();
    const inherited = Object.create({ n2: child({ leaked: "n1" }) });
    expect(workflowPorts(graph, graph.nodes[0], inherited)).toEqual([]);
    const outputs = Object.fromEntries([
      ["__proto__", "n1"],
      ["constructor", "n1"],
    ]);
    expect(
      workflowPorts(graph, graph.nodes[0], { n2: child(outputs) }),
    ).toEqual([
      { id: "__proto__", missing: false },
      { id: "constructor", missing: false },
    ]);
    expect(
      connectNodes(graph, "n2", "n7", "constructor", { n2: child(outputs) })
        .edges[1]?.output,
    ).toBe("constructor");
  });

  it("requires own workflow map entries even for prototype-like legacy node IDs", () => {
    const graph = doc();
    graph.nodes[0].id = "__proto__";
    expect(workflowPorts(graph, graph.nodes[0], {})).toEqual([]);
    const workflows = Object.fromEntries([
      ["__proto__", child({ summary: "n1" })],
    ]);
    expect(workflowPorts(graph, graph.nodes[0], workflows)).toEqual([
      { id: "summary", missing: false },
    ]);
  });

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
  it("does not mistake inherited publications for unchanged stale exports", () => {
    const graph = doc();
    graph.outputs = Object.create({
      constructor: "missing",
      hidden: "missing",
    });
    for (const name of ["constructor", "hidden"])
      expect(() => assignPublications(graph, { [name]: "missing" })).toThrow();
    graph.outputs = Object.fromEntries([["constructor", "missing"]]);
    expect(
      assignPublications(graph, { constructor: "missing" }).outputs,
    ).toEqual({ constructor: "missing" });
  });

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

describe("safe node IDs", () => {
  const withIds = (ids: string[]): GraphDoc => ({
    ...doc(),
    nodes: ids.map((id) => ({ id, x: 0, y: 0 })),
  });

  it("ignores unsafe and overflowing numeric suffixes while preserving ordinary successors", () => {
    const graph = withIds([
      "n2",
      "n0007",
      "n9007199254740992",
      `n${"9".repeat(400)}`,
      "custom",
    ]);
    expect(nextNodeId(graph)).toBe("n8");
    expect(graph.nodes.some((node) => node.id === nextNodeId(graph))).toBe(
      false,
    );
  });

  it("fills a bounded gap when the maximum safe ID already exists", () => {
    const graph = withIds([
      `n${Number.MAX_SAFE_INTEGER}`,
      "n9007199254740992",
      "n1",
      "n2",
      "n4",
    ]);
    expect(nextNodeId(graph)).toBe("n3");
    expect(Number.isSafeInteger(Number(nextNodeId(graph).slice(1)))).toBe(true);
    expect(nextNodeId(withIds([`n${Number.MAX_SAFE_INTEGER - 1}`]))).toBe(
      `n${Number.MAX_SAFE_INTEGER}`,
    );
  });

  it("allocates on a wide graph without spreading nodes into function arguments", () => {
    expect(
      nextNodeId(
        withIds(Array.from({ length: 150_000 }, (_, index) => `n${index + 1}`)),
      ),
    ).toBe("n150001");
  });
});

describe("runtime node data", () => {
  const withData = (data: unknown): NodeDoc =>
    ({ id: "n1", x: 0, y: 0, data }) as NodeDoc;

  it("treats unknown, inherited and non-object discriminators as blank", () => {
    for (const data of [
      undefined,
      null,
      "agent",
      0,
      true,
      [],
      { type: "unknown" },
      { type: "__proto__" },
      { type: "constructor" },
      Object.create({ type: "agent" }),
    ]) {
      const node = withData(data);
      expect(typedData(node)).toBeUndefined();
      expect(nodeLabel(node)).toBe("Blank node");
      expect(nodeColor(node)).toBe("#9ca3af");
      expect(typeof nodeSummary(node)).toBe("string");
    }
  });

  it("renders partial known configurations without crashing before validation", () => {
    for (const type of [
      "agent",
      "database",
      "directory",
      "file",
      "input",
      "data",
      "script",
      "workflow",
    ]) {
      expect(nodeSummary(withData({ type }))).toBeTruthy();
    }
    expect(
      nodeSummary(
        withData({ type: "agent", provider: "constructor", model: "m" }),
      ),
    ).toBe("Constructor · m");
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
