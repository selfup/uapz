import type { Page } from "@playwright/test";
import {
  test,
  expect,
  queueReply,
  releaseReply,
  callsFor,
} from "./support/workspace";

/** Start the mocked run and wait at its review, the last live state it offers. */
async function startRun(page: Page) {
  await page.getByRole("button", { name: "Run workflow", exact: true }).click();
  await page.getByRole("button", { name: "Start run", exact: true }).click();
  const review = page.getByRole("region", { name: "Review file changes" });
  await expect(review).toBeVisible();
  return review;
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

test("a run whose worker stopped without a record is reported as failed and releases the editor", async ({
  page,
}) => {
  await startRun(page);
  await expect(page.getByRole("status")).toHaveText("n2 · Waiting for review");
  // The service's terminal state after a worker panic: not running, no
  // record, only the error. The renderer must treat it as the run's end.
  await page.evaluate(() => {
    const state = (window as any).__testState;
    state.runtime = {
      ...state.runtime,
      running: false,
      cancelling: false,
      review: null,
      stream: null,
      run: null,
      error: "the execution worker stopped unexpectedly",
    };
  });
  // The error reaches both the banner and the run panel.
  await expect(page.locator(".error-banner")).toContainText(
    "the execution worker stopped unexpectedly",
  );
  await expect(page.locator(".run-output-error")).toContainText(
    "the execution worker stopped unexpectedly",
  );
  await expect(page.getByRole("status")).toHaveText("Run failed");
  await expect(
    page.getByRole("region", { name: "Review file changes" }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: "Cancel run", exact: true }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: "Run workflow", exact: true }),
  ).toBeEnabled();
  await expect(
    page.getByRole("button", { name: "New", exact: true }),
  ).toBeEnabled();
  await expect(page.locator(".run-status")).toHaveText("Failed");
});

test("a review decision is sent once however often its button is pressed while it is in flight", async ({
  page,
}) => {
  const review = await startRun(page);
  await queueReply(page, { method: "review", gate: "deciding" });
  const apply = review.getByRole("button", {
    name: "Apply changes",
    exact: true,
  });
  await apply.dblclick();
  await pending(page, "deciding");
  await expect(apply).toBeDisabled();
  await expect(
    review.getByRole("button", { name: "Reject", exact: true }),
  ).toBeDisabled();
  // Cancelling stays possible while the decision is on its way.
  await expect(
    review.getByRole("button", { name: "Cancel run", exact: true }),
  ).toBeEnabled();
  expect(await callsFor(page, "review")).toHaveLength(1);
  await releaseReply(page, "deciding");
  await expect(review).toHaveCount(0);
  await expect(page.getByRole("status")).toHaveText("Completed in 0.1s");
  await expect(page.getByRole("alert")).toHaveCount(0);
  expect(await callsFor(page, "review")).toHaveLength(1);
});

test("a saved, idle workflow quits at once", async ({ page }) => {
  await page.evaluate(() =>
    (window as any).__testState.emit("close-requested"),
  );
  await expect
    .poll(() => page.evaluate(() => (window as any).__testState.quits))
    .toBe(1);
  await expect(page.getByRole("dialog")).toHaveCount(0);
});

test("quitting while a run is in progress asks first, even with nothing unsaved", async ({
  page,
}) => {
  const review = await startRun(page);
  await expect(page.locator(".save-state")).toHaveText("Saved workflow");
  const quits = () =>
    page.evaluate(() => (window as any).__testState.quits as number);
  await page.evaluate(() =>
    (window as any).__testState.emit("close-requested"),
  );
  const dialog = page.getByRole("dialog", { name: "Quit uapz?" });
  await expect(dialog).toContainText(
    "A run is in progress. Quitting cancels it.",
  );
  await expect(dialog.getByRole("button")).toHaveText([
    "Cancel",
    "Quit and cancel the run",
  ]);
  // A second close request while the question is open changes nothing.
  await page.evaluate(() =>
    (window as any).__testState.emit("close-requested"),
  );
  await expect(page.getByRole("dialog")).toHaveCount(1);
  await dialog.getByRole("button", { name: "Cancel", exact: true }).click();
  await expect(dialog).toHaveCount(0);
  expect(await quits()).toBe(0);
  await expect(review).toBeVisible();
  await page.evaluate(() =>
    (window as any).__testState.emit("close-requested"),
  );
  await dialog
    .getByRole("button", { name: "Quit and cancel the run", exact: true })
    .click();
  await expect.poll(quits).toBe(1);
});

test("renaming the open workflow re-keys its remembered runs so outputs are requested under the new name", async ({
  page,
}) => {
  await page.evaluate(() => {
    (window as any).__testState.partialResults = true;
  });
  const review = await startRun(page);
  await review
    .getByRole("button", { name: "Apply changes", exact: true })
    .click();
  await expect(page.getByRole("status")).toHaveText("Completed in 0.1s");
  // The service moves the records and lists them under the new name.
  await queueReply(page, { method: "rename_graph", value: { name: "Demo2" } });
  await page.evaluate(() => {
    const state = (window as any).__testState;
    state.graphs.Demo2 = state.graphs.Demo;
    delete state.graphs.Demo;
    for (const run of state.history) run.graph = "Demo2";
  });
  const row = page.locator(".library-row").filter({ hasText: "Demo" }).first();
  await row.hover();
  await row.getByLabel("Rename Demo", { exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "Rename workflow" });
  await dialog.getByLabel("New name").fill("Demo2");
  await dialog.getByRole("button", { name: "Rename", exact: true }).click();
  await expect(page.getByLabel("Workflow name", { exact: true })).toHaveValue(
    "Demo2",
  );
  // The remembered result for n0 came from the live record under the old
  // name; loading its full output must ask under the name the run now has.
  // The run panel fetched partial outputs on completion, under the name the
  // run had then; only the request made after the rename is under test.
  const before = (await callsFor(page, "output")).length;
  await page.locator('[data-id="n0"]').click();
  await page.getByRole("tab", { name: /^results$/i }).click();
  await page
    .getByRole("button", { name: "View full output", exact: true })
    .click();
  await expect
    .poll(async () => (await callsFor(page, "output")).length)
    .toBe(before + 1);
  expect((await callsFor(page, "output")).at(-1)?.args).toMatchObject({
    graph: "Demo2",
    run_id: "20261025T120000000Z",
    node: "n0",
  });
});
