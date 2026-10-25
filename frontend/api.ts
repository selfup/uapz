import { invoke, isTauri } from "@tauri-apps/api/core";

/** All filesystem, provider and execution operations stay behind the Rust service. */
export function request<T>(
  method: string,
  args: Record<string, unknown> = {},
): Promise<T> {
  if (!isTauri())
    return Promise.reject(
      new Error("Open uapz with npm run desktop to access your workspace."),
    );
  return invoke<T>("request", { method, args });
}
