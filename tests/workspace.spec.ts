import { test, expect, type Page } from "@playwright/test";
import { defaultNodeData } from "../frontend/graph";
import type { GraphDoc, NodeType } from "../frontend/types";

const leaf: GraphDoc = {
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
const child: GraphDoc = {
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
const parent: GraphDoc = {
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
      };
      host.__testState = state;
      host.__TAURI_INTERNALS__ = {
        invoke: async (command, { method, args = {} }) => {
          if (command !== "request")
            throw new Error(`Unexpected IPC ${command}`);
          state.calls.push({ method, args: structuredClone(args) });
          switch (method) {
            case "bootstrap":
              return {
                workspace: "/test/workspace",
                graphs: Object.keys(state.graphs),
                kinds: [],
                agents: [],
                databases: [],
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
                outcome: { kind: args.decision === "apply" ? "ok" : "failed" },
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

const browserErrors = new WeakMap<Page, string[]>();
test.beforeEach(async ({ page }) => {
  const errors: string[] = [];
  browserErrors.set(page, errors);
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
});
test.afterEach(async ({ page }) => {
  expect(browserErrors.get(page)).toEqual([]);
});

test("named ports, searchable library and graph edits roundtrip without changing wire handles", async ({
  page,
}, testInfo) => {
  await expect(
    page.locator('[data-id="n1"] [data-handleid="port:heading"]'),
  ).toBeVisible();
  await expect(
    page.locator('[data-id="n1"] [data-handleid="port:body"]'),
  ).toBeVisible();
  await page.getByLabel("Search library").fill("Agent");
  await expect(
    page
      .locator(".library")
      .getByRole("button", { name: "Agent", exact: true }),
  ).toBeVisible();
  await expect(
    page
      .locator(".library")
      .getByRole("button", { name: "Write file", exact: true }),
  ).toHaveCount(0);
  await page.getByLabel("Clear search").click();
  await page.locator('[data-id="n0"]').click();
  await page.getByLabel("Name", { exact: true }).fill("messageEdited");
  await page.getByRole("button", { name: "Save", exact: true }).click();
  const saved = await page.evaluate(() => (window as any).__testState.saved);
  expect(saved.nodes[0].name).toBe("messageEdited");
  expect(saved.edges).toEqual(parent.edges);
  expect(saved.outputs).toEqual(parent.outputs);
  await page.screenshot({
    path: testInfo.outputPath("workspace.png"),
    fullPage: true,
  });
});

test("nested navigation preserves root draft, selection, viewport and pending input values", async ({
  page,
}, testInfo) => {
  await page.getByRole("button", { name: "Run workflow", exact: true }).click();
  await page.getByLabel("Run input message").fill("Keep this input");
  await page.locator('[data-id="n1"]').dblclick();
  await expect(
    page.getByText("Saved workflow · Read only", { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Save", exact: true }),
  ).toBeDisabled();
  await expect(
    page.getByRole("button", { name: "Add node", exact: true }),
  ).toHaveCount(0);
  await page.locator('[data-id="n4"]').dblclick();
  await expect(
    page.locator('[data-id="n0"]').getByText("Original source"),
  ).toBeVisible();
  await page
    .getByRole("navigation", { name: "Workflow breadcrumbs" })
    .getByRole("button", { name: "Create parts", exact: true })
    .click();
  await expect(page.locator('[data-id="n4"]')).toHaveClass(/selected/);
  await page.screenshot({
    path: testInfo.outputPath("child-workflow.png"),
    fullPage: true,
  });
  await page
    .getByRole("navigation", { name: "Workflow breadcrumbs" })
    .getByRole("button", { name: "Demo", exact: true })
    .click();
  await expect(page.getByLabel("Run input message")).toHaveValue(
    "Keep this input",
  );
  await expect(page.locator('[data-id="n1"]')).toHaveClass(/selected/);
  await page.getByRole("button", { name: "Start run", exact: true }).click();
  await expect(
    page.getByRole("region", { name: "Review file changes" }),
  ).toBeVisible();
  const started = await page.evaluate(
    () =>
      (window as any).__testState.calls.find(
        (call: any) => call.method === "start_run",
      ).args,
  );
  expect(started.inputs.message).toBe("Keep this input");
  expect(started.doc.nodes).toEqual(parent.nodes);
  expect(started.doc.edges).toEqual(parent.edges);
  expect(started.doc.view).toEqual(parent.view);
});

test("running child navigation uses snapshots, preserves reviews and exposes named results", async ({
  page,
}, testInfo) => {
  await page.getByRole("button", { name: "Run workflow", exact: true }).click();
  await page.getByRole("button", { name: "Start run", exact: true }).click();
  await expect(
    page.getByRole("region", { name: "Review file changes" }),
  ).toBeVisible();
  await page.evaluate(
    () =>
      ((window as any).__testState.graphs.Extract.nodes[0].name =
        "Changed on disk"),
  );
  await page.locator('[data-id="n1"]').dblclick();
  await expect(
    page.getByText("Run snapshot · Read only", { exact: true }),
  ).toBeVisible();
  await page.locator('[data-id="n4"]').dblclick();
  await expect(
    page.locator('[data-id="n0"]').getByText("Original source"),
  ).toBeVisible();
  await expect(page.getByText("Changed on disk")).toHaveCount(0);
  await page
    .getByRole("button", { name: "Review changes", exact: true })
    .click();
  await page
    .getByRole("button", { name: "Apply changes", exact: true })
    .click();
  await expect(
    page.getByRole("region", { name: "Review file changes" }),
  ).toHaveCount(0);
  await page.locator('[data-id="n1"]').click();
  await page.getByRole("tab", { name: "Results", exact: true }).click();
  await expect(
    page.locator(".inspector").getByText("Body text", { exact: true }),
  ).toBeVisible();
  await page.screenshot({
    path: testInfo.outputPath("named-results.png"),
    fullPage: true,
  });
  await page.getByRole("tab", { name: "History", exact: true }).click();
  await page.locator(".inspector .ins-run > summary").click();
  await expect(
    page
      .locator(".inspector")
      .getByRole("button", { name: "Inspect this run", exact: true }),
  ).toBeVisible();
});

test("node library exposes all typed forms and saving keeps their typed configs", async ({
  page,
}) => {
  page.on("dialog", (dialog) => dialog.accept());
  await page.getByRole("button", { name: "New", exact: true }).click();
  for (const label of [
    "Run input",
    "Workflow",
    "Agent",
    "Database",
    "Script",
    "Data",
    "Directory",
    "Write file",
  ]) {
    await page
      .locator(".library")
      .getByRole("button", { name: label, exact: true })
      .click();
    await expect(
      page.getByRole("tab", { name: "Configure", exact: true }),
    ).toBeVisible();
    await expect(
      page.locator(".inspector").getByLabel("Name", { exact: true }),
    ).toBeVisible();
  }
  await page.getByRole("button", { name: "Save", exact: true }).click();
  const saved = await page.evaluate(() => (window as any).__testState.saved);
  expect(saved.nodes.map((node: any) => node.data.type)).toEqual([
    "input",
    "workflow",
    "agent",
    "database",
    "script",
    "data",
    "directory",
    "file",
  ]);
});

test("unsaved edits can be kept or discarded in the desktop confirmation dialog", async ({
  page,
}) => {
  await page.locator('[data-id="n0"]').click();
  await page.getByLabel("Name", { exact: true }).fill("keepMe");
  await page.getByRole("button", { name: "New", exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "Confirm change" });
  await expect(dialog).toBeVisible();
  await dialog.getByRole("button", { name: "Cancel", exact: true }).click();
  await expect(page.getByLabel("Name", { exact: true })).toHaveValue("keepMe");
  await page.getByRole("button", { name: "New", exact: true }).click();
  await dialog.getByRole("button", { name: "Continue", exact: true }).click();
  await expect(
    page.getByRole("heading", { name: "Build a workflow" }),
  ).toBeVisible();
});
