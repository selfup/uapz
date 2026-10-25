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
