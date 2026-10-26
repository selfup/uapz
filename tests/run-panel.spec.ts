import type { Locator, Page } from "@playwright/test";
import {
  test,
  expect,
  queueReply,
  releaseReply,
  callsFor,
} from "./support/workspace";
import type { RunEvent } from "../frontend/runFeed";

async function start(page: Page) {
  await page.getByRole("button", { name: "Run workflow", exact: true }).click();
  await expect(
    page.getByRole("tab", { name: "Run output", exact: true }),
  ).toHaveAttribute("aria-selected", "true");
  await page.getByRole("button", { name: "Start run", exact: true }).click();
  await expect(
    page.getByRole("region", { name: "Review file changes" }),
  ).toBeVisible();
  // Let the fake execution advance beyond review without completing it.
  await page.evaluate(() => {
    (window as any).__testState.runtime.review = null;
  });
}

async function publish(page: Page, events: RunEvent[]) {
  await page.evaluate((events) => {
    (window as any).__testState.runtime.events = events;
  }, events);
}

function distanceFromBottom(feed: Locator) {
  return feed.evaluate(
    (element) =>
      element.scrollHeight - element.clientHeight - element.scrollTop,
  );
}

test("Run opens all node outputs including named and nested results without selecting nodes", async ({
  page,
}, testInfo) => {
  await start(page);
  const result = {
    state: "ran" as const,
    label: "Text",
    preview: "Input value",
    complete: true,
  };
  await queueReply(page, {
    method: "output",
    args: { run_id: "live-1", node: "n0", call_path: "n1/n4" },
    value: {
      label: "Text",
      text: "Full child output",
      base64: null,
      saved: null,
      complete: true,
    },
  });
  await publish(page, [
    { seq: 1, kind: "output", id: "n0", result },
    {
      seq: 2,
      kind: "output",
      id: "n1/n4/n0",
      result: {
        ...result,
        preview: "Child preview",
        complete: false,
        key: "child-key",
      },
    },
    {
      seq: 3,
      kind: "output",
      id: "n1",
      result: {
        ...result,
        state: "reused",
        preview: "Primary content",
        named_outputs: {
          heading: { ...result, preview: "Heading content" },
          body: { ...result, preview: "Body content" },
        },
      },
    },
    { seq: 4, kind: "text", id: "n2", text: "Streaming the final step" },
  ]);
  const feed = page.getByLabel("Run output feed", { exact: true });
  for (const text of [
    "Input value",
    "Full child output",
    "Primary content",
    "Heading content",
    "Body content",
    "Streaming the final step",
  ])
    await expect(feed).toContainText(text);
  await expect(feed).toContainText("Original source");
  await expect(feed).toContainText("Create parts");
  await expect(feed).toContainText("reused");
  // Selection and child navigation must not hide the workflow-wide live feed.
  await page.locator('[data-id="n1"]').dblclick();
  await expect(
    page.getByRole("tab", { name: "Run output", exact: true }),
  ).toHaveAttribute("aria-selected", "true");
  await expect(feed).toContainText("Input value");
  await expect(feed).toContainText("Full child output");
  expect(await callsFor(page, "output")).toHaveLength(1);
  await page.screenshot({
    path: testInfo.outputPath("workflow-run-output.png"),
    fullPage: true,
  });
});

test("live output follows new activity, pauses for scrollback and resumes on request", async ({
  page,
}) => {
  await start(page);
  const events: RunEvent[] = Array.from({ length: 70 }, (_, index) => ({
    seq: index + 1,
    kind: "log",
    text: `Job line ${index + 1}: processing the workflow`,
  }));
  await publish(page, events);
  const feed = page.getByLabel("Run output feed", { exact: true });
  await expect(feed).toContainText("Job line 70:");
  await expect.poll(() => distanceFromBottom(feed)).toBeLessThan(3);
  await feed.hover();
  await page.mouse.wheel(0, -600);
  const follow = page.getByRole("button", {
    name: "Follow output",
    exact: true,
  });
  await expect(follow).toBeVisible();
  const scrollback = await feed.evaluate((element) => element.scrollTop);
  events.push({ seq: 71, kind: "log", text: "Job line 71: still running" });
  await publish(page, events);
  await expect(feed).toContainText("Job line 71:");
  await expect
    .poll(async () =>
      Math.abs(
        (await feed.evaluate((element) => element.scrollTop)) - scrollback,
      ),
    )
    .toBeLessThan(3);
  await follow.click();
  await expect.poll(() => distanceFromBottom(feed)).toBeLessThan(3);
  events.push({ seq: 72, kind: "log", text: "Job line 72: following again" });
  await publish(page, events);
  await expect(feed).toContainText("Job line 72:");
  await expect.poll(() => distanceFromBottom(feed)).toBeLessThan(3);

  // Content can grow after a poll while a complete output is being fetched.
  await queueReply(page, {
    method: "output",
    gate: "long-output",
    args: { run_id: "live-1", node: "n0" },
    value: {
      label: "Text",
      text: "Expanded output\n".repeat(100),
      base64: null,
      saved: null,
      complete: true,
    },
  });
  events.push({
    seq: 73,
    kind: "output",
    id: "n0",
    result: {
      state: "ran",
      preview: "Short preview",
      label: "Text",
      complete: false,
      key: "long-output",
    },
  });
  await publish(page, events);
  await releaseReply(page, "long-output");
  await expect(feed).toContainText("Expanded output");
  await expect.poll(() => distanceFromBottom(feed)).toBeLessThan(3);
  // Tab switches retain the run feed and following behavior.
  await page.getByRole("tab", { name: "Node inspector", exact: true }).click();
  await page.getByRole("tab", { name: "Run output", exact: true }).click();
  await expect.poll(() => distanceFromBottom(feed)).toBeLessThan(3);
});

