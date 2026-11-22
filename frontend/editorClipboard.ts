import {
  CARD_SIZE,
  cloneNodes,
  flowToGraphPoint,
  graphToFlowPoint,
  type Placement,
  type Point,
} from "./graph";
import type { EdgeDoc, GraphDoc, NodeDoc } from "./types";

export type EditorClipboard = {
  nodes: NodeDoc[];
  edges: EdgeDoc[];
  pastes: number;
};

export function copyGraphSelection(
  graph: GraphDoc,
  ids: string[],
): EditorClipboard | null {
  if (!ids.length) return null;
  const chosen = new Set(ids);
  return {
    nodes: graph.nodes
      .filter((node) => chosen.has(node.id))
      .map((node) => structuredClone(node)),
    edges: graph.edges.filter(
      (edge) => chosen.has(edge.from) && chosen.has(edge.to),
    ),
    pastes: 0,
  };
}

export function visibleCanvasCenter(
  graph: GraphDoc,
  size: { width: number; height: number },
): Point {
  const center = flowToGraphPoint(
    {
      x: size.width / 2,
      y: size.height / 2,
    },
    graph.view,
  );
  return {
    x: center.x - CARD_SIZE.width / 2,
    y: center.y - CARD_SIZE.height / 2,
  };
}

export function pastePlacement(
  graph: GraphDoc,
  clip: EditorClipboard,
  viewport: { width: number; height: number },
  at?: Point,
): Placement {
  if (at) return { at };
  const onScreen = (node: NodeDoc) => {
    const point = graphToFlowPoint({ x: node.x, y: node.y }, graph.view);
    return (
      point.x >= 0 &&
      point.y >= 0 &&
      point.x <= viewport.width &&
      point.y <= viewport.height
    );
  };
  // Repeated pastes step away from the originals; copies from another
  // workflow or off-screen land in the middle of the visible canvas.
  const step = 40 * clip.pastes;
  return clip.nodes.some(onScreen)
    ? { offset: { x: step, y: step } }
    : { at: visibleCanvasCenter(graph, viewport) };
}

export function insertCopiedNodes(
  graph: GraphDoc,
  nodes: NodeDoc[],
  edges: EdgeDoc[],
  placement: Placement,
): { graph: GraphDoc; ids: string[] } {
  return cloneNodes(graph, nodes, edges, placement);
}
