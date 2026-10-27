import type { Page } from "@playwright/test";
import type { Library } from "../frontend/types";
import { test, expect, queueReply, callsFor } from "./support/workspace";

async function libraryWith(page: Page, files: Partial<Library>) {
  const library: Library = {
    workspace: "/test/workspace",
    graphs: ["Demo", "Parts", "Extract"],
    kinds: [],
    agents: [],
    databases: [],
    prompts: [],
    scripts: [],
    data: [],
    catalog: {
      nodes: [],
      providers: [],
      model_types: [],
      outputs: [],
      engines: [],
      runtimes: [],
      formats: [],
      write_modes: [],
      input_types: [],
    },
    ...files,
  };
  await queueReply(page, { method: "bootstrap", value: library });
  await page.getByRole("button", { name: "Refresh library" }).click();
}

test("the empty state renders once and its call to action adds a Run input", async ({
  page,
}) => {
  await page.getByRole("button", { name: "New", exact: true }).click();
  await expect(page.getByText("Build a workflow")).toHaveCount(1);
  await page
    .getByRole("button", { name: "Start with a Run input", exact: true })
    .click();
  await expect(page.locator(".react-flow__node")).toHaveCount(1);
  await expect(page.getByText("Build a workflow")).toHaveCount(0);
});

test("nodes added from the library land in free space instead of on each other", async ({
  page,
}) => {
  const agent = page
    .locator(".library")
    .getByRole("button", { name: "Agent", exact: true });
  await agent.click();
  await agent.click();
  await agent.click();
  const boxes = await Promise.all(
    ["n0", "n1", "n2", "n3", "n4", "n5"].map((id) =>
      page.locator(`[data-id="${id}"]`).boundingBox(),
    ),
  );
  for (let a = 0; a < boxes.length; a++)
    for (let b = a + 1; b < boxes.length; b++) {
      const first = boxes[a]!;
      const second = boxes[b]!;
      const overlap =
        first.x < second.x + second.width &&
        first.x + first.width > second.x &&
        first.y < second.y + second.height &&
        first.y + first.height > second.y;
      expect(overlap, `${a} overlaps ${b}`).toBe(false);
    }
});

test("script and data rows retarget the selected node and otherwise add one", async ({
  page,
}) => {
  await libraryWith(page, {
    scripts: ["tool.py", "run.sh"],
    data: ["rows.csv"],
  });
  for (const section of [/^Scripts/, /^Data\d/])
    await page.locator(".library summary").filter({ hasText: section }).click();
  await page
    .locator(".library")
    .getByRole("button", { name: "Script", exact: true })
    .click();
  await expect(page.locator(".react-flow__node")).toHaveCount(4);
  await page
    .locator(".library")
    .getByRole("button", { name: "tool.py", exact: true })
    .click();
  await expect(page.locator(".react-flow__node")).toHaveCount(4);
  await expect(page.getByLabel("Script file", { exact: true })).toHaveValue(
    "tool.py",
  );
  await page
    .locator(".library")
    .getByRole("button", { name: "run.sh", exact: true })
    .click();
  await expect(page.getByLabel("Script file", { exact: true })).toHaveValue(
    "run.sh",
  );
  await expect(page.getByLabel("Runtime", { exact: true })).toHaveValue(
    "shell",
  );
  await page
    .locator(".library")
    .getByRole("button", { name: "rows.csv", exact: true })
    .click();
  await expect(page.locator(".react-flow__node")).toHaveCount(5);
  await expect(page.getByLabel("File", { exact: true })).toHaveValue(
    "rows.csv",
  );
  await expect(page.getByLabel("Format", { exact: true })).toHaveValue("csv");
});

test("file run inputs offer the data directory listing", async ({ page }) => {
  await libraryWith(page, { data: ["notes.txt", "rows.csv"] });
  await queueReply(page, {
    method: "prepare_run",
    value: {
      targets: ["n2"],
      fields: [
        {
          node: "n0",
          name: "source",
          config: { kind: "file", default: "", required: true },
        },
      ],
      inputs: { source: "" },
    },
  });
  await page.getByRole("button", { name: "Run workflow", exact: true }).click();
  const input = page.getByLabel("Run input source");
  await page.locator(".run-form").getByLabel("Show options").click();
  await expect(page.locator(".run-form").getByRole("option")).toHaveText([
    "notes.txt",
    "rows.csv",
  ]);
  await page
    .locator(".run-form")
    .getByRole("option", { name: "rows.csv", exact: true })
    .click();
  await expect(input).toHaveValue("rows.csv");
  await page.getByRole("button", { name: "Start run", exact: true }).click();
  expect((await callsFor(page, "start_run"))[0]?.args.inputs).toEqual({
    source: "rows.csv",
  });
});

test("validation problems mark the card and hold Run workflow until fixed", async ({
  page,
}) => {
  await queueReply(page, {
    method: "validate_graph",
    value: {
      issues: [
        { node: "n0", field: "name", message: "is not unique" },
        { node: "n0", field: "default", message: "is too long" },
      ],
      fields: [],
      sinks: ["n2"],
    },
  });
  await page.locator('[data-id="n0"]').click();
  await page.getByLabel("Name", { exact: true }).fill("messageA");
  const badge = page.locator('[data-id="n0"] .card-problem');
  await expect(badge).toHaveAttribute("title", "name: is not unique");
  const run = page.getByRole("button", {
    name: "Run workflow, 2 problems to fix first",
  });
  await expect(run).toBeDisabled();
  await expect(
    page.getByRole("button", { name: "Run workflow fresh", exact: true }),
  ).toBeDisabled();
  await page.getByLabel("Name", { exact: true }).fill("messageB");
  await expect(badge).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: "Run workflow", exact: true }),
  ).toBeEnabled();
});

test("Run fresh starts the whole workflow without cached outputs", async ({
  page,
}) => {
  await page
    .getByRole("button", { name: "Run workflow fresh", exact: true })
    .click();
  await page.getByRole("button", { name: "Start run", exact: true }).click();
  await expect(
    page.getByRole("region", { name: "Review file changes" }),
  ).toBeVisible();
  expect((await callsFor(page, "start_run"))[0]?.args.fresh).toBe(true);
});

test("the review panel steps through files with Previous and Next", async ({
  page,
}) => {
  await page.getByRole("button", { name: "Run workflow", exact: true }).click();
  await page.getByRole("button", { name: "Start run", exact: true }).click();
  const review = page.getByRole("region", { name: "Review file changes" });
  await expect(review).toBeVisible();
  await page.evaluate(() => {
    const runtime = (window as any).__testState.runtime;
    runtime.review.files.push({
      path: "body.md",
      before_bytes: 3,
      after_bytes: 9,
      diff: "-old\n+Body text\n",
    });
  });
  await expect(review.getByText("File 1 of 2")).toBeVisible();
  await expect(review.getByLabel("Previous file")).toBeDisabled();
  await review.getByLabel("Next file").click();
  await expect(review.getByText("File 2 of 2")).toBeVisible();
  await expect(review.getByLabel("Review file")).toHaveValue("1");
  await expect(review.getByLabel("Next file")).toBeDisabled();
  await expect(review.locator(".review-diff")).toContainText("Body text");
  await review.getByLabel("Previous file").click();
  await expect(review.getByText("File 1 of 2")).toBeVisible();
});