test("historical runs expose all results and failed full-output loads can be retried", async ({
  page,
}) => {
  await start(page);
  await queueReply(page, {
    method: "output",
    error: "Temporarily unavailable",
  });
  await publish(page, [
    {
      seq: 1,
      kind: "output",
      id: "n0",
      result: {
        state: "ran",
        preview: "Retained preview",
        label: "Text",
        complete: false,
        key: "retry-output",
      },
    },
  ]);
  const feed = page.getByLabel("Run output feed", { exact: true });
  await expect(feed).toContainText("Temporarily unavailable");
  await expect(feed).toContainText("Retained preview");
  await feed.getByRole("button", { name: /retry/i }).click();
  await expect(feed).toContainText("Example full output");

  await page.evaluate(() => {
    const state = (window as any).__testState;
    const output = {
      state: "ran",
      preview: "Historic input",
      label: "Text",
      complete: true,
    };
    const childRun = {
      id: "child-history",
      graph: "Parts",
      targets: ["n4"],
      elapsed_ms: 5,
      nodes: { n4: { ...output, preview: "Historic child output" } },
      log: [],
      outcome: { kind: "ok" },
    };
    const run = {
      id: "historic-run",
      graph: "Demo",
      targets: ["n2"],
      elapsed_ms: 10,
      nodes: {
        n0: output,
        n1: {
          ...output,
          preview: "Historic workflow output",
          named_outputs: {
            body: { ...output, preview: "Historic named body" },
          },
        },
        n2: { state: "failed", error: "Write permission denied" },
      },
      log: ["Saved execution log"],
      outcome: { kind: "failed", error: "Write permission denied" },
      calls: { n1: { graph: state.graphs.Parts, run: childRun } },
      workflows: state.runtime.workflows,
    };
    state.runtime.running = false;
    state.runtime.run = run;
    state.history = [run];
  });
  await expect(page.locator(".status-bar [role=status]")).toHaveText(
    "Run failed",
  );
  await page.getByRole("tab", { name: "Node inspector", exact: true }).click();
  await page.getByRole("tab", { name: "History", exact: true }).click();
  await page.locator(".ins-run > summary").click();
  await page
    .getByRole("button", { name: "Inspect this run", exact: true })
    .click();
  await expect(
    page.getByRole("tab", { name: "Run output", exact: true }),
  ).toHaveAttribute("aria-selected", "true");
  for (const text of [
    "Historic input",
    "Historic workflow output",
    "Historic named body",
    "Historic child output",
    "Write permission denied",
    "Saved execution log",
  ])
    await expect(feed).toContainText(text);
  await expect(feed).not.toContainText("Retained preview");
});

test("large outputs load a bounded preview automatically and expand on request", async ({
  page,
}) => {
  await start(page);
  await queueReply(page, {
    method: "output",
    args: { node: "n0", max_bytes: 256 * 1024 },
    value: {
      label: "Text",
      text: "Automatic bounded preview",
      base64: null,
      saved: null,
      complete: false,
      limited: true,
      bytes: 1024 * 1024,
    },
  });
  await publish(page, [
    {
      seq: 1,
      kind: "output",
      id: "n0",
      result: {
        state: "ran",
        preview: "Saved short preview",
        label: "Text",
        complete: false,
        key: "large-output",
      },
    },
  ]);
  const feed = page.getByLabel("Run output feed", { exact: true });
  await expect(feed).toContainText("Automatic bounded preview");
  await expect(feed).not.toContainText("no longer in the cache");
  await queueReply(page, {
    method: "output",
    args: { node: "n0" },
    value: {
      label: "Text",
      text: "All of the large output",
      base64: null,
      saved: null,
      complete: true,
    },
  });
  await feed
    .getByRole("button", { name: "Load full output", exact: true })
    .click();
  await expect(feed).toContainText("All of the large output");
  const calls = await callsFor(page, "output");
  expect(calls).toHaveLength(2);
  expect(calls[0].args.max_bytes).toBe(256 * 1024);
  expect(calls[1].args).not.toHaveProperty("max_bytes");
});

test("a failed node retains its partial streamed answer next to the failure", async ({
  page,
}) => {
  await start(page);
  await publish(page, [
    {
      seq: 1,
      kind: "text",
      id: "n1",
      text: "Partial answer before connection failed",
    },
  ]);
  const feed = page.getByLabel("Run output feed", { exact: true });
  await expect(feed).toContainText("Partial answer before connection failed");
  await page.evaluate(() => {
    const state = (window as any).__testState;
    const run = {
      id: "failed-stream",
      graph: "Demo",
      targets: ["n2"],
      elapsed_ms: 30,
      nodes: { n1: { state: "failed", error: "Provider connection lost" } },
      log: ["Provider connection lost"],
      outcome: { kind: "failed", error: "Provider connection lost" },
    };
    state.runtime.running = false;
    state.runtime.run = run;
    state.history = [run];
  });
  await expect(page.locator(".status-bar [role=status]")).toHaveText(
    "Run failed",
  );
  await expect(feed).toContainText("Partial answer before connection failed");
  await expect(feed).toContainText("Provider connection lost");
  await expect(feed).toContainText("partial output");
});
