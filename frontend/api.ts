import { invoke, isTauri } from "@tauri-apps/api/core";
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
