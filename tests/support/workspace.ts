import { test as base, expect, type Page } from "@playwright/test";
import { defaultNodeData } from "../../frontend/graph";
import type { GraphDoc, NodeType } from "../../frontend/types";

export interface Reply {
  method: string;
  args?: Record<string, unknown>;
  value?: unknown;
  error?: string;
  gate?: string;
}

/** Queue one RPC reply; gates let tests release responses in any order. */
export async function queueReply(page: Page, reply: Reply) {
  await page.evaluate(
    (reply) => (window as any).__testState.replies.push(reply),
    reply,
  );
}

export async function releaseReply(page: Page, gate: string) {
  await expect
    .poll(() =>
      page.evaluate(
        (gate) => Boolean((window as any).__testState.pending[gate]),
        gate,
      ),
    )
    .toBe(true);
  await page.evaluate(
    (gate) => (window as any).__testState.pending[gate](),
    gate,
  );
  await expect
    .poll(() =>
      page.evaluate(
        (gate) => Boolean((window as any).__testState.released[gate]),
        gate,
      ),
    )
    .toBe(true);
  // Allow React to render the released IPC response before asserting it was ignored.
  await page.evaluate(
    () =>
      new Promise<void>((resolve) =>
        requestAnimationFrame(() => requestAnimationFrame(() => resolve())),
      ),
  );
}

export async function callsFor(
  page: Page,
  method: string,
): Promise<{ method: string; args: Record<string, unknown> }[]> {
  return page.evaluate(
    (method) =>
      (window as any).__testState.calls.filter(
        (call: { method: string }) => call.method === method,
      ),
    method,
  );
}

export const leaf: GraphDoc = {
  version: 1,
  name: "Extract",
  view: { pan_x: 50, pan_y: 30, zoom: 1 },
  nodes: [
    {
      id: "n0",
      name: "Original source",
      x: 50,
      y: 70,
      data: {
        ...defaultNodeData("input"),
        type: "input",
        kind: "text",
        default: "Example",
        required: true,
      },
    },
  ],
  edges: [],
  outputs: { heading: "n0", body: "n0" },
};
export const child: GraphDoc = {
  version: 1,
  name: "Parts",
  view: { pan_x: 40, pan_y: 30, zoom: 1 },
  nodes: [
    {
      id: "n4",
      name: "Extract content",
      x: 80,
      y: 70,
      data: { type: "workflow", graph: "Extract", output: "", inputs: {} },
    },
  ],
  edges: [],
  outputs: { heading: "n4.heading", body: "n4.body" },
};
export const parent: GraphDoc = {
  version: 1,
  name: "Demo",
  view: { pan_x: 30, pan_y: 30, zoom: 1 },
  nodes: [
    {
      id: "n0",
      name: "message",
      x: 20,
      y: 60,
      data: {
        type: "input",
        kind: "text",
        default: "Draft text",
        required: true,
      },
    },
    {
      id: "n1",
      name: "Create parts",
      x: 320,
      y: 60,
      data: { type: "workflow", graph: "Parts", output: "", inputs: {} },
    },
    {
      id: "n2",
      name: "Save heading",
      x: 320,
      y: 340,
      data: {
        type: "file",
        path: "heading.md",
        mode: "overwrite",
        unfence: false,
        bundle: false,
        review: true,
      },
    },
  ],
  edges: [
    { from: "n0", to: "n1" },
    { from: "n1", output: "heading", to: "n2" },
  ],
  outputs: { heading: "n1.heading", body: "n1.body" },
};

