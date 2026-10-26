import { test, expect, parent } from "./support/workspace";
import { defaultNodeData } from "../frontend/graph";

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
  await page.getByRole("tab", { name: "Node inspector", exact: true }).click();
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

test("saved preset labels and colors survive editing and reopening typed nodes", async ({
  page,
}, testInfo) => {
  const nodes = [
    {
      id: "n0",
      x: 20,
      y: 60,
      color: "#22c55e",
      data: { type: "data", path: "cities.csv", format: "csv" },
    },
    {
      id: "n1",
      x: 320,
      y: 60,
      kind: "Name Every City",
      color: "#ab67cd",
      data: defaultNodeData("agent"),
    },
    {
      id: "n2",
      x: 320,
      y: 340,
      color: "#f59e0b",
      data: defaultNodeData("script"),
    },
  ];
  await page.evaluate((nodes) => {
    (window as any).__testState.graphs.Demo.nodes = nodes;
  }, nodes);
  await page
    .locator(".library")
    .getByRole("button", { name: "Demo", exact: true })
    .click();
  const card = page.locator('[data-id="n1"] .graph-card');
  await expect(card.locator(".graph-card-title")).toHaveText("Name Every City");
  await expect(card).toHaveCSS("border-left-color", "rgb(171, 103, 205)");
  await expect(page.locator('[data-id="n0"] .graph-card')).toHaveCSS(
    "border-left-color",
    "rgb(34, 197, 94)",
  );
  await expect(page.locator('[data-id="n2"] .graph-card')).toHaveCSS(
    "border-left-color",
    "rgb(245, 158, 11)",
  );
  await card.click();
  await expect(page.locator(".inspector h2")).toHaveText("Name Every City");
  await page
    .getByLabel("System prompt", { exact: true })
    .fill("List each city.");
  await expect(card).toHaveCSS("border-left-color", "rgb(171, 103, 205)");
  await page.screenshot({
    path: testInfo.outputPath("node-labels-and-colors.png"),
    fullPage: true,
  });
  await page.getByLabel("Name", { exact: true }).fill("cityNames");
  await expect(card.locator(".graph-card-title")).toHaveText("cityNames");
  await page.getByRole("button", { name: "Save", exact: true }).click();
  const saved = await page.evaluate(() =>
    (window as any).__testState.saved.nodes.find(
      (node: any) => node.id === "n1",
    ),
  );
  expect(saved).toMatchObject({
    name: "cityNames",
    kind: "Name Every City",
    color: "#ab67cd",
    data: { prompt: "List each city." },
  });
  await page
    .locator(".library")
    .getByRole("button", { name: "Demo", exact: true })
    .click();
  await expect(card.locator(".graph-card-title")).toHaveText("cityNames");
  await expect(card).toHaveCSS("border-left-color", "rgb(171, 103, 205)");
});

test("adding and loading presets retains their labels without assigning template names", async ({
  page,
}) => {
  await page.evaluate(
    ({ agent, database }) => {
      const { type: agentType, ...agentConfig } = agent;
      const { type: databaseType, ...databaseConfig } = database;
      const state = (window as any).__testState;
      state.agents = [
        { name: "Name Every City", config: agentConfig },
        { name: "Summarize", config: agentConfig },
      ];
      state.databases = [{ name: "City Database", config: databaseConfig }];
    },
    { agent: defaultNodeData("agent"), database: defaultNodeData("database") },
  );
  await page.getByRole("button", { name: "Refresh library" }).click();
  await page.getByLabel("Search library").fill("Name Every City");
  await page
    .locator(".library")
    .getByRole("button", { name: "Name Every City", exact: true })
    .click();
  await expect(page.locator('[data-id="n3"] .graph-card-title')).toHaveText(
    "Name Every City",
  );
  await page
    .locator(".inspector summary")
    .filter({ hasText: "Reusable agent preset" })
    .click();
  await page
    .getByLabel("Load preset", { exact: true })
    .selectOption("Summarize");
  await expect(page.locator('[data-id="n3"] .graph-card-title')).toHaveText(
    "Summarize",
  );
  await page.getByLabel("Search library").fill("City Database");
  await page
    .locator(".library")
    .getByRole("button", { name: "City Database", exact: true })
    .click();
  await expect(page.locator('[data-id="n4"] .graph-card-title')).toHaveText(
    "City Database",
  );
  await page.getByRole("button", { name: "Save", exact: true }).click();
  const saved = await page.evaluate(
    () => (window as any).__testState.saved.nodes,
  );
  expect(saved.find((node: any) => node.id === "n3")).toMatchObject({
    kind: "Summarize",
    color: "#8b5cf6",
  });
  expect(saved.find((node: any) => node.id === "n4")).toMatchObject({
    kind: "City Database",
    color: "#0ea5e9",
  });
  expect(saved.find((node: any) => node.id === "n3").name).toBeUndefined();
  expect(saved.find((node: any) => node.id === "n4").name).toBeUndefined();
});
