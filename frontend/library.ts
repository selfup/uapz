import { NODE_LABELS } from "./graph";
import type { NodeType } from "./types";

export type CreateType = NodeType | "blank" | "database-write";

/** What a library row adds, carried on a drag so the canvas can place it at the drop. */
export type LibraryItem =
  | { kind: "type"; type: CreateType }
  | { kind: "kind"; name: string; color: string }
  | { kind: "agent"; name: string }
  | { kind: "database"; name: string }
  | { kind: "script"; name: string }
  | { kind: "data"; name: string };

export function parseLibraryItem(payload: string): LibraryItem | null {
  let value: unknown;
  try {
    value = JSON.parse(payload);
  } catch {
    return null;
  }
  if (!value || typeof value !== "object") return null;
  const item = value as Record<string, unknown>;
  if (
    item.kind === "type" &&
    typeof item.type === "string" &&
    (item.type === "blank" ||
      item.type === "database-write" ||
      Object.hasOwn(NODE_LABELS, item.type))
  )
    return { kind: "type", type: item.type as CreateType };
  if (typeof item.name !== "string") return null;
  if (item.kind === "kind" && typeof item.color === "string")
    return { kind: "kind", name: item.name, color: item.color };
  if (
    item.kind === "agent" ||
    item.kind === "database" ||
    item.kind === "script" ||
    item.kind === "data"
  )
    return { kind: item.kind, name: item.name };
  return null;
}
