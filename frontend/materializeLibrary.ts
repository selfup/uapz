import {
  dataFormatFor,
  defaultNodeData,
  NODE_COLORS,
  nodeColor,
  scriptRuntimeFor,
} from "./graph";
import type { CreateType, LibraryItem } from "./library";
import type { Library, NodeData, NodeType } from "./types";

export function createNodeData(type: CreateType): NodeData | undefined {
  if (type === "blank") return undefined;
  if (type === "database-write") {
    const database = defaultNodeData("database");
    if (database.type === "database") return { ...database, read_only: false };
    return undefined;
  }
  return defaultNodeData(type as NodeType);
}

export type MaterializedLibraryNode = {
  data?: NodeData;
  kind?: { name: string; color: string };
  /** Point the selected script/data node at this path instead of adding a node. */
  retarget?: { type: "script" | "data"; name: string };
};

/** Resolve a library row into node data (or a retarget of the selection). */
export function materializeLibraryItem(
  item: LibraryItem,
  library: Pick<Library, "agents" | "databases"> | null | undefined,
  options: { preferRetarget: boolean } = { preferRetarget: true },
): MaterializedLibraryNode | null {
  switch (item.kind) {
    case "type":
      return { data: createNodeData(item.type) };
    case "kind":
      return { kind: { name: item.name, color: item.color } };
    case "agent": {
      const agent = library?.agents.find((entry) => entry.name === item.name);
      if (!agent) return null;
      return {
        data: { type: "agent", ...agent.config },
        kind: { name: agent.name, color: NODE_COLORS.agent },
      };
    }
    case "database": {
      const database = library?.databases.find(
        (entry) => entry.name === item.name,
      );
      if (!database) return null;
      const data: NodeData = { type: "database", ...database.config };
      return {
        data,
        kind: {
          name: database.name,
          color: nodeColor({ id: "", x: 0, y: 0, data }),
        },
      };
    }
    case "script":
      if (options.preferRetarget) return { retarget: { type: "script", name: item.name } };
      return {
        data: {
          type: "script",
          runtime: scriptRuntimeFor(item.name),
          path: item.name,
          args: [],
          timeout_secs: 60,
        },
      };
    case "data":
      if (options.preferRetarget) return { retarget: { type: "data", name: item.name } };
      return {
        data: {
          type: "data",
          path: item.name,
          format: dataFormatFor(item.name),
        },
      };
  }
}

export function scriptOrDataFromLibrary(
  type: "script" | "data",
  name: string,
): NodeData {
  if (type === "script")
    return {
      type: "script",
      runtime: scriptRuntimeFor(name),
      path: name,
      args: [],
      timeout_secs: 60,
    };
  return { type: "data", path: name, format: dataFormatFor(name) };
}
