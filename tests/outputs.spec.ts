import type { Page } from "@playwright/test";
import type { Run } from "../frontend/types";
import {
  test,
  expect,
  queueReply,
  releaseReply,
  callsFor,
} from "./support/workspace";

async function completedRun(page: Page) {
  await page.getByRole("button", { name: "Run workflow", exact: true }).click();
  await page.getByRole("button", { name: "Start run", exact: true }).click();
  await expect(
    page.getByRole("region", { name: "Review file changes" }),
  ).toBeVisible();
  const run: Run = {
    id: "20261025T120000123Z",
    graph: "Demo",
    targets: ["n2"],
    elapsed_ms: 100,
    outcome: { kind: "ok" },
    log: [],
    nodes: {
      n0: {
        state: "ran",
        preview: "Input preview",
        label: "Text",
        complete: false,
      },
      n1: {
        state: "ran",
        preview: "Summary preview",
        label: "Text",
        complete: false,
      },
      n2: { state: "ran", preview: "Saved", label: "Text", complete: true },
    },
  };
  await page.evaluate((run) => {
    const state = (window as any).__testState;
    state.runtime = { ...state.runtime, running: false, review: null, run };
    state.history = [run];
  }, run);
  await expect(page.getByRole("status")).toHaveText("Completed in 0.1s");
  await page.locator('[data-id="n0"]').click();
  await page.getByRole("tab", { name: "Results", exact: true }).click();
  return run;
}

test("full-output failures preserve the preview and allow a retry", async ({
  page,
}) => {
  const run = await completedRun(page);
  await queueReply(page, {
    method: "output",
    error: "Output cache is temporarily unavailable",
  });
  await page.getByRole("button", { name: "View full output" }).click();
  await expect(page.getByRole("alert")).toHaveText(
    "Output cache is temporarily unavailable",
  );
  await expect(page.locator(".ins-output")).toHaveText("Input preview");
  await expect(
    page.getByRole("button", { name: "View full output" }),
  ).toBeEnabled();
  await queueReply(page, {
    method: "output",
    value: {
      label: "Text",
      text: "Complete input contents",
      base64: null,
      saved: null,
      complete: true,
    },
  });
  await page.getByRole("button", { name: "View full output" }).click();
  await expect(page.locator(".ins-output")).toHaveText(
    "Complete input contents",
  );
  await expect(page.getByRole("alert")).toHaveCount(0);
  await expect(
    page.getByText("Preview truncated.", { exact: true }),
  ).toHaveCount(0);
  const calls = await callsFor(page, "output");
  expect(calls).toHaveLength(2);
  expect(calls.map((call) => call.args)).toEqual(
    Array(2).fill({ graph: "Demo", run_id: run.id, node: "n0" }),
  );
});

test("late output responses cannot appear under the next selected node", async ({
  page,
}) => {
  await completedRun(page);
  await queueReply(page, {
    method: "output",
    args: { node: "n0" },
    gate: "old-output",
    value: {
      label: "Text",
      text: "OLD INPUT CONTENTS",
      base64: null,
      saved: null,
      complete: true,
    },
  });
  await page.getByRole("button", { name: "View full output" }).click();
  await expect(
    page.getByRole("button", { name: "Loading…", exact: true }),
  ).toBeDisabled();
  await page.locator('[data-id="n1"]').click();
  await expect(page.locator(".ins-output")).toHaveText("Summary preview");
  await releaseReply(page, "old-output");
  await expect(page.locator(".ins-output")).toHaveText("Summary preview");
  await expect(
    page.getByText("OLD INPUT CONTENTS", { exact: true }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: "View full output" }),
  ).toBeEnabled();
});
