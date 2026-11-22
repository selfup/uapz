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

test("clearing history waits while occupied and exits historical view cleanly", async ({
  page,
}) => {
  await page.evaluate(() => {
    (window as any).__testState.history = [
      {
        id: "older-run",
        graph: "Demo",
        targets: ["n2"],
        elapsed_ms: 20,
        nodes: {
          n0: {
            state: "ran",
            preview: "Earlier output",
            label: "Text",
            complete: true,
          },
        },
        log: ["Earlier execution"],
        outcome: { kind: "ok" },
      },
    ];
  });
  for (const name of ["Parts", "Demo"]) {
    await page
      .locator(".library")
      .getByRole("button", { name, exact: true })
      .click();
    await expect(page.getByLabel("Workflow name", { exact: true })).toHaveValue(
      name,
    );
  }

  await page.getByRole("tab", { name: "History", exact: true }).click();
  await page.locator(".ins-run > summary").click();
  await page
    .getByRole("button", { name: "Inspect this run", exact: true })
    .click();
  await expect(page.getByRole("status")).toHaveText("Showing run older-run");

  // Opening the run inputs form makes the editor occupied; Clear cache and Clear history stay disabled.
  await page.getByRole("button", { name: "Run workflow", exact: true }).click();
  await expect(
    page.getByRole("region", { name: "Run inputs" }),
  ).toBeVisible();
  await page.getByRole("tab", { name: "Node inspector", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "Clear cache", exact: true }),
  ).toBeDisabled();
  await expect(
    page.getByRole("button", { name: "Clear history", exact: true }),
  ).toBeDisabled();
  await page
    .getByRole("button", { name: "Close run inputs", exact: true })
    .click();

  await page
    .getByRole("button", { name: "Clear history", exact: true })
    .click();
  const dialog = page.getByRole("dialog", { name: "Clear history" });
  await dialog.getByRole("button", { name: "Clear", exact: true }).click();
  await expect(page.locator(".ins-run")).toHaveCount(0);
  await expect(page.getByRole("status")).toHaveText("History cleared");
  await page.getByRole("tab", { name: "Run output", exact: true }).click();
  await expect(page.getByText("No run yet", { exact: true })).toBeVisible();

  // Starting and completing a run after clearing history announces completion cleanly.
  const review = await startRun(page);
  await review
    .getByRole("button", { name: "Apply changes", exact: true })
    .click();
  await expect(page.getByRole("status")).toHaveText("Completed in 0.1s");
});

test("deleting the open workflow clears its history and remembered outputs", async ({
  page,
}) => {
  const review = await startRun(page);
  await review
    .getByRole("button", { name: "Apply changes", exact: true })
    .click();
  await expect(page.getByRole("status")).toHaveText("Completed in 0.1s");

  await page.locator('[data-id="n0"]').click();
  await page.getByRole("tab", { name: "Node inspector", exact: true }).click();
  await page.getByRole("tab", { name: "Results", exact: true }).click();
  await expect(page.locator(".ins-result .ins-output")).toContainText(
    "Example",
  );
  await page.getByRole("tab", { name: "History", exact: true }).click();
  await expect(page.locator(".ins-run")).toHaveCount(1);

  await queueReply(page, { method: "delete_graph", value: null });
  const row = page.locator(".library-row").filter({ hasText: "Demo" }).first();
  await row.hover();
  await row.getByLabel("Delete Demo", { exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "Delete workflow" });
  await page.evaluate(() => {
    const state = (window as any).__testState;
    delete state.graphs.Demo;
    state.history = [];
  });
  await dialog.getByRole("button", { name: "Delete", exact: true }).click();
  await expect(page.getByText("Unsaved changes")).toBeVisible();
  await expect(page.locator(".ins-run")).toHaveCount(0);
  await page.getByRole("tab", { name: "Results", exact: true }).click();
  await expect(
    page.getByText("No result for this node yet", { exact: true }),
  ).toBeVisible();
});

test("quitting while start_run is in flight asks before cancelling", async ({
  page,
}) => {
  await page.getByRole("button", { name: "Run workflow", exact: true }).click();
  await queueReply(page, { method: "start_run", gate: "starting" });
  await page.getByRole("button", { name: "Start run", exact: true }).click();
  await pending(page, "starting");
  await expect(page.locator(".save-state")).toHaveText("Saved workflow");

  await page.evaluate(() =>
    (window as any).__testState.emit("close-requested"),
  );
  const dialog = page.getByRole("dialog", { name: "Quit uapz?" });
  await expect(dialog).toContainText(
    "A run is in progress. Quitting cancels it.",
  );
  expect(await page.evaluate(() => (window as any).__testState.quits)).toBe(0);
  await dialog.getByRole("button", { name: "Cancel", exact: true }).click();
  await releaseReply(page, "starting");
  await expect(
    page.getByRole("region", { name: "Review file changes" }),
  ).toBeVisible();
  expect(await page.evaluate(() => (window as any).__testState.quits)).toBe(0);
});

test("save and quit refuses to quit while another save is already in flight", async ({
  page,
}) => {
  await page.locator('[data-id="n0"]').click();
  await page.getByLabel("Name", { exact: true }).fill("editedBeforeQuit");
  await queueReply(page, { method: "save_graph", gate: "in-flight-save" });
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await pending(page, "in-flight-save");

  await page.evaluate(() =>
    (window as any).__testState.emit("close-requested"),
  );
  const dialog = page.getByRole("dialog", { name: "Quit uapz?" });
  await dialog
    .getByRole("button", { name: "Save and quit", exact: true })
    .click();
  await expect(page.locator(".error-banner")).toContainText(
    "Wait for the current save to finish before quitting.",
  );
  expect(await page.evaluate(() => (window as any).__testState.quits)).toBe(0);
  await releaseReply(page, "in-flight-save");
  await expect(page.locator(".save-state")).toHaveText("Saved workflow");
});
