import { describe, expect, it } from "vitest";
import {
  createNodeData,
  materializeLibraryItem,
  scriptOrDataFromLibrary,
} from "./materializeLibrary";
import { NODE_COLORS } from "./graph";

describe("materializeLibrary", () => {
  it("builds typed defaults and write-database variants", () => {
    expect(createNodeData("blank")).toBeUndefined();
    expect(createNodeData("agent")?.type).toBe("agent");
    const write = createNodeData("database-write");
    expect(write).toMatchObject({ type: "database", read_only: false });
  });

  it("resolves presets from the library by name", () => {
    const library = {
      agents: [
        {
          name: "local",
          config: {
            provider: "ollama" as const,
            endpoint: "",
            model_type: "generative" as const,
            model: "llama",
            auth: { kind: "none" as const },
            prompt: "hi",
            context_size: 1,
            temperature: 0,
            output: "text" as const,
          },
        },
      ],
      databases: [
        {
          name: "db",
          config: {
            engine: "sqlite" as const,
            path: "x.sqlite",
            url_env: "",
            input: "",
            input_table: "input",
            query: "",
            read_only: true,
          },
        },
      ],
    };
    expect(
      materializeLibraryItem({ kind: "agent", name: "local" }, library),
    ).toMatchObject({
      data: { type: "agent", model: "llama" },
      kind: { name: "local", color: NODE_COLORS.agent },
    });
    expect(
      materializeLibraryItem({ kind: "database", name: "missing" }, library),
    ).toBeNull();
  });

  it("prefers retargeting script and data rows when asked", () => {
    expect(
      materializeLibraryItem(
        { kind: "script", name: "run.py" },
        null,
        { preferRetarget: true },
      ),
    ).toEqual({ retarget: { type: "script", name: "run.py" } });
    expect(
      materializeLibraryItem(
        { kind: "data", name: "rows.csv" },
        null,
        { preferRetarget: false },
      ),
    ).toMatchObject({
      data: { type: "data", path: "rows.csv", format: "csv" },
    });
    expect(scriptOrDataFromLibrary("script", "tool.py")).toMatchObject({
      type: "script",
      path: "tool.py",
      runtime: "python",
    });
  });
});
