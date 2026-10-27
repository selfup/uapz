import { test, expect, callsFor } from "./support/workspace";

test("the last opened workflow and panel layout come back after a relaunch", async ({
  page,
}) => {
  await expect(page.getByLabel("Workflow name", { exact: true })).toHaveValue(
    "Demo",
  );
  await page
    .locator(".library")
    .getByRole("button", { name: "Parts", exact: true })
    .click();
  await page.getByRole("button", { name: "Hide library", exact: true }).click();
  await expect(page.locator(".library")).toBeHidden();
  await page.evaluate(() => {
    const layout = JSON.parse(localStorage.getItem("uapz.layout") || "{}");
    layout.rightWidth = 420;
    localStorage.setItem("uapz.layout", JSON.stringify(layout));
  });
  await page.reload();
  await expect(page.getByLabel("Workflow name", { exact: true })).toHaveValue(
    "Parts",
  );
  await expect(page.locator(".library")).toBeHidden();
  const inspector = (await page.locator(".inspector-column").boundingBox())!;
  expect(inspector.width).toBeCloseTo(420, 0);
  await page.keyboard.press("ControlOrMeta+b");
  await expect(page.locator(".library")).toBeVisible();
  await page.keyboard.press("ControlOrMeta+j");
  await expect(page.locator(".inspector-column")).toBeHidden();
  await page
    .getByRole("button", { name: "Show inspector", exact: true })
    .click();
  await expect(page.locator(".inspector-column")).toBeVisible();
});

test("a stale last-workflow entry is ignored", async ({ page }) => {
  await page.evaluate(() =>
    localStorage.setItem(
      "uapz.layout",
      JSON.stringify({
        lastGraph: { workspace: "/test/workspace", name: "Gone" },
      }),
    ),
  );
  await page.reload();
  await expect(page.getByText("Connecting to your workspace…")).toHaveCount(0);
  await expect(page.getByLabel("Workflow name", { exact: true })).toHaveValue(
    "untitled",
  );
  await expect(page.getByRole("alert")).toHaveCount(0);
});

test("leaving unsaved changes offers to save them first", async ({ page }) => {
  await page.locator('[data-id="n0"]').click();
  await page.getByLabel("Name", { exact: true }).fill("keepMe");
  await page.getByRole("button", { name: "New", exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "Unsaved changes" });
  await expect(dialog.getByRole("button")).toHaveText([
    "Cancel",
    "Discard changes",
    "Save and continue",
  ]);
  await dialog
    .getByRole("button", { name: "Save and continue", exact: true })
    .click();
  await expect(
    page.getByRole("heading", { name: "Build a workflow" }),
  ).toBeVisible();
  const saved = (await callsFor(page, "save_graph")).at(-1)!.args.doc as {
    nodes: { name?: string }[];
  };
  expect(saved.nodes[0]?.name).toBe("keepMe");
});

test("library deletions use a specific dialog title and action", async ({
  page,
}) => {
  await page.evaluate(() => {
    (window as any).__testState.agents = [
      {
        name: "Helper",
        config: {
          provider: "ollama",
          endpoint: "",
          model_type: "generative",
          model: "llama3.1",
          auth: { kind: "none" },
          prompt: "",
          context_size: 8192,
          temperature: 0.7,
          output: "text",
        },
      },
    ];
  });
  await page.getByRole("button", { name: "Refresh library" }).click();
  await page
    .locator(".library summary")
    .filter({ hasText: /^Agents/ })
    .click();
  // The delete control appears on hover.
  const row = page.locator(".library-row").filter({ hasText: "Helper" });
  await row.hover();
  await row.getByLabel("Delete Helper").click();
  const dialog = page.getByRole("dialog", { name: "Delete from library" });
  await expect(dialog).toBeVisible();
  await dialog.getByRole("button", { name: "Cancel", exact: true }).click();
  await expect(dialog).toHaveCount(0);
});

test("the shortcut list opens from the footer and the ? key", async ({
  page,
}) => {
  await page.getByRole("button", { name: "Keyboard shortcuts" }).click();
  const dialog = page.getByRole("dialog", { name: "Keyboard shortcuts" });
  await expect(dialog).toBeVisible();
  await expect(dialog.getByText("Undo · redo")).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(dialog).toHaveCount(0);
  await page
    .locator(".react-flow__pane")
    .click({ position: { x: 700, y: 600 } });
  await page.keyboard.press("?");
  await expect(dialog).toBeVisible();
  await dialog.getByRole("button", { name: "Close", exact: true }).click();
  await expect(dialog).toHaveCount(0);
});

test("the header previews the file name a workflow will save under", async ({
  page,
}) => {
  const name = page.getByLabel("Workflow name", { exact: true });
  await name.fill("My Graph!");
  await expect(page.locator(".save-as")).toHaveText("· saves as My-Graph-");
  await name.fill("my-graph");
  await expect(page.locator(".save-as")).toHaveCount(0);
});

test("the theme follows the system and can be forced from the footer", async ({
  page,
}) => {
  const html = page.locator("html");
  await expect(html).toHaveCSS("background-color", "rgb(17, 18, 22)");
  await page.emulateMedia({ colorScheme: "light" });
  await expect(html).toHaveCSS("background-color", "rgb(236, 238, 243)");
  await page.getByLabel("Theme", { exact: true }).selectOption("dark");
  await expect(html).toHaveCSS("background-color", "rgb(17, 18, 22)");
  await page.emulateMedia({ colorScheme: "dark" });
  await page.getByLabel("Theme", { exact: true }).selectOption("light");
  await expect(html).toHaveCSS("background-color", "rgb(236, 238, 243)");
  await page.reload();
  await expect(html).toHaveCSS("background-color", "rgb(236, 238, 243)");
  await expect(page.getByLabel("Theme", { exact: true })).toHaveValue("light");
});
