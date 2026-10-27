import type { Page } from "@playwright/test";
import {
  test,
  expect,
  queueReply,
  callsFor,
  parent,
} from "./support/workspace";

async function rowAction(page: Page, name: string, action: string) {
  const row = page.locator(".library-row").filter({ hasText: name }).first();
  await row.hover();
  await row.getByLabel(action, { exact: true }).click();
}

test("workflows can be renamed, duplicated and deleted from the library", async ({
  page,
}) => {
  await queueReply(page, { method: "rename_graph", value: { name: "Parts2" } });
  await rowAction(page, "Parts", "Rename Parts");
  const rename = page.getByRole("dialog", { name: "Rename workflow" });
  await expect(rename.getByLabel("New name")).toHaveValue("Parts");
  await rename.getByLabel("New name").fill("Parts2");
  await page.evaluate(() => {
    const state = (window as any).__testState;
    state.graphs.Parts2 = state.graphs.Parts;
    delete state.graphs.Parts;
  });
  await rename.getByRole("button", { name: "Rename", exact: true }).click();
  expect((await callsFor(page, "rename_graph"))[0]?.args).toEqual({
    from: "Parts",
    to: "Parts2",
  });
  await expect(
    page
      .locator(".library")
      .getByRole("button", { name: "Parts2", exact: true }),
  ).toBeVisible();
  await expect(page.getByRole("status")).toHaveText("Renamed Parts to Parts2");

  await queueReply(page, {
    method: "duplicate_graph",
    value: { name: "Extract-copy" },
  });
  await rowAction(page, "Extract", "Duplicate Extract");
  const duplicate = page.getByRole("dialog", { name: "Duplicate workflow" });
  await expect(duplicate.getByLabel("Copy name")).toHaveValue("Extract-copy");
  await page.evaluate(() => {
    const state = (window as any).__testState;
    state.graphs["Extract-copy"] = state.graphs.Extract;
  });
  await duplicate.getByLabel("Copy name").press("Enter");
  expect((await callsFor(page, "duplicate_graph"))[0]?.args).toEqual({
    from: "Extract",
    to: "Extract-copy",
  });
  await expect(
    page
      .locator(".library")
      .getByRole("button", { name: "Extract-copy", exact: true }),
  ).toBeVisible();

  await queueReply(page, { method: "delete_graph", value: null });
  await rowAction(page, "Demo", "Delete Demo");
  const remove = page.getByRole("dialog", { name: "Delete workflow" });
  await expect(remove).toContainText("Delete the workflow Demo");
  await page.evaluate(() => {
    delete (window as any).__testState.graphs.Demo;
  });
  await remove.getByRole("button", { name: "Delete", exact: true }).click();
  await expect(
    page.locator(".library").getByRole("button", { name: "Demo", exact: true }),
  ).toHaveCount(0);
  await expect(page.getByText("Unsaved changes")).toBeVisible();
  await expect(page.getByRole("status")).toContainText("Deleted Demo");
});

test("the empty state installs the bundled examples and opens the first one", async ({
  page,
}) => {
  await page.getByRole("button", { name: "New", exact: true }).click();
  await page.evaluate((parent) => {
    (window as any).__testState.graphs["reviewed-note"] = {
      ...parent,
      name: "reviewed-note",
    };
  }, parent);
  await queueReply(page, {
    method: "install_examples",
    value: { installed: ["reviewed-note"], skipped: [] },
  });
  await page
    .locator(".empty-canvas")
    .getByRole("button", { name: "Install example workflows", exact: true })
    .click();
  await expect(page.getByLabel("Workflow name", { exact: true })).toHaveValue(
    "reviewed-note",
  );
  await expect(
    page
      .locator(".library")
      .getByRole("button", { name: "reviewed-note", exact: true }),
  ).toBeVisible();
});

test("run feed entries select their node on the canvas and outputs can be copied", async ({
  page,
}) => {
  await page.context().grantPermissions(["clipboard-read", "clipboard-write"]);
  await page.getByRole("button", { name: "Run workflow", exact: true }).click();
  await page.getByRole("button", { name: "Start run", exact: true }).click();
  await page
    .getByRole("button", { name: "Apply changes", exact: true })
    .click();
  await expect(page.getByRole("status").first()).toHaveText(
    "Completed in 0.1s",
  );
  const feed = page.getByLabel("Run output feed");
  await feed.getByRole("button", { name: "Create parts", exact: true }).click();
  await expect(page.locator('[data-id="n1"]')).toHaveClass(/selected/);
  await expect(page.getByRole("tab", { name: "Run output" })).toHaveAttribute(
    "aria-selected",
    "true",
  );
  await feed.getByRole("button", { name: "Copy", exact: true }).first().click();
  await expect(feed.getByText("Copied", { exact: true })).toBeVisible();
  expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(
    "Example",
  );
});

test("agents can list provider models and see whether a key variable is set", async ({
  page,
}) => {
  await page
    .locator(".library")
    .getByRole("button", { name: "Agent", exact: true })
    .click();
  await queueReply(page, {
    method: "list_models",
    value: ["llama3.1", "mistral-small"],
  });
  await page.getByRole("button", { name: "Fetch models", exact: true }).click();
  await expect(page.locator(".inspector").getByRole("status")).toHaveText(
    "Models listed",
  );
  // Every listed id is offered even though the field already holds a full one.
  const model = page.getByLabel("Model", { exact: true });
  await expect(model).toHaveValue("llama3.1");
  await model.click();
  await expect(
    page.locator(".inspector").getByRole("listbox").getByRole("option"),
  ).toHaveText(["llama3.1", "mistral-small"]);
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("Enter");
  await expect(model).toHaveValue("mistral-small");
  await expect(page.locator(".inspector").getByRole("listbox")).toHaveCount(0);
  await model.fill("mis");
  await expect(
    page.locator(".inspector").getByRole("listbox").getByRole("option"),
  ).toHaveText(["mistral-small"]);
  await page.keyboard.press("Escape");
  await expect(
    page.getByText("Not among the models Ollama listed."),
  ).toBeVisible();
  await model.fill("mistral-small");
  await expect(page.getByText("2 models from Ollama")).toBeVisible();
  await queueReply(page, { method: "env_var_set", value: false });
  await page.getByLabel("Provider", { exact: true }).selectOption("openai");
  await expect(
    page.getByText("OPENAI_API_KEY is not set in uapz's environment", {
      exact: false,
    }),
  ).toBeVisible();
  await queueReply(page, { method: "env_var_set", value: true });
  await page
    .getByLabel("API key environment variable", { exact: true })
    .fill("MY_KEY");
  await expect(
    page.getByText("MY_KEY is set in uapz's environment.", { exact: true }),
  ).toBeVisible();
});
