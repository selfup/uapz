import type { Page } from "@playwright/test";
import type {
  AgentConfig,
  GraphDoc,
  Library,
  NodeData,
} from "../frontend/types";
import { test, expect, queueReply, releaseReply } from "./support/workspace";

const agent: AgentConfig = {
  provider: "ollama",
  endpoint: "",
  model_type: "generative",
  model: "local",
  auth: { kind: "none" },
  prompt: "Original instructions",
  context_size: 8192,
  temperature: 0.7,
  output: "text",
};
const database: NodeData = {
  type: "database",
  engine: "sqlite",
  path: "old.sqlite",
  url_env: "",
  input: "",
  input_table: "input",
  query: "SELECT original",
  read_only: true,
};
const library: Library = {
  workspace: "/test/workspace",
  graphs: ["Demo", "Parts", "Extract"],
  kinds: [],
  agents: [{ name: "SQL helper", config: agent }],
  databases: [],
  prompts: ["First prompt", "Second prompt"],
  scripts: [],
  data: [],
  catalog: {
    nodes: [],
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
function graph(data: NodeData): GraphDoc {
  return {
    version: 1,
    name: "Demo",
    view: { pan_x: 30, pan_y: 30, zoom: 1 },
    nodes: [{ id: "n0", name: "Editable step", x: 40, y: 70, data }],
    edges: [],
    outputs: {},
  };
}
async function openInspector(page: Page, data: NodeData) {
  await queueReply(page, { method: "bootstrap", value: library });
  await page.getByRole("button", { name: "Refresh library" }).click();
  await page
    .locator(".library summary")
    .filter({ hasText: /^Agents/ })
    .click();
  await expect(
    page
      .locator(".library")
      .getByRole("button", { name: "SQL helper", exact: true }),
  ).toBeVisible();
  await queueReply(page, {
    method: "load_graph",
    args: { name: "Demo" },
    value: graph(data),
  });
  await page
    .locator(".library")
    .getByRole("button", { name: "Demo", exact: true })
    .click();
  await page.locator('[data-id="n0"]').click();
  await page.getByRole("tab", { name: "Node inspector", exact: true }).click();
}
async function pending(page: Page, gate: string) {
  await expect
    .poll(() =>
      page.evaluate(
        (gate) => Boolean((window as any).__testState.pending[gate]),
        gate,
      ),
    )
    .toBe(true);
}

test("an Inspector prompt response cannot overwrite newer instructions", async ({
  page,
}) => {
  await openInspector(page, { ...agent, type: "agent" });
  await page.getByText("Saved prompts", { exact: true }).click();
  await queueReply(page, {
    method: "load_prompt",
    gate: "old-instructions",
    value: "Stale saved prompt",
  });
  await page
    .getByLabel("Load prompt", { exact: true })
    .selectOption("First prompt");
  await pending(page, "old-instructions");
  await page
    .getByLabel("System prompt", { exact: true })
    .fill("My newer instructions");
  await releaseReply(page, "old-instructions");
  await expect(page.getByLabel("System prompt", { exact: true })).toHaveValue(
    "My newer instructions",
  );
  await expect(page.getByLabel("Prompt name", { exact: true })).toHaveValue("");
});

test("the latest Inspector prompt choice wins even when the first response arrives first", async ({
  page,
}) => {
  await openInspector(page, { ...agent, type: "agent" });
  await page.getByText("Saved prompts", { exact: true }).click();
  await queueReply(page, {
    method: "load_prompt",
    args: { name: "First prompt" },
    gate: "first-choice",
    value: "Old choice",
  });
  await queueReply(page, {
    method: "load_prompt",
    args: { name: "Second prompt" },
    gate: "second-choice",
    value: "Latest choice",
  });
  await page
    .getByLabel("Load prompt", { exact: true })
    .selectOption("First prompt");
  await pending(page, "first-choice");
  await page
    .getByLabel("Load prompt", { exact: true })
    .selectOption("Second prompt");
  await pending(page, "second-choice");
  await releaseReply(page, "first-choice");
  await expect(page.getByLabel("System prompt", { exact: true })).toHaveValue(
    "Original instructions",
  );
  await releaseReply(page, "second-choice");
  await expect(page.getByLabel("System prompt", { exact: true })).toHaveValue(
    "Latest choice",
  );
});

test("generated SQL cannot replace a query edited while generation is pending", async ({
  page,
}) => {
  await openInspector(page, database);
  await page.getByText("Schema & SQL assistant", { exact: true }).click();
  await page
    .getByLabel("What should the query do?", { exact: true })
    .fill("Count records");
  await queueReply(page, {
    method: "write_sql",
    gate: "old-query",
    value: "SELECT count(*) FROM old_table",
  });
  await page.getByRole("button", { name: "Write query", exact: true }).click();
  await pending(page, "old-query");
  await page.getByLabel("Query", { exact: true }).fill("SELECT my_newer_query");
  await releaseReply(page, "old-query");
  await expect(page.getByLabel("Query", { exact: true })).toHaveValue(
    "SELECT my_newer_query",
  );
});

test("schema results and failures stay attached to the inspected database configuration", async ({
  page,
}) => {
  await openInspector(page, database);
  await page.getByText("Schema & SQL assistant", { exact: true }).click();
  await queueReply(page, {
    method: "describe_schema",
    gate: "old-schema",
    value: "OLD_DATABASE_TABLE",
  });
  await page
    .getByRole("button", { name: "Inspect schema", exact: true })
    .click();
  await pending(page, "old-schema");
  await page.getByLabel("Database file", { exact: true }).fill("new.sqlite");
  await releaseReply(page, "old-schema");
  await expect(
    page.getByText("OLD_DATABASE_TABLE", { exact: true }),
  ).toHaveCount(0);
  await queueReply(page, {
    method: "describe_schema",
    gate: "obsolete-schema-error",
    error: "Old schema connection failed",
  });
  await page
    .getByRole("button", { name: "Inspect schema", exact: true })
    .click();
  await pending(page, "obsolete-schema-error");
  await page
    .locator(".library")
    .getByRole("button", { name: "Extract", exact: true })
    .click();
  await page
    .getByRole("dialog", { name: "Unsaved changes" })
    .getByRole("button", { name: "Discard changes", exact: true })
    .click();
  await expect(page.getByLabel("Name", { exact: true })).toHaveValue(
    "Original source",
  );
  await releaseReply(page, "obsolete-schema-error");
  await expect(
    page.getByText("Old schema connection failed", { exact: true }),
  ).toHaveCount(0);
});

test("reopening a graph with the same node ID resets Inspector drafts and rejects old prompt responses", async ({
  page,
}) => {
  await openInspector(page, { ...agent, type: "agent" });
  await page.getByText("Saved prompts", { exact: true }).click();
  await page
    .getByLabel("Prompt name", { exact: true })
    .fill("Unsaved prompt name");
  await queueReply(page, {
    method: "load_prompt",
    gate: "old-document-prompt",
    value: "Old document instructions",
  });
  await page
    .getByLabel("Load prompt", { exact: true })
    .selectOption("First prompt");
  await pending(page, "old-document-prompt");
  await queueReply(page, {
    method: "load_graph",
    args: { name: "Demo" },
    value: graph({ ...agent, type: "agent", prompt: "Reopened instructions" }),
  });
  await page
    .locator(".library")
    .getByRole("button", { name: "Demo", exact: true })
    .click();
  await page.locator('[data-id="n0"]').click();
  const prompts = page
    .locator(".ins-details")
    .filter({ has: page.getByText("Saved prompts", { exact: true }) });
  if ((await prompts.getAttribute("open")) === null)
    await page.getByText("Saved prompts", { exact: true }).click();
  await expect(page.getByLabel("Prompt name", { exact: true })).toHaveValue("");
  await releaseReply(page, "old-document-prompt");
  await expect(page.getByLabel("System prompt", { exact: true })).toHaveValue(
    "Reopened instructions",
  );
});