async function mockService(page: Page) {
  const catalog = (
    [
      "input",
      "workflow",
      "agent",
      "database",
      "script",
      "data",
      "directory",
      "file",
    ] as NodeType[]
  ).map((type) => ({
    type,
    label: type,
    color: "#8888cc",
    data: defaultNodeData(type),
  }));
  await page.addInitScript(
    ({ parent, child, leaf, catalog }) => {
      const host = window as unknown as {
        isTauri: boolean;
        __TAURI_INTERNALS__: {
          invoke: (
            command: string,
            payload: { method: string; args: Record<string, any> },
          ) => Promise<any>;
        };
        __testState: any;
      };
      host.isTauri = true;
      const state: any = {
        graphs: { Demo: parent, Parts: child, Extract: leaf },
        runtime: null,
        history: [],
        calls: [],
        saved: null,
        replies: [] as Reply[],
        pending: {} as Record<string, () => void>,
        released: {} as Record<string, boolean>,
      };
      host.__testState = state;
      host.__TAURI_INTERNALS__ = {
        invoke: async (command, { method, args = {} }) => {
          if (command !== "request")
            throw new Error(`Unexpected IPC ${command}`);
          state.calls.push({ method, args: structuredClone(args) });
          const index = state.replies.findIndex(
            (reply: Reply) =>
              reply.method === method &&
              Object.entries(reply.args || {}).every(
                ([key, value]) =>
                  JSON.stringify(args[key]) === JSON.stringify(value),
              ),
          );
          if (index >= 0) {
            const reply: Reply = state.replies.splice(index, 1)[0];
            if (reply.gate) {
              const gate = reply.gate;
              await new Promise<void>((resolve) => {
                state.pending[gate] = resolve;
              });
              delete state.pending[gate];
              state.released[gate] = true;
            }
            if (reply.error) throw new Error(reply.error);
            if (Object.hasOwn(reply, "value"))
              return structuredClone(reply.value);
          }
          switch (method) {
            case "bootstrap":
              return {
                workspace: "/test/workspace",
                graphs: Object.keys(state.graphs),
                kinds: [],
                agents: state.agents || [],
                databases: state.databases || [],
                prompts: [],
                scripts: [],
                data: [],
                catalog: {
                  nodes: catalog,
                  providers: [],
                  model_types: [],
                  outputs: [],
                  engines: [],
                  runtimes: [],
                  formats: [],
                  write_modes: [],
                  input_types: [],
                },
              };
            case "load_graph":
              return structuredClone(state.graphs[args.name]);
            case "save_graph":
              state.saved = structuredClone(args.doc);
              state.graphs[args.doc.name] = state.saved;
              return {
                name: args.doc.name,
                json: "graphs/Demo.json",
                markdown: "graphs/Demo.md",
              };
            case "validate_node":
              return [];
            case "validate_graph":
              return { issues: [], fields: [], sinks: ["n2"] };
            case "workflow_fields":
              return [];
            case "history":
              return structuredClone(state.history);
            case "runtime_state":
              return structuredClone(state.runtime);
            case "prepare_run":
              return {
                targets: args.targets || ["n2"],
                fields: [
                  {
                    node: "n0",
                    name: "message",
                    config: {
                      kind: "text",
                      default: "Draft text",
                      required: true,
                    },
                  },
                ],
                inputs: { message: "Draft text" },
              };
            case "start_run":
              state.runtime = {
                run_id: "live-1",
                running: true,
                cancelling: false,
                graph: structuredClone(args.doc),
                targets: ["n2"],
                marks: { n0: "ran", n1: "ran", n2: "running", "n1/n4": "ran" },
                log: ["n1/n4: completed"],
                stream: null,
                workflows: {
                  n1: structuredClone(child),
                  "n1/n4": structuredClone(leaf),
                },
                review: {
                  id: "review-1",
                  node: "n2",
                  files: [
                    {
                      path: "heading.md",
                      before_bytes: null,
                      after_bytes: 8,
                      diff: "+ Example\n",
                    },
                  ],
                },
                run: null,
                error: null,
              };
              return { run_id: "live-1" };
            case "review": {
              if (args.review_id !== "review-1" || args.run_id !== "live-1")
                throw new Error("Stale review");
              state.runtime.running = false;
              state.runtime.review = null;
              const result = {
                state: "ran",
                preview: "Example",
                label: "Text · 7 bytes",
                complete: true,
              };
              state.runtime.run = {
                id: "20261025T120000000Z",
                graph: "Demo",
                targets: ["n2"],
                elapsed_ms: 100,
                inputs: { message: "Keep this input" },
                nodes: {
                  n0: result,
                  n1: {
                    ...result,
                    named_outputs: {
                      heading: result,
                      body: { ...result, preview: "Body text" },
                    },
                  },
                  n2: result,
                },
                log: ["Completed"],
                outcome:
                  args.decision === "apply"
                    ? { kind: "ok" }
                    : { kind: "failed", error: "File changes rejected" },
                workflows: state.runtime.workflows,
                calls: {},
              };
              state.history = [state.runtime.run];
              return null;
            }
            case "cancel_run":
              state.runtime.running = false;
              state.runtime.review = null;
              state.runtime.run = {
                id: "cancelled",
                graph: "Demo",
                targets: ["n2"],
                elapsed_ms: 10,
                inputs: {},
                nodes: {},
                log: [],
                outcome: { kind: "cancelled" },
                workflows: state.runtime.workflows,
              };
              state.history = [state.runtime.run];
              return null;
            case "clear_cache":
              return 3;
            case "clear_history":
              state.history = [];
              return 1;
            case "output":
              return {
                text: "Example full output",
                base64: null,
                label: "Text",
                saved: null,
                complete: true,
              };
            default:
              throw new Error(`Unexpected method ${method}`);
          }
        },
      };
    },
    { parent, child, leaf, catalog },
  );
}

export const test = base.extend({
  page: async ({ page }, use) => {
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    page.on("console", (message) => {
      if (["error", "warning"].includes(message.type()))
        errors.push(message.text());
    });
    await mockService(page);
    await page.goto("/");
    await page
      .locator(".library")
      .getByRole("button", { name: "Demo", exact: true })
      .click();
    await use(page);
    expect(errors, "Unexpected browser errors or warnings").toEqual([]);
  },
});
export { expect };
