import { beforeEach, describe, expect, it, vi } from "vitest";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { request } from "./api";

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
      graph: "daily",
      inputs: { source: "notes.txt" },
      fresh: true,
    };
    const response = { targets: ["summary"], fields: [], inputs: args.inputs };
    vi.mocked(invoke).mockResolvedValue(response);

    await expect(request("prepare_run", args)).resolves.toBe(response);
    expect(invoke).toHaveBeenCalledExactlyOnceWith("request", {
      method: "prepare_run",
      args,
    });
  });

  it("preserves service failures so callers can display the original details", async () => {
    const failure = { message: "Missing workflow", graph: "daily" };
    vi.mocked(invoke).mockRejectedValue(failure);

    await expect(request("library")).rejects.toBe(failure);
    expect(invoke).toHaveBeenCalledExactlyOnceWith("request", {
      method: "library",
      args: {},
    });
  });

  it("rejects browser access without attempting a desktop command", async () => {
    vi.mocked(isTauri).mockReturnValue(false);

    await expect(request("library")).rejects.toThrow(
      "Open uapz with npm run desktop to access your workspace.",
    );
    expect(invoke).not.toHaveBeenCalled();
  });
});
