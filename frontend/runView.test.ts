import { describe, expect, it } from "vitest";
import { deriveRunView } from "./runView";
import type { Run, RuntimeState } from "./types";
import { emptyGraph } from "./workspace";

const baseRun = (id: string, extra: Partial<Run> = {}): Run => ({
  id,
  graph: "demo",
  targets: [],
  elapsed_ms: 0,
  outcome: { kind: "ok" },
  nodes: {},
  log: [],
  ...extra,
});

describe("deriveRunView", () => {
  it("returns idle defaults with no runtime", () => {
    const view = deriveRunView({
      runtime: null,
      attachedRun: null,
      shownRun: null,
      historical: false,
      running: false,
      activeSelected: null,
      readOnly: false,
      remembered: { nodes: {}, origins: {} },
    });
    expect(view).toMatchObject({
      runMatches: false,
      currentRun: null,
      childRun: null,
      outputRun: null,
      liveLog: [],
      liveStream: null,
      marks: {},
      outputContext: undefined,
    });
  });

  it("prefers remembered origins for root output when not historical", () => {
    const current = baseRun("current");
    const remembered = baseRun("remembered");
    const view = deriveRunView({
      runtime: null,
      attachedRun: null,
      shownRun: current,
      historical: false,
      running: false,
      activeSelected: "n1",
      readOnly: false,
      remembered: { nodes: {}, origins: { n1: remembered } },
    });
    expect(view.currentRun).toBe(current);
    expect(view.outputRun).toBe(remembered);
    expect(view.outputContext).toEqual({
      graph: "demo",
      runId: "remembered",
    });
  });

  it("ignores remembered origins for historical or read-only views", () => {
    const current = baseRun("current");
    const remembered = baseRun("remembered");
    const historical = deriveRunView({
      runtime: null,
      attachedRun: null,
      shownRun: current,
      historical: true,
      running: false,
      activeSelected: "n1",
      readOnly: false,
      remembered: { nodes: {}, origins: { n1: remembered } },
    });
    const child = deriveRunView({
      runtime: null,
      attachedRun: null,
      shownRun: current,
      historical: false,
      running: false,
      activeChild: { path: "n2" },
      activeSelected: "n1",
      readOnly: true,
      remembered: { nodes: {}, origins: { n1: remembered } },
    });
    expect(historical.outputRun).toBe(current);
    expect(child.outputRun).toBe(current);
    expect(child.outputContext?.runId).toBe("current");
  });

  it("strips child path prefixes from live log and stream", () => {
    const childRun = baseRun("child", {
      log: ["local"],
      nodes: { n9: { state: "ran" } },
    });
    const parent = baseRun("parent", {
      calls: {
        n1: { graph: emptyGraph(), run: childRun },
      },
    });
    const runtime: RuntimeState = {
      run_id: "live",
      running: true,
      cancelling: false,
      graph: emptyGraph(),
      targets: [],
      marks: { "n1/n9": "running", n2: "pending" },
      log: ["n1/child-line", "root-line", "n1/more"],
      stream: { id: "n1/n9", text: "token" },
      workflows: {},
      review: null,
      run: parent,
      error: null,
    };
    const view = deriveRunView({
      runtime,
      attachedRun: "live",
      shownRun: null,
      historical: false,
      running: true,
      activeChild: { path: "n1" },
      activeSelected: "n9",
      readOnly: true,
      remembered: { nodes: {}, origins: {} },
    });
    expect(view.runMatches).toBe(true);
    expect(view.childRun).toBe(childRun);
    expect(view.liveLog).toEqual(["child-line", "more"]);
    expect(view.liveStream).toEqual({ id: "n9", text: "token" });
    expect(view.marks).toEqual({ n9: "running" });
    expect(view.outputContext).toEqual({
      graph: "demo",
      runId: "parent",
      callPath: "n1",
    });
  });

  it("uses finished child run log when not live-matching", () => {
    const childRun = baseRun("child", { log: ["done"] });
    const parent = baseRun("parent", {
      calls: { n1: { graph: emptyGraph(), run: childRun } },
    });
    const view = deriveRunView({
      runtime: null,
      attachedRun: null,
      shownRun: parent,
      historical: true,
      running: false,
      activeChild: { path: "n1" },
      activeSelected: "n9",
      readOnly: true,
      remembered: { nodes: {}, origins: {} },
    });
    expect(view.liveLog).toEqual(["done"]);
    expect(view.liveStream).toBeNull();
    expect(view.marks).toEqual({});
  });
});
