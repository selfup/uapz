import { describe, expect, it } from "vitest";
import {
  copyGraphSelection,
  pastePlacement,
  visibleCanvasCenter,
} from "./editorClipboard";
import { emptyGraph } from "./workspace";
import type { GraphDoc } from "./types";

const graphWithNodes = (): GraphDoc => ({
  ...emptyGraph(),
  view: { pan_x: 0, pan_y: 0, zoom: 1 },
  nodes: [
    {
      id: "n1",
      x: 10,
      y: 10,
      data: { type: "input", kind: "text", default: "", required: true },
    },
    {
      id: "n2",
      x: 200,
      y: 10,
      data: { type: "data", path: "a.csv", format: "csv" },
    },
  ],
  edges: [{ from: "n1", to: "n2" }],
});

describe("editorClipboard", () => {
  it("copies selected nodes and connecting edges", () => {
    const clip = copyGraphSelection(graphWithNodes(), ["n1", "n2"]);
    expect(clip?.nodes.map((node) => node.id)).toEqual(["n1", "n2"]);
    expect(clip?.edges).toEqual([{ from: "n1", to: "n2" }]);
    expect(clip?.pastes).toBe(0);
  });

  it("returns null for an empty selection", () => {
    expect(copyGraphSelection(graphWithNodes(), [])).toBeNull();
  });

  it("steps repeated on-screen pastes and centers off-screen ones", () => {
    const graph = graphWithNodes();
    const onScreen = copyGraphSelection(graph, ["n1"])!;
    onScreen.pastes = 2;
    expect(
      pastePlacement(graph, onScreen, { width: 800, height: 600 }),
    ).toEqual({ offset: { x: 80, y: 80 } });

    const offScreen = copyGraphSelection(graph, ["n1"])!;
    offScreen.pastes = 1;
    expect(
      pastePlacement(graph, offScreen, { width: 0, height: 0 }),
    ).toEqual({
      at: visibleCanvasCenter(graph, { width: 0, height: 0 }),
    });

    expect(
      pastePlacement(graph, onScreen, { width: 800, height: 600 }, {
        x: 5,
        y: 6,
      }),
    ).toEqual({ at: { x: 5, y: 6 } });
  });
});
