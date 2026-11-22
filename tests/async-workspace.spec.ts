import {
  test,
  expect,
  queueReply,
  releaseReply,
  callsFor,
} from "./support/workspace";

test("failed saves retain the draft and can be retried", async ({ page }) => {
  await page.locator('[data-id="n0"]').click();
  await page.getByLabel("Name", { exact: true }).fill("draftName");
  await queueReply(page, { method: "save_graph", error: "Disk is full" });
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByRole("alert")).toContainText("Disk is full");
  await expect(page.getByLabel("Name", { exact: true })).toHaveValue(
    "draftName",
  );
  await expect(
    page.getByText("Unsaved changes", { exact: true }),
  ).toBeVisible();
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText("Saved workflow", { exact: true })).toBeVisible();
  await expect
    .poll(async () => (await callsFor(page, "save_graph")).length)
    .toBe(2);
});

test("edits made during a save survive the response and remain unsaved", async ({
  page,
}) => {
  await page.locator('[data-id="n0"]').click();
  await page.getByLabel("Name", { exact: true }).fill("firstDraft");
  await queueReply(page, { method: "save_graph", gate: "saving" });
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect
    .poll(async () => (await callsFor(page, "save_graph")).length)
    .toBe(1);
  await page.getByLabel("Name", { exact: true }).fill("newerDraft");
  await releaseReply(page, "saving");
  await expect(page.getByLabel("Name", { exact: true })).toHaveValue(
    "newerDraft",
  );
  await expect(
    page.getByText("Unsaved changes", { exact: true }),
  ).toBeVisible();
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText("Saved workflow", { exact: true })).toBeVisible();
  const saved = await page.evaluate(() => (window as any).__testState.saved);
  expect(saved.nodes[0].name).toBe("newerDraft");
});

test("a save response from the previous graph cannot replace the open graph", async ({
  page,
}) => {
  await queueReply(page, { method: "save_graph", gate: "old-save" });
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect
    .poll(async () => (await callsFor(page, "save_graph")).length)
    .toBe(1);
  await page
    .locator(".library")
    .getByRole("button", { name: "Parts", exact: true })
    .click();
  await expect(page.locator('[data-id="n4"]')).toBeVisible();
  await releaseReply(page, "old-save");
  await expect(page.locator('[data-id="n4"]')).toBeVisible();
  await expect(page.locator('[data-id="n2"]')).toHaveCount(0);
  await expect(
    page.getByRole("navigation", { name: "Workflow breadcrumbs" }),
  ).toHaveText("Parts");
});

test("rename, delete and duplicate wait for an in-flight save, while open stays available", async ({
  page,
}) => {
  await queueReply(page, { method: "save_graph", gate: "saving-mutates" });
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect
    .poll(async () => (await callsFor(page, "save_graph")).length)
    .toBe(1);
  const demo = page.locator(".library-row").filter({ hasText: "Demo" }).first();
  await demo.hover();
  await expect(demo.getByLabel("Rename Demo", { exact: true })).toBeDisabled();
  await expect(
    demo.getByLabel("Duplicate Demo", { exact: true }),
  ).toBeDisabled();
  await expect(demo.getByLabel("Delete Demo", { exact: true })).toBeDisabled();
  await page
    .locator(".library")
    .getByRole("button", { name: "Parts", exact: true })
    .click();
  await expect(page.locator('[data-id="n4"]')).toBeVisible();
  await releaseReply(page, "saving-mutates");
  const parts = page
    .locator(".library-row")
    .filter({ hasText: "Parts" })
    .first();
  await parts.hover();
  await expect(parts.getByLabel("Rename Parts", { exact: true })).toBeEnabled();
});

test("the latest graph selection wins when loads finish out of order", async ({
  page,
}) => {
  await queueReply(page, {
    method: "load_graph",
    args: { name: "Parts" },
    gate: "slow-load",
  });
  await page
    .locator(".library")
    .getByRole("button", { name: "Parts", exact: true })
    .click();
  await expect
    .poll(() =>
      page.evaluate(() =>
        Boolean((window as any).__testState.pending["slow-load"]),
      ),
    )
    .toBe(true);
  await page
    .locator(".library")
    .getByRole("button", { name: "Extract", exact: true })
    .click();
  await expect(page.locator('[data-id="n0"] .graph-card-title')).toHaveText(
    "Original source",
  );
  await releaseReply(page, "slow-load");
  await expect(page.locator('[data-id="n0"] .graph-card-title')).toHaveText(
    "Original source",
  );
  await expect(
    page.getByRole("navigation", { name: "Workflow breadcrumbs" }),
  ).toHaveText("Extract");
});

