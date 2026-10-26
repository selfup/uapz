import type { Run } from "../frontend/types";
import {
  test,
  expect,
  queueReply,
  releaseReply,
  callsFor,
} from "./support/workspace";

test("a delayed completion poll preserves the history run explicitly selected after review", async ({
  page,
}) => {
  const older: Run = {
    id: "older-run",
    graph: "Demo",
    targets: ["n2"],
    elapsed_ms: 20,
    nodes: {
      n0: {
        state: "ran",
        preview: "Previously saved input",
        label: "Text",
        complete: true,
      },
    },
    log: ["Earlier execution"],
    outcome: { kind: "ok" },
  };
  await page.evaluate((older) => {
    (window as any).__testState.history = [older];
  }, older);
  // Open a different graph and return so Demo's history is freshly loaded.
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
  await expect(page.locator(".ins-run")).toHaveCount(1);

  await page.getByRole("button", { name: "Run workflow", exact: true }).click();
  await page.getByRole("button", { name: "Start run", exact: true }).click();
  const review = page.getByRole("region", { name: "Review file changes" });
  await expect(review).toBeVisible();
  await queueReply(page, { method: "runtime_state", gate: "late-completion" });
  await expect
    .poll(() =>
      page.evaluate(() =>
        Boolean((window as any).__testState.pending["late-completion"]),
      ),
    )
    .toBe(true);
  await review
    .getByRole("button", { name: "Apply changes", exact: true })
    .click();
  await expect(review).toHaveCount(0);
  await page.evaluate((older) => {
    (window as any).__testState.history.push(older);
  }, older);

  await page.getByRole("tab", { name: "Node inspector", exact: true }).click();
  await page.getByRole("tab", { name: "History", exact: true }).click();
  await page.locator(".ins-run > summary").click();
  await page
    .getByRole("button", { name: "Inspect this run", exact: true })
    .click();
  const feed = page.getByLabel("Run output feed", { exact: true });
  await expect(feed).toContainText("Previously saved input");
  await expect(page.getByRole("status")).toHaveText("Showing run older-run");

  const historyCalls = (await callsFor(page, "history")).length;
  await releaseReply(page, "late-completion");
  // Completion bookkeeping must still refresh history, even while its viewed
  // output and notice remain associated with the user's explicit selection.
  await expect
    .poll(async () => (await callsFor(page, "history")).length)
    .toBeGreaterThan(historyCalls);
  await expect(feed).toContainText("Previously saved input");
  await expect(feed).not.toContainText("Body text");
  await expect(page.getByRole("status")).toHaveText("Showing run older-run");
  await page.getByRole("tab", { name: "Node inspector", exact: true }).click();
  await page.getByRole("tab", { name: "History", exact: true }).click();
  await expect(page.locator(".ins-run")).toHaveCount(2);
});
