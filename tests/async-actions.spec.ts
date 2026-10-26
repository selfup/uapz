import type { Page } from "@playwright/test";
import type { Library } from "../frontend/types";
import {
  test,
  expect,
  parent,
  queueReply,
  releaseReply,
  callsFor,
} from "./support/workspace";

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

async function promptLibrary(page: Page) {
  const library: Library = {
    workspace: "/test/workspace",
    graphs: ["Demo", "Parts", "Extract"],
    kinds: [],
    agents: [],
    databases: [],
    prompts: ["Project prompt", "Latest prompt"],
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
  };
  await queueReply(page, { method: "bootstrap", value: library });
  await page.getByRole("button", { name: "Refresh library" }).click();
  await page
    .locator(".library summary")
    .filter({ hasText: /^Prompts/ })
    .click();
  await expect(
    page
      .locator(".library")
      .getByRole("button", { name: "Project prompt", exact: true }),
  ).toBeVisible();
  await page
    .locator(".library")
    .getByRole("button", { name: "Agent", exact: true })
    .click();
}

test("preparing a run blocks repeated execution and root navigation until its inputs are ready", async ({
  page,
}) => {
  await queueReply(page, { method: "prepare_run", gate: "prepare" });
  await page.getByRole("button", { name: "Run workflow", exact: true }).click();
  await pending(page, "prepare");
  await expect(
    page.getByRole("button", { name: "Run workflow", exact: true }),
  ).toBeDisabled();
  await expect(
    page.getByRole("button", { name: "New", exact: true }),
  ).toBeDisabled();
  await expect(
    page
      .locator(".library")
      .getByRole("button", { name: "Parts", exact: true }),
  ).toBeDisabled();
  await page.keyboard.press("Control+Enter");
  await page.keyboard.press("Control+Shift+N");
  expect(await callsFor(page, "prepare_run")).toHaveLength(1);
  await expect(page.getByLabel("Workflow name", { exact: true })).toHaveValue(
    "Demo",
  );
  await releaseReply(page, "prepare");
  await expect(page.getByLabel("Run input message")).toHaveValue("Draft text");
  await page.getByRole("button", { name: "Start run", exact: true }).click();
  await expect(
    page.getByRole("region", { name: "Review file changes" }),
  ).toBeVisible();
  expect((await callsFor(page, "start_run"))[0]?.args.doc).toEqual(parent);
});

test("edits made during preparation require fresh run inputs instead of executing the old document", async ({
  page,
}) => {
  await queueReply(page, { method: "prepare_run", gate: "prepare-edit" });
  await page.getByRole("button", { name: "Run workflow", exact: true }).click();
  await pending(page, "prepare-edit");
  await page.locator('[data-id="n0"]').click();
  await page.getByLabel("Name", { exact: true }).fill("newMessage");
  await releaseReply(page, "prepare-edit");
  await expect(page.getByRole("alert")).toContainText(
    "workflow changed while preparing",
  );
  await expect(
    page.getByRole("button", { name: "Start run", exact: true }),
  ).toHaveCount(0);
  expect(await callsFor(page, "start_run")).toHaveLength(0);
  await page.getByRole("button", { name: "Run workflow", exact: true }).click();
  await page.getByRole("button", { name: "Start run", exact: true }).click();
  await expect(
    page.getByRole("region", { name: "Review file changes" }),
  ).toBeVisible();
  const started = (await callsFor(page, "start_run"))[0]?.args.doc as
    typeof parent | undefined;
  expect(started?.nodes[0]?.name).toBe("newMessage");
});

test("a graph load started before Run cannot replace its pending input document", async ({
  page,
}) => {
  await expect(
    page.locator('[data-id="n1"] [data-handleid="port:body"]'),
  ).toBeVisible();
  await queueReply(page, {
    method: "load_graph",
    args: { name: "Parts" },
    gate: "prior-navigation",
  });
  await page
    .locator(".library")
    .getByRole("button", { name: "Parts", exact: true })
    .click();
  await pending(page, "prior-navigation");
  await page.getByRole("button", { name: "Run workflow", exact: true }).click();
  await expect(page.getByLabel("Run input message")).toBeVisible();
  await releaseReply(page, "prior-navigation");
  await expect(page.getByLabel("Workflow name", { exact: true })).toHaveValue(
    "Demo",
  );
  await expect(page.getByLabel("Run input message")).toHaveValue("Draft text");
  await page.getByRole("button", { name: "Start run", exact: true }).click();
  await expect(
    page.getByRole("region", { name: "Review file changes" }),
  ).toBeVisible();
  expect((await callsFor(page, "start_run"))[0]?.args.doc).toEqual(parent);
});

