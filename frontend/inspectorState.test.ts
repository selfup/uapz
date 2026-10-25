import { describe, expect, it } from "vitest";
import { compareNodeOutputs, sameNodeConfig } from "./inspectorState";
import type { NodeData } from "./types";

describe("node result configuration comparison", () => {
  const agent: NodeData = {
    type: "agent",
    provider: "ollama",
    endpoint: "",
    model_type: "generative",
    model: "llama3.1",
    auth: { kind: "none" },
    prompt: "Summarize {{source}}",
    context_size: 8192,
    temperature: 0.7,
    output: "text",
  };

  it("keeps a just-finished result current after Rust omits an empty message", () => {
    expect(sameNodeConfig(agent, { ...agent, message: "" })).toBe(true);
  });

  it("compares configuration values independently of object field order", () => {
    const reordered = Object.fromEntries(
      Object.entries(agent).reverse(),
    ) as NodeData;
    expect(sameNodeConfig(agent, reordered)).toBe(true);
  });

  it("recognizes when a prompt or authentication setting changes", () => {
    expect(sameNodeConfig(agent, { ...agent, prompt: "Changed prompt" })).toBe(
      false,
    );
    expect(
      sameNodeConfig(agent, {
        ...agent,
        auth: { kind: "env", var: "MODEL_KEY" },
      }),
    ).toBe(false);
  });

  it("handles the omitted write-review flag without hiding an enabled review", () => {
    const file: NodeData = {
      type: "file",
      path: "report.txt",
      mode: "create",
      bundle: false,
      unfence: true,
    };
    expect(sameNodeConfig(file, { ...file, review: false })).toBe(true);
    expect(sameNodeConfig(file, { ...file, review: true })).toBe(false);
  });
});

describe("previous output comparison", () => {
  it("uses content keys even when previews share the same prefix", () => {
    expect(
      compareNodeOutputs(
        { key: "a", preview: "shared prefix" },
        { key: "b", preview: "shared prefix" },
      ),
    ).toBe("different");
    expect(compareNodeOutputs({ key: "a" }, { key: "a" })).toBe("same");
  });

  it("distinguishes matching full text from matching truncated previews", () => {
    const text = { label: "Text", preview: "hello", complete: true };
    expect(compareNodeOutputs(text, text)).toBe("same");
    expect(compareNodeOutputs(text, { ...text, complete: false })).toBe(
      "preview",
    );
  });

  it("does not claim unavailable binary contents or failures match", () => {
    expect(compareNodeOutputs({ label: "Image" }, { label: "Image" })).toBe(
      "unknown",
    );
    expect(
      compareNodeOutputs({ key: "a" }, { key: "a", error: "failed" }),
    ).toBe("different");
  });
});