test("a pending child load cannot open over a newly selected root graph", async ({
  page,
}) => {
  // Wait for the interface lookup so the gate applies to the user's navigation.
  await expect(
    page.locator('[data-id="n1"] [data-handleid="port:body"]'),
  ).toBeVisible();
  await queueReply(page, {
    method: "load_graph",
    args: { name: "Parts" },
    gate: "child-load",
  });
  await page.locator('[data-id="n1"]').dblclick();
  await expect
    .poll(() =>
      page.evaluate(() =>
        Boolean((window as any).__testState.pending["child-load"]),
      ),
    )
    .toBe(true);
  await page
    .locator(".library")
    .getByRole("button", { name: "Extract", exact: true })
    .click();
  await expect(page.locator('[data-id="n0"] .graph-card-title')).toHaveText(
    "Original source",
  );
  await releaseReply(page, "child-load");
  await expect(
    page.getByRole("navigation", { name: "Workflow breadcrumbs" }),
  ).toHaveText("Extract");
  await expect(
    page.getByRole("button", { name: "Save", exact: true }),
  ).toBeEnabled();
});

test("renaming a dirty open workflow keeps it unsaved while renaming a clean one stays saved", async ({
  page,
}) => {
  await queueReply(page, {
    method: "rename_graph",
    value: { name: "DemoClean" },
  });
  await page.evaluate(() => {
    const state = (window as any).__testState;
    state.graphs.DemoClean = state.graphs.Demo;
    delete state.graphs.Demo;
  });
  const demo = page.locator(".library-row").filter({ hasText: "Demo" }).first();
  await demo.hover();
  await demo.getByLabel("Rename Demo", { exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "Rename workflow" });
  await dialog.getByLabel("New name").fill("DemoClean");
  await dialog.getByRole("button", { name: "Rename", exact: true }).click();
  await expect(page.getByLabel("Workflow name", { exact: true })).toHaveValue(
    "DemoClean",
  );
  await expect(page.locator(".save-state")).toHaveText("Saved workflow");

  // Make the open workflow dirty, then rename again: savedKey must not follow.
  await page.locator('[data-id="n0"]').click();
  await page.getByLabel("Name", { exact: true }).fill("dirtyBeforeRename");
  await expect(page.locator(".save-state")).toHaveText("Unsaved changes");

  await queueReply(page, {
    method: "rename_graph",
    value: { name: "DemoDirty" },
  });
  await page.evaluate(() => {
    const state = (window as any).__testState;
    state.graphs.DemoDirty = state.graphs.DemoClean;
    delete state.graphs.DemoClean;
  });
  const clean = page
    .locator(".library-row")
    .filter({ hasText: "DemoClean" })
    .first();
  await clean.hover();
  await clean.getByLabel("Rename DemoClean", { exact: true }).click();
  await dialog.getByLabel("New name").fill("DemoDirty");
  await dialog.getByRole("button", { name: "Rename", exact: true }).click();
  await expect(page.getByLabel("Workflow name", { exact: true })).toHaveValue(
    "DemoDirty",
  );
  await expect(page.locator(".save-state")).toHaveText("Unsaved changes");
});

test("nested workflow navigation stops at 8 levels", async ({ page }) => {
  await page.evaluate(() => {
    const state = (window as any).__testState;
    state.graphs.Loop = {
      version: 1,
      name: "Loop",
      view: { pan_x: 30, pan_y: 30, zoom: 1 },
      nodes: [
        {
          id: "n1",
          name: "Step",
          x: 80,
          y: 80,
          data: { type: "workflow", graph: "Loop", output: "", inputs: {} },
        },
      ],
      edges: [],
      outputs: {},
    };
  });
  await page.getByRole("button", { name: "Refresh library" }).click();
  await page
    .locator(".library")
    .getByRole("button", { name: "Loop", exact: true })
    .click();
  const breadcrumbs = page.getByRole("navigation", {
    name: "Workflow breadcrumbs",
  });
  await expect(breadcrumbs.getByRole("button")).toHaveCount(1);
  for (let depth = 1; depth <= 8; depth += 1) {
    await page.locator('[data-id="n1"]').dblclick();
    await expect(breadcrumbs.getByRole("button")).toHaveCount(depth + 1);
  }
  await page.locator('[data-id="n1"]').dblclick();
  await expect(page.locator(".error-banner")).toContainText(
    "Workflow navigation is limited to 8 levels.",
  );
  await expect(breadcrumbs.getByRole("button")).toHaveCount(9);
});
