import { beforeEach, describe, expect, it, vi } from "vitest";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { request } from "./api";
import { emptyGraph } from "./workspace";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(),
  isTauri: vi.fn(),
}));

describe("workspace service requests", () => {
  beforeEach(() => {
    vi.resetAllMocks();
    vi.mocked(isTauri).mockReturnValue(true);
  });

  it("passes workspace arguments and preserves the service response", async () => {
    const args = {
      doc: { ...emptyGraph(), name: "daily" },
      inputs: { source: "notes.txt" },
    };
    const response = { targets: ["summary"], fields: [], inputs: args.inputs };
    vi.mocked(invoke).mockResolvedValue(response);

    await expect(request("prepare_run", args)).resolves.toEqual(response);
    expect(invoke).toHaveBeenCalledExactlyOnceWith("request", {
      method: "prepare_run",
      args,
    });
  });

  it("preserves service failures so callers can display the original details", async () => {
    const failure = { message: "Missing workflow", graph: "daily" };
    vi.mocked(invoke).mockRejectedValue(failure);

    await expect(request("bootstrap")).rejects.toBe(failure);
    expect(invoke).toHaveBeenCalledExactlyOnceWith("request", {
      method: "bootstrap",
      args: {},
    });
  });

  it("rejects browser access without attempting a desktop command", async () => {
    vi.mocked(isTauri).mockReturnValue(false);

    await expect(request("bootstrap")).rejects.toThrow(
      "Open uapz with npm run desktop to access your workspace.",
    );
    expect(invoke).not.toHaveBeenCalled();
  });

  it("rejects invalid data before the caller can put it into UI state", async () => {
    vi.mocked(invoke).mockResolvedValue({ ...emptyGraph(), nodes: "invalid" });
    await expect(request("load_graph", { name: "daily" })).rejects.toThrow(
      "Invalid load_graph response: response.nodes: expected an array",
    );
  });
});
