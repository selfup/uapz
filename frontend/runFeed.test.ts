import { describe, expect, it } from "vitest";
import { feedNodeName, runFeed, type RunEvent } from "./runFeed";
import { emptyGraph } from "./workspace";
import type { Run, RuntimeState } from "./types";

const history = (): Run => ({
  id: "history",
  graph: "root",
  targets: [],
  elapsed_ms: 10,
  outcome: { kind: "ok" },
  nodes: {},
  log: [],
});
const live = (events: RunEvent[]): RuntimeState => ({
  run_id: "live",
  graph: emptyGraph(),
  running: true,
  cancelling: false,
  targets: [],
  marks: {},
  log: [],
  stream: null,
  workflows: {},
  review: null,
  run: null,
  error: null,
  events,
});

describe("workflow run feed", () => {
  it("retains interleaved chunk groups from all nodes in event order", () => {
    const events: RunEvent[] = [
      { seq: 1, kind: "text", id: "n1", text: "first" },
      { seq: 2, kind: "text", id: "n2/n1", text: "child" },
      { seq: 3, kind: "text", id: "n1", text: "second" },
    ];
    expect(
      runFeed(live(events), null).map(({ key: _key, ...event }) => event),
    ).toEqual(events);
  });

  it("replaces same-node streams with completed output while retaining other streams", () => {
    const result = {
      state: "ran" as const,
      preview: "complete",
      complete: true,
    };
    const events: RunEvent[] = [
      { seq: 1, kind: "text", id: "n1", text: "partial" },
      { seq: 2, kind: "text", id: "n2", text: "still streaming" },
      { seq: 3, kind: "output", id: "n1", result },
    ];
    const runtime = {
      ...live(events),
      stream: { id: "n1", text: "partial" },
      run: { ...history(), nodes: { n1: result } },
    };
    expect(runFeed(runtime, runtime.run).map((entry) => entry.key)).toEqual([
      "event-2",
      "event-3",
    ]);
  });

  it("retains streamed text when the final run records an error without output", () => {
    const runtime = {
      ...live([
        { seq: 1, kind: "text", id: "n1", text: "Useful partial answer" },
        { seq: 2, kind: "node", id: "n1", state: "failed" },
      ]),
      running: false,
      run: {
        ...history(),
        outcome: { kind: "failed" as const, error: "Connection lost" },
        nodes: { n1: { state: "failed" as const, error: "Connection lost" } },
      },
    };
    const entries = runFeed(runtime, runtime.run);
    expect(entries.find((entry) => entry.kind === "text")).toMatchObject({
      id: "n1",
      text: "Useful partial answer",
    });
    expect(entries.find((entry) => entry.kind === "output")).toMatchObject({
      id: "n1",
      result: { error: "Connection lost" },
    });
    expect(
      runFeed(
        {
          ...runtime,
          events: [],
          stream: { id: "n1", text: "Legacy partial answer" },
        },
        runtime.run,
      ).find((entry) => entry.kind === "text"),
    ).toMatchObject({ text: "Legacy partial answer" });
  });

  it("recovers nested historical outputs omitted from the live feed without duplicating retained outputs", () => {
    const result = { state: "ran" as const, preview: "saved", complete: true };
    const run: Run = {
      ...history(),
      log: ["started", "finished"],
      nodes: { n1: result },
      calls: {
        n1: {
          graph: emptyGraph(),
          run: { ...history(), nodes: { n2: result } },
        },
      },
    };
    const runtime = {
      ...live([{ seq: 7, kind: "output", id: "n1", result }]),
      run,
      log: run.log,
    };
    const entries = runFeed(runtime, run);
    expect(
      entries
        .filter((entry) => entry.kind === "log")
        .map((entry) => entry.text),
    ).toEqual(["started", "finished"]);
    expect(
      entries
        .filter((entry) => entry.kind === "output")
        .map((entry) => entry.id),
    ).toEqual(["n1", "n1/n2"]);
    expect(
      runFeed(null, run)
        .filter((entry) => entry.kind === "output")
        .map((entry) => entry.id),
    ).toEqual(["n1", "n1/n2"]);
  });

  it("resolves child labels from immutable captures before older call graphs", () => {
    const root = {
      ...emptyGraph(),
      nodes: [{ id: "n1", name: "Research", x: 0, y: 0 }],
    };
    const capturedNode = { id: "n2", name: "Summarize", x: 0, y: 0 };
    const captured = {
      ...emptyGraph(),
      nodes: [capturedNode],
    };
    const run = {
      ...history(),
      calls: {
        n1: {
          graph: {
            ...captured,
            nodes: [{ ...capturedNode, name: "Old name" }],
          },
          run: history(),
        },
      },
    };
    expect(feedNodeName(root, "n1/n2", { n1: captured }, run)).toBe(
      "Research / Summarize",
    );
    expect(feedNodeName(root, "n1/missing", {}, run)).toBe(
      "Research / missing",
    );
  });
});
