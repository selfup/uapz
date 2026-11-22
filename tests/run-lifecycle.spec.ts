import type { Page } from "@playwright/test";
import {
  test,
  expect,
  parent,
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

test("launching with an active service session loads its graph as dirty and attaches", async ({
  page,
}) => {
  await page.addInitScript((doc) => {
    window.localStorage.setItem(
      "uapz.layout",
      JSON.stringify({
        lastGraph: { workspace: "/test/workspace", name: "Parts" },
      }),
    );
    (window as any).__testState.runtime = {
      run_id: "live-1",
      running: true,
      cancelling: false,
      graph: structuredClone(doc),
      targets: ["n2"],
      marks: { n0: "ran", n1: "ran", n2: "running" },
      log: ["Resumed session"],
      stream: null,
      workflows: {},
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
  }, parent);
  await page.reload();
  await expect(page.getByLabel("Workflow name", { exact: true })).toHaveValue(
    "Demo",
  );
  await expect(page.locator(".save-state")).toHaveText("Unsaved changes");
  await expect(
    page.getByRole("tab", { name: "Run output", exact: true }),
  ).toHaveAttribute("aria-selected", "true");
  const review = page.getByRole("region", { name: "Review file changes" });
  await expect(review).toBeVisible();
  await review
    .getByRole("button", { name: "Apply changes", exact: true })
    .click();
  await expect(page.getByRole("status")).toHaveText("Completed in 0.1s");
});

test("polling attaches to an unowned active run for the open graph", async ({
  page,
}) => {
  // Open the Run inputs form so orphan attach must dismiss it and switch to Run output.
  await page.getByRole("button", { name: "Run workflow", exact: true }).click();
  await expect(
    page.getByRole("region", { name: "Run inputs" }),
  ).toBeVisible();
  await page.evaluate((doc) => {
    (window as any).__testState.runtime = {
      run_id: "live-1",
      running: true,
      cancelling: false,
      graph: structuredClone(doc),
      targets: ["n2"],
      marks: { n0: "ran", n1: "ran", n2: "running" },
      log: ["Orphan run started"],
      stream: null,
      workflows: {},
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
  }, parent);
  const review = page.getByRole("region", { name: "Review file changes" });
  await expect(review).toBeVisible();
  await expect(
    page.getByRole("region", { name: "Run inputs" }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("tab", { name: "Run output", exact: true }),
  ).toHaveAttribute("aria-selected", "true");
  await review
    .getByRole("button", { name: "Apply changes", exact: true })
    .click();
  await expect(page.getByRole("status")).toHaveText("Completed in 0.1s");
});

test("cancelling a running workflow shows Cancelling state until the worker finishes", async ({
  page,
}) => {
  const review = await startRun(page);
  // Keep the worker running in the cancelling state after cancel_run is accepted.
  await queueReply(page, { method: "cancel_run", value: null });
  await page.evaluate(() => {
    const state = (window as any).__testState;
    state.runtime = { ...state.runtime, cancelling: true };
  });
  await page
    .getByRole("banner")
    .getByRole("button", { name: "Cancel run", exact: true })
    .click();
  await expect(
    page
      .getByRole("banner")
      .getByRole("button", { name: "Cancelling…", exact: true }),
  ).toBeDisabled();
  await expect(
    review.getByRole("button", { name: "Apply changes", exact: true }),
  ).toBeDisabled();
  await expect(
    review.getByRole("button", { name: "Reject", exact: true }),
  ).toBeDisabled();
  await expect(
    review.getByRole("button", { name: "Cancel run", exact: true }),
  ).toBeDisabled();
  // Once the pending review clears while still cancelling, the run panel badge shows Cancelling.
  await page.evaluate(() => {
    const state = (window as any).__testState;
    state.runtime = { ...state.runtime, review: null };
  });
  await expect(page.locator(".run-status")).toHaveText("Cancelling");

  await page.evaluate(() => {
    const state = (window as any).__testState;
    state.runtime = {
      ...state.runtime,
      running: false,
      cancelling: false,
      run: {
        id: "cancelled-1",
        graph: "Demo",
        targets: ["n2"],
        elapsed_ms: 15,
        inputs: {},
        nodes: {},
        log: [],
        outcome: { kind: "cancelled" },
      },
    };
  });
  await expect(page.getByRole("status")).toHaveText("Run cancelled");
  await expect(page.locator(".run-status")).toHaveText("Cancelled");
  await expect(
    page.getByRole("button", { name: "Run workflow", exact: true }),
  ).toBeEnabled();
});

test("workflows without input fields start immediately and return to idle on start failure", async ({
  page,
}) => {
  await queueReply(page, {
    method: "prepare_run",
    value: { targets: ["n2"], fields: [], inputs: {} },
  });
  await queueReply(page, {
    method: "start_run",
    error: "Service refused immediate start",
  });
  await page.getByRole("button", { name: "Run workflow", exact: true }).click();
  await expect(page.locator(".error-banner")).toContainText(
    "Service refused immediate start",
  );
  await expect(
    page.getByRole("region", { name: "Run inputs" }),
  ).toHaveCount(0);
  expect(await callsFor(page, "start_run")).toHaveLength(1);
  await expect(
    page.getByRole("button", { name: "Run workflow", exact: true }),
  ).toBeEnabled();
});

test("save stays available during a run while navigation and destructive actions wait", async ({
  page,
}) => {
  await startRun(page);
  await expect(
    page.getByRole("button", { name: "New", exact: true }),
  ).toBeDisabled();
  await expect(
    page
      .locator(".library")
      .getByRole("button", { name: "Parts", exact: true }),
  ).toBeDisabled();
  const demoRow = page
    .locator(".library-row")
    .filter({ hasText: "Demo" })
    .first();
  await demoRow.hover();
  await expect(
    demoRow.getByLabel("Rename Demo", { exact: true }),
  ).toBeDisabled();
  await expect(
    demoRow.getByLabel("Duplicate Demo", { exact: true }),
  ).toBeDisabled();
  await expect(
    demoRow.getByLabel("Delete Demo", { exact: true }),
  ).toBeDisabled();

  await page.getByRole("tab", { name: "Node inspector", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "Clear cache", exact: true }),
  ).toBeDisabled();
  await expect(
    page.getByRole("button", { name: "Clear history", exact: true }),
  ).toBeDisabled();

  const save = page.getByRole("button", { name: "Save", exact: true });
  await expect(save).toBeEnabled();
  await save.click();
  await expect
    .poll(async () => (await callsFor(page, "save_graph")).length)
    .toBe(1);
});

test("a finished run with a save error surfaces the error even while inspecting history", async ({
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
            preview: "Older output",
            label: "Text",
            complete: true,
          },
        },
        log: [],
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
  const review = await startRun(page);
  await queueReply(page, { method: "runtime_state", gate: "save-err-poll" });
  await pending(page, "save-err-poll");
  await review
    .getByRole("button", { name: "Apply changes", exact: true })
    .click();
  await expect(review).toHaveCount(0);
  await page.evaluate(() => {
    const state = (window as any).__testState;
    state.runtime = {
      ...state.runtime,
      error: "The run record was not saved: disk full",
    };
  });
  await page.getByRole("tab", { name: "Node inspector", exact: true }).click();
  await page.getByRole("tab", { name: "History", exact: true }).click();
  await page.locator(".ins-run > summary").last().click();
  await page
    .getByRole("button", { name: "Inspect this run", exact: true })
    .last()
    .click();
  await expect(page.getByRole("status")).toHaveText("Showing run older-run");

  await releaseReply(page, "save-err-poll");
  await expect(page.locator(".error-banner")).toContainText(
    "The run record was not saved: disk full",
  );
  await expect(page.getByRole("status")).toHaveText("Showing run older-run");
});

test("quitting handles unsaved idle, unsaved active, read-only child, and save failures", async ({
  page,
}) => {
  const quits = () =>
    page.evaluate(() => (window as any).__testState.quits as number);

  // Dirty + Idle: Save and quit failure stays open; Save and quit success saves then quits.
  await page.locator('[data-id="n0"]').click();
  await page.getByLabel("Name", { exact: true }).fill("editedForQuit");
  await expect(page.locator(".save-state")).toHaveText("Unsaved changes");

  await queueReply(page, { method: "save_graph", error: "Read-only disk" });
  await page.evaluate(() =>
    (window as any).__testState.emit("close-requested"),
  );
  const dialog = page.getByRole("dialog", { name: "Quit uapz?" });
  await expect(dialog).toContainText("This workflow has unsaved changes.");
  await expect(dialog.getByRole("button")).toHaveText([
    "Cancel",
    "Quit without saving",
    "Save and quit",
  ]);
  await dialog
    .getByRole("button", { name: "Save and quit", exact: true })
    .click();
  await expect(page.locator(".error-banner")).toContainText("Read-only disk");
  expect(await quits()).toBe(0);

  await page.evaluate(() =>
    (window as any).__testState.emit("close-requested"),
  );
  await dialog
    .getByRole("button", { name: "Save and quit", exact: true })
    .click();
  await expect.poll(quits).toBe(1);
  expect(await callsFor(page, "save_graph")).toHaveLength(2);

  // Dirty + Idle: Quit without saving does not save.
  await page.getByLabel("Name", { exact: true }).fill("discardForQuit");
  await page.evaluate(() =>
    (window as any).__testState.emit("close-requested"),
  );
  await dialog
    .getByRole("button", { name: "Quit without saving", exact: true })
    .click();
  await expect.poll(quits).toBe(2);
  expect(await callsFor(page, "save_graph")).toHaveLength(2);

  // Dirty + Running: combined message with Save and quit, omitted in read-only child.
  await startRun(page);
  await page.evaluate(() =>
    (window as any).__testState.emit("close-requested"),
  );
  await expect(dialog).toContainText(
    "This workflow has unsaved changes, and a run is in progress. Quitting cancels the run.",
  );
  await expect(dialog.getByRole("button")).toHaveText([
    "Cancel",
    "Quit without saving",
    "Save and quit",
  ]);
  await dialog.getByRole("button", { name: "Cancel", exact: true }).click();

  await page.locator('[data-id="n1"]').dblclick();
  await expect(page.locator(".provenance")).toContainText("Read only");
  await page.evaluate(() =>
    (window as any).__testState.emit("close-requested"),
  );
  await expect(dialog.getByRole("button")).toHaveText([
    "Cancel",
    "Quit without saving",
  ]);
  await dialog
    .getByRole("button", { name: "Quit without saving", exact: true })
    .click();
  await expect.poll(quits).toBe(3);
});

test("quitting while an accepted run is still pending asks before cancelling", async ({
  page,
}) => {
  await page.getByRole("button", { name: "Run workflow", exact: true }).click();
  await queueReply(page, { method: "start_run", gate: "pending-start" });
  await page.getByRole("button", { name: "Start run", exact: true }).click();
  await pending(page, "pending-start");
  await queueReply(page, { method: "runtime_state", gate: "first-status" });
  await releaseReply(page, "pending-start");
  await pending(page, "first-status");

  await page.evaluate(() =>
    (window as any).__testState.emit("close-requested"),
  );
  const dialog = page.getByRole("dialog", { name: "Quit uapz?" });
  await expect(dialog).toContainText(
    "A run is in progress. Quitting cancels it.",
  );
  expect(await page.evaluate(() => (window as any).__testState.quits)).toBe(0);
  await dialog.getByRole("button", { name: "Cancel", exact: true }).click();
  await releaseReply(page, "first-status");
  await expect(
    page.getByRole("region", { name: "Review file changes" }),
  ).toBeVisible();
});

test("an in-flight install_examples completion does not replace an open quit dialog", async ({
  page,
}) => {
  await page.getByRole("button", { name: "New", exact: true }).click();
  await page
    .getByLabel("Workflow name", { exact: true })
    .fill("unsaved-empty");
  await page.evaluate((doc) => {
    (window as any).__testState.graphs["reviewed-note"] = {
      ...doc,
      name: "reviewed-note",
    };
  }, parent);
  await queueReply(page, {
    method: "install_examples",
    gate: "installing",
    value: { installed: ["reviewed-note"], skipped: [] },
  });
  await page
    .locator(".empty-canvas")
    .getByRole("button", { name: "Install example workflows", exact: true })
    .click();
  await pending(page, "installing");

  await page.evaluate(() =>
    (window as any).__testState.emit("close-requested"),
  );
  const dialog = page.getByRole("dialog", { name: "Quit uapz?" });
  await expect(dialog).toBeVisible();

  await releaseReply(page, "installing");
  // The quit dialog stays intact and the completed install does not navigate away.
  await expect(dialog).toBeVisible();
  await expect(page.getByLabel("Workflow name", { exact: true })).toHaveValue(
    "unsaved-empty",
  );
  await dialog.getByRole("button", { name: "Cancel", exact: true }).click();
  await expect(dialog).toHaveCount(0);
});
