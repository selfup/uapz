import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { Method, Protocol } from "./protocol";
import { decodeResponse } from "./responses";

/** All filesystem, provider and execution operations stay behind the Rust service. */
export async function request<M extends Method>(
  method: M,
  ...parameters: Protocol[M]["args"] extends undefined
    ? [args?: Record<string, never>]
    : [args: Protocol[M]["args"]]
): Promise<Protocol[M]["result"]> {
  if (!isTauri())
    throw new Error("Open uapz with npm run desktop to access your workspace.");
  const value = await invoke<unknown>("request", {
    method,
    args: parameters[0] || {},
  });
  return decodeResponse(method, value);
}

/** Subscribe to an event from the desktop shell. Outside it, nothing arrives. */
export async function onHostEvent(
  event: string,
  handler: () => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  try {
    return await listen(event, () => handler());
  } catch {
    return () => {};
  }
}

/** Exit the desktop shell now; the renderer has already settled unsaved work. */
export async function quitApp() {
  if (!isTauri()) return;
  await invoke("quit");
}