for (const unavailable of ["error", "null"] as const) {
  test(`an accepted run cannot be resubmitted when its first status response is ${unavailable}`, async ({
    page,
  }) => {
    await page
      .getByRole("button", { name: "Run workflow", exact: true })
      .click();
    await queueReply(page, { method: "start_run", gate: "accept-start" });
    await page.getByRole("button", { name: "Start run", exact: true }).click();
    await pending(page, "accept-start");
    await queueReply(page, {
      method: "runtime_state",
      ...(unavailable === "error"
        ? { error: "Execution status is unavailable" }
        : { value: null }),
    });
    await queueReply(page, { method: "runtime_state", gate: "recover-status" });
    await releaseReply(page, "accept-start");
    await pending(page, "recover-status");
    if (unavailable === "error")
      await expect(page.getByRole("alert")).toContainText(
        "Execution status is unavailable",
      );
    await expect(
      page.getByRole("button", { name: "Start run", exact: true }),
    ).toHaveCount(0);
    await expect(
      page.getByRole("button", { name: "New", exact: true }),
    ).toBeDisabled();
    await expect(
      page.getByRole("button", { name: "Cancel run", exact: true }),
    ).toBeVisible();
    await page.keyboard.press("Control+Enter");
    expect(await callsFor(page, "start_run")).toHaveLength(1);
    expect(await callsFor(page, "prepare_run")).toHaveLength(1);
    await releaseReply(page, "recover-status");
    await expect(
      page.getByRole("region", { name: "Review file changes" }),
    ).toBeVisible();
    expect(await callsFor(page, "start_run")).toHaveLength(1);
  });
}

test("loading a prompt preserves newer edits to other fields on the same agent", async ({
  page,
}) => {
  await promptLibrary(page);
  await queueReply(page, {
    method: "load_prompt",
    gate: "prompt",
    value: "Saved project instructions",
  });
  await page
    .locator(".library")
    .getByRole("button", { name: "Project prompt", exact: true })
    .click();
  await pending(page, "prompt");
  await page.getByLabel("Name", { exact: true }).fill("editedAgent");
  await releaseReply(page, "prompt");
  await expect(page.getByLabel("Name", { exact: true })).toHaveValue(
    "editedAgent",
  );
  await expect(page.getByLabel("System prompt", { exact: true })).toHaveValue(
    "Saved project instructions",
  );
  await page.getByRole("button", { name: "Save", exact: true }).click();
  const saved = (await callsFor(page, "save_graph"))[0]?.args.doc as
    typeof parent | undefined;
  expect(saved?.nodes.find((node) => node.id === "n3")).toMatchObject({
    name: "editedAgent",
    data: { type: "agent", prompt: "Saved project instructions" },
  });
});

test("a pending saved prompt cannot replace text the user has since entered", async ({
  page,
}) => {
  await promptLibrary(page);
  await queueReply(page, {
    method: "load_prompt",
    gate: "prompt-edit",
    value: "Stale instructions",
  });
  await page
    .locator(".library")
    .getByRole("button", { name: "Project prompt", exact: true })
    .click();
  await pending(page, "prompt-edit");
  await page
    .getByLabel("System prompt", { exact: true })
    .fill("My current instructions");
  await releaseReply(page, "prompt-edit");
  await expect(page.getByLabel("System prompt", { exact: true })).toHaveValue(
    "My current instructions",
  );
  await page.getByRole("button", { name: "Save", exact: true }).click();
  const saved = (await callsFor(page, "save_graph"))[0]?.args.doc as
    typeof parent | undefined;
  expect(saved?.nodes.find((node) => node.id === "n3")?.data).toMatchObject({
    type: "agent",
    prompt: "My current instructions",
  });
});

test("a failed save from the previous document does not surface an error in a new graph", async ({
  page,
}) => {
  await queueReply(page, {
    method: "save_graph",
    gate: "old-save",
    error: "Old document is unwritable",
  });
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await pending(page, "old-save");
  await page.getByRole("button", { name: "New", exact: true }).click();
  await expect(
    page.getByRole("heading", { name: "Build a workflow" }),
  ).toBeVisible();
  await releaseReply(page, "old-save");
  await expect(page.getByRole("alert")).toHaveCount(0);
  await expect(
    page.getByRole("heading", { name: "Build a workflow" }),
  ).toBeVisible();
});

for (const response of ["snapshot", "failure"] as const) {
  test(`a late runtime poll ${response} cannot override a newer completed status`, async ({
    page,
  }) => {
    await page
      .getByRole("button", { name: "Run workflow", exact: true })
      .click();
    await page.getByRole("button", { name: "Start run", exact: true }).click();
    const review = page.getByRole("region", { name: "Review file changes" });
    await expect(review).toBeVisible();
    const stale = await page.evaluate(
      () => (window as any).__testState.runtime,
    );
    await queueReply(page, {
      method: "runtime_state",
      gate: "old-runtime",
      ...(response === "snapshot"
        ? { value: stale }
        : { error: "Old poll failed" }),
    });
    await pending(page, "old-runtime");

    await review
      .getByRole("button", { name: "Apply changes", exact: true })
      .click();
    await expect(review).toHaveCount(0);
    await expect(
      page.getByRole("button", { name: "New", exact: true }),
    ).toBeEnabled();
    // Keep the following poll suspended so a restored review cannot disappear
    // again before assertions observe the stale response's effect.
    await queueReply(page, { method: "runtime_state", gate: "next-runtime" });
    await releaseReply(page, "old-runtime");
    await pending(page, "next-runtime");
    await expect(page.getByRole("alert")).toHaveCount(0);
    await expect(review).toHaveCount(0);
    await expect(
      page.getByRole("button", { name: "Cancel run", exact: true }),
    ).toHaveCount(0);
    await expect(
      page.getByRole("button", { name: "New", exact: true }),
    ).toBeEnabled();
    await releaseReply(page, "next-runtime");
    await expect(page.getByRole("status")).toHaveText("Completed in 0.1s");
  });
}
