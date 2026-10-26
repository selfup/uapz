import { describe, expect, it } from "vitest";
import { decodeResponse } from "./responses";
import { defaultNodeData } from "./graph";
import { emptyGraph } from "./workspace";
import type { NodeType, Run, RuntimeState } from "./types";

const run = (): Run => ({
  id: "saved",
  graph: "demo",
  targets: [],
  elapsed_ms: 10,
  nodes: {},
  log: [],
  outcome: { kind: "ok" },
});
const runtime = (): RuntimeState => ({
  run_id: "live",
  running: true,
  cancelling: false,
  graph: emptyGraph(),
  targets: [],
  marks: {},
  log: [],
  stream: null,
  workflows: {},
  review: null,
  run: null,
  error: null,
});

describe("desktop response decoding", () => {
  it("accepts every node type and keeps blank/legacy optional fields usable", () => {
    const types: NodeType[] = [
      "agent",
      "database",
      "script",
      "data",
      "directory",
      "file",
      "input",
      "workflow",
    ];
    const graph = {
      ...emptyGraph(),
      view: { pan_x: 0, pan_y: 0 },
      nodes: [
        ...types.map((type, index) => ({
          id: `n${index}`,
          x: 0,
          y: 0,
          data: defaultNodeData(type),
        })),
        { id: "blank", x: 0, y: 0, data: {} },
        { id: "null", x: 0, y: 0, data: null },
      ],
    };
    expect(decodeResponse("load_graph", graph)).toEqual(graph);
  });

  it.each([
    null,
    [],
    "workflow",
    { ...emptyGraph(), nodes: {} },
    { ...emptyGraph(), nodes: [{ id: "n0", x: Infinity, y: 0 }] },
    { ...emptyGraph(), nodes: [{ id: "n0", x: 1e300, y: 0 }] },
    { ...emptyGraph(), view: { pan_x: 0, pan_y: 0, zoom: 0 } },
    {
      ...emptyGraph(),
      nodes: [
        { id: "n0", x: 0, y: 0 },
        { id: "n0", x: 0, y: 0 },
      ],
    },
    {
      ...emptyGraph(),
      nodes: [
        { id: "n0", x: 0, y: 0, data: { type: "agent", prompt: "SECRET" } },
      ],
    },
    {
      ...emptyGraph(),
      nodes: [{ id: "n0", x: 0, y: 0, data: { type: "constructor" } }],
    },
  ])("rejects malformed graphs before rendering: %j", (value) => {
    expect(() => decodeResponse("load_graph", value)).toThrow(
      /^Invalid load_graph response:/,
    );
  });

  it("reports the malformed field without echoing workspace content", () => {
    const value = {
      ...emptyGraph(),
      nodes: [
        {
          id: "n0",
          x: 0,
          y: 0,
          data: {
            ...defaultNodeData("script"),
            args: ["SECRET", { text: "SECRET" }],
          },
        },
      ],
    };
    try {
      decodeResponse("load_graph", value);
      throw new Error("unexpected success");
    } catch (error) {
      expect(String(error)).toContain("response.nodes[0].data.args[1]");
      expect(String(error)).not.toContain("SECRET");
    }
  });

  it("keeps prototype-named publication and result keys as own properties", () => {
    const outputs = JSON.parse('{"__proto__":"n0","constructor":"n0"}');
    const graph = decodeResponse("load_graph", { ...emptyGraph(), outputs });
    expect(Object.getPrototypeOf(graph.outputs)).toBeNull();
    expect(Object.hasOwn(graph.outputs!, "__proto__")).toBe(true);
    expect(graph.outputs!["__proto__"]).toBe("n0");
    expect(
      decodeResponse("prepare_run", { targets: [], fields: [], inputs: {} })
        .inputs.constructor,
    ).toBeUndefined();
    const result = { state: "ran", preview: "safe", complete: true };
    const saved = {
      ...run(),
      nodes: {
        n0: {
          ...result,
          named_outputs: Object.fromEntries([["__proto__", result]]),
        },
      },
    };
    expect(
      decodeResponse("history", [saved])[0]?.nodes.n0?.named_outputs?.[
        "__proto__"
      ],
    ).toEqual(result);
  });

  it("validates recursive history and fails cleanly on excessive nesting", () => {
    const saved = run();
    saved.calls = { n0: { graph: emptyGraph(), run: run() } };
    expect(decodeResponse("history", [saved])).toEqual([saved]);
    let deep = run();
    for (let index = 0; index < 100; index++)
      deep = { ...run(), calls: { n0: { graph: emptyGraph(), run: deep } } };
    expect(() => decodeResponse("history", [deep])).toThrow(
      /nesting or item limit/,
    );
  });

  it("rejects invalid stream/event/review state while accepting no active session", () => {
    expect(decodeResponse("runtime_state", null)).toBeNull();
    const live = runtime();
    expect(decodeResponse("runtime_state", live)).toEqual(live);
    for (const patch of [
      { marks: { n0: "unknown" } },
      { stream: { id: "n0", text: 3 } },
      {
        events: [
          { seq: 1, kind: "log", text: "ok" },
          { seq: 1, kind: "log", text: "duplicate" },
        ],
      },
      {
        events: [
          { seq: 1, kind: "output", id: "n0", result: { state: "unknown" } },
        ],
      },
      {
        review: {
          id: "review",
          node: "n0",
          files: [
            { path: "x", before_bytes: null, after_bytes: 1, diff: false },
          ],
        },
      },
      { run: { ...run(), outcome: { kind: "failed" } } },
    ])
      expect(() =>
        decodeResponse("runtime_state", { ...live, ...patch }),
      ).toThrow(/Invalid runtime_state response/);
  });

  it("requires output payloads to match their transport and completeness flags", () => {
    const output = {
      label: "Text",
      text: "safe",
      base64: null,
      saved: null,
      complete: true,
    };
    expect(decodeResponse("output", output)).toEqual(output);
    expect(
      decodeResponse("output", {
        ...output,
        complete: false,
        limited: true,
        bytes: 999,
      }),
    ).toMatchObject({ limited: true });
    for (const patch of [
      { text: {} },
      { base64: "YQ==" },
      { text: null },
      { text: null, base64: "not base64" },
      { complete: "true" },
      { limited: true },
      { bytes: -1 },
    ])
      expect(() => decodeResponse("output", { ...output, ...patch })).toThrow(
        /Invalid output response/,
      );
  });

  it("rejects non-JSON blank data and sparse node arrays", () => {
    for (const data of [
      new Date(),
      new Map(),
      Object.create({ type: "agent" }),
    ])
      expect(() =>
        decodeResponse("load_graph", {
          ...emptyGraph(),
          nodes: [{ id: "n0", x: 0, y: 0, data }],
        }),
      ).toThrow(/Invalid load_graph response/);
    expect(() =>
      decodeResponse("load_graph", { ...emptyGraph(), nodes: Array(2) }),
    ).toThrow(/Invalid load_graph response/);
  });
});
