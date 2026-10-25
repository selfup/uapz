import type { NodeData } from "./types";

function canonical(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(canonical);
  if (value && typeof value === "object")
    return Object.fromEntries(
      Object.entries(value)
        .sort(([left], [right]) => left.localeCompare(right))
        .map(([key, child]) => [key, canonical(child)]),
    );
  return value;
}

/** Core omits empty agent messages and false file review flags when saving a run. */
export function sameNodeConfig(
  left: NodeData | null | undefined,
  right: NodeData | null | undefined,
): boolean {
  const normalize = (data: NodeData | null | undefined) =>
    data?.type === "agent"
      ? { ...data, message: data.message || "" }
      : data?.type === "file"
        ? { ...data, review: Boolean(data.review) }
        : data;
  return (
    JSON.stringify(canonical(normalize(left))) ===
    JSON.stringify(canonical(normalize(right)))
  );
}

type OutputSummary = {
  key?: string | null;
  label?: string | null;
  preview?: string;
  complete?: boolean;
  error?: string | null;
};

/** A matching truncated preview cannot establish that the complete outputs match. */
export function compareNodeOutputs(
  current: OutputSummary,
  previous: OutputSummary,
): "same" | "preview" | "different" | "unknown" {
  if (current.error || previous.error) return "different";
  if (current.key && previous.key)
    return current.key === previous.key ? "same" : "different";
  if (current.label !== previous.label) return "different";
  if (current.preview !== previous.preview) return "different";
  if (current.complete && previous.complete) return "same";
  return current.preview ? "preview" : "unknown";
}
