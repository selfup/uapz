const LAYOUT_KEY = "uapz.layout";

export type Theme = "system" | "dark" | "light";

export interface Layout {
  leftWidth?: number;
  rightWidth?: number;
  libraryOpen?: boolean;
  inspectorOpen?: boolean;
  theme?: Theme;
  lastGraph?: { workspace: string; name: string };
}

/** Per-user conveniences only. Storage can be missing, blocked or cleared, and
 * the app must behave the same without it. */
export function readLayout(): Layout {
  try {
    const raw = window.localStorage.getItem(LAYOUT_KEY);
    const value: unknown = raw ? JSON.parse(raw) : null;
    return value && typeof value === "object" ? (value as Layout) : {};
  } catch {
    return {};
  }
}

export function writeLayout(patch: Layout) {
  try {
    window.localStorage.setItem(
      LAYOUT_KEY,
      JSON.stringify({ ...readLayout(), ...patch }),
    );
  } catch {
    // The layout memory is optional.
  }
}

export const clamp = (
  value: unknown,
  low: number,
  high: number,
  fallback: number,
) =>
  typeof value === "number" && Number.isFinite(value)
    ? Math.min(high, Math.max(low, value))
    : fallback;
