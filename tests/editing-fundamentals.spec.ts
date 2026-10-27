import type { Page } from "@playwright/test";
import { test, expect, callsFor } from "./support/workspace";

const nodes = (page: Page) => page.locator(".react-flow__node");
const edges = (page: Page) => page.locator(".react-flow__edge");

test("deleting a node can be undone and redone from the keyboard and toolbar", async ({
  page,
}) => {
  await page.locator('[data-id="n1"] .graph-card-title').click();
  await page.getByLabel("Workflow graph editor", { exact: true }).focus();
  await page.keyboard.press("Delete");
  await expect(nodes(page)).toHaveCount(2);
  await expect(page.getByText("Unsaved changes")).toBeVisible();
  await page.keyboard.press("ControlOrMeta+z");
  await expect(nodes(page)).toHaveCount(3);
  await expect(edges(page)).toHaveCount(2);
  await expect(page.getByText("Saved workflow")).toBeVisible();
  await page.keyboard.press("ControlOrMeta+Shift+z");
  await expect(nodes(page)).toHaveCount(2);
  await expect(page.getByText("Unsaved changes")).toBeVisible();
  await page.getByRole("button", { name: "Undo", exact: true }).click();
  await expect(nodes(page)).toHaveCount(3);
  await expect(
    page.getByRole("button", { name: "Undo", exact: true }),
  ).toBeDisabled();
  await expect(
    page.getByRole("button", { name: "Redo", exact: true }),
  ).toBeEnabled();
});

test("typing in the inspector forms one undo step and leaves text fields their own undo", async ({
  page,
}) => {
  await page.locator('[data-id="n0"] .graph-card-title').click();
  const name = page.getByLabel("Name", { exact: true });
  await name.fill("messageX");
  await name.pressSequentially("yz");
  await expect(page.locator('[data-id="n0"] .graph-card-title')).toHaveText(
    "messageXyz",
  );
  // Inside the field the browser owns undo; the graph step stays intact.
  await page.keyboard.press("ControlOrMeta+z");
  await expect(name).not.toHaveValue("message");
  await expect(page.locator('[data-id="n0"] .graph-card-title')).not.toHaveText(
    "message",
  );
  await page
    .locator(".react-flow__pane")
    .click({ position: { x: 700, y: 600 } });
  await page.keyboard.press("ControlOrMeta+z");
  await expect(page.locator('[data-id="n0"] .graph-card-title')).toHaveText(
    "message",
  );
});

test("shift drag and modifier clicks select several nodes that move and delete together", async ({
  page,
}) => {
  const pane = page.locator(".react-flow__pane");
  const box = (await pane.boundingBox())!;
  await page.keyboard.down("Shift");
  await page.mouse.move(box.x + 30, box.y + 40);
  await page.mouse.down();
  await page.mouse.move(box.x + 620, box.y + 300, { steps: 10 });
  await page.mouse.up();
  await page.keyboard.up("Shift");
  await expect(page.locator('[data-id="n0"]')).toHaveClass(/selected/);
  await expect(page.locator('[data-id="n1"]')).toHaveClass(/selected/);
  await expect(page.locator('[data-id="n2"]')).not.toHaveClass(/selected/);
  const before = (await page.locator('[data-id="n1"]').boundingBox())!;
  const from = (await page
    .locator('[data-id="n0"] .graph-card-title')
    .boundingBox())!;
  await page.mouse.move(from.x + 20, from.y + 5);
  await page.mouse.down();
  await page.mouse.move(from.x + 20, from.y + 95, { steps: 10 });
  await page.mouse.up();
  const after = (await page.locator('[data-id="n1"]').boundingBox())!;
  expect(after.y - before.y).toBeGreaterThan(60);
  await page.getByRole("button", { name: "Save", exact: true }).click();
  const saved = (await callsFor(page, "save_graph")).at(-1)!.args.doc as {
    nodes: { id: string; y: number }[];
  };
  expect(saved.nodes.find((node) => node.id === "n1")!.y).toBeGreaterThan(60);
  await page.locator('[data-id="n2"] .graph-card-title').click({
    modifiers: ["ControlOrMeta"],
  });
  await expect(page.locator('[data-id="n0"]')).toHaveClass(/selected/);
  await expect(page.locator('[data-id="n2"]')).toHaveClass(/selected/);
  await expect(page.locator(".inspector h2")).toHaveText("Save heading");
  await page.getByLabel("Workflow graph editor", { exact: true }).focus();
  await page.keyboard.press("Backspace");
  await expect(nodes(page)).toHaveCount(0);
  await page.keyboard.press("ControlOrMeta+z");
  await expect(nodes(page)).toHaveCount(3);
});

test("duplicate, copy and paste create renamed copies with their internal wires", async ({
  page,
}) => {
  await page.locator('[data-id="n0"] .graph-card-title').click();
  await page.locator('[data-id="n1"] .graph-card-title').click({
    modifiers: ["ControlOrMeta"],
  });
  await page.keyboard.press("ControlOrMeta+d");
  await expect(nodes(page)).toHaveCount(5);
  await expect(edges(page)).toHaveCount(3);
  await expect(page.locator('[data-id="n3"] .graph-card-title')).toHaveText(
    "message-copy",
  );
  await expect(page.locator('[data-id="n4"] .graph-card-title')).toHaveText(
    "Create parts-copy",
  );
  await expect(page.locator('[data-id="n3"]')).toHaveClass(/selected/);
  await expect(page.locator('[data-id="n4"]')).toHaveClass(/selected/);
  await expect(page.locator('[data-id="n0"]')).not.toHaveClass(/selected/);
  await page.locator('[data-id="n2"] .graph-card-title').click();
  await page.keyboard.press("ControlOrMeta+c");
  await expect(page.getByRole("status")).toHaveText("Copied 1 node");
  await page.keyboard.press("ControlOrMeta+v");
  await expect(nodes(page)).toHaveCount(6);
  await expect(page.locator('[data-id="n5"] .graph-card-title')).toHaveText(
    "Save heading-copy",
  );
  await page.keyboard.press("ControlOrMeta+v");
  await expect(nodes(page)).toHaveCount(7);
  const [first, second] = await Promise.all([
    page.locator('[data-id="n5"]').boundingBox(),
    page.locator('[data-id="n6"]').boundingBox(),
  ]);
  expect(second!.x - first!.x).toBeCloseTo(40, 0);
  await page.keyboard.press("ControlOrMeta+z");
  await page.keyboard.press("ControlOrMeta+z");
  await page.keyboard.press("ControlOrMeta+z");
  await expect(nodes(page)).toHaveCount(3);
});

test("the context menu offers node, wire and canvas actions", async ({
  page,
}) => {
  await page.locator('[data-id="n2"] .graph-card-title').click({
    button: "right",
  });
  const menu = page.getByRole("menu", { name: "Canvas actions" });
  await expect(menu).toBeVisible();
  await expect(menu.getByRole("menuitem")).toHaveText([
    "Run node",
    "Run fresh",
    "Copy⌘C",
    "Duplicate⌘D",
    "Delete⌫",
  ]);
  await menu.getByRole("menuitem", { name: "Delete" }).click();
  await expect(nodes(page)).toHaveCount(2);
  await expect(menu).toHaveCount(0);
  await page.locator('[data-id="n1"] .graph-card-title').click({
    button: "right",
  });
  await expect(menu.getByRole("menuitem").first()).toHaveText(
    "Open child graph",
  );
  await page.keyboard.press("Escape");
  await expect(menu).toHaveCount(0);
  // Wires are SVG paths; aim at the midpoint between the two ports.
  const source = (await page
    .locator('[data-id="n0"] [data-handleid="default"]')
    .boundingBox())!;
  const target = (await page
    .locator('[data-id="n1"] [data-handleid="input"]')
    .boundingBox())!;
  await page.mouse.click(
    (source.x + target.x) / 2 + source.width / 2,
    (source.y + target.y) / 2 + source.height / 2,
    { button: "right" },
  );
  await menu.getByRole("menuitem", { name: "Delete wire" }).click();
  await expect(edges(page)).toHaveCount(0);
  const pane = page.locator(".react-flow__pane");
  await pane.click({ button: "right", position: { x: 500, y: 500 } });
  await expect(
    menu.getByRole("menuitem", { name: "Paste here" }),
  ).toBeDisabled();
  await menu.getByRole("menuitem", { name: "Add node here" }).click();
  await expect(nodes(page)).toHaveCount(3);
  // The deleted n2 id is free again, so the added node takes it.
  const added = (await page.locator('[data-id="n2"]').boundingBox())!;
  const box = (await pane.boundingBox())!;
  expect(Math.abs(added.x + added.width / 2 - (box.x + 500))).toBeLessThan(4);
});

test("library rows can be dragged onto the canvas where they drop", async ({
  page,
}) => {
  const pane = page.locator(".react-flow__pane");
  await page
    .locator(".library")
    .getByRole("button", { name: "Agent", exact: true })
    .dragTo(pane, { targetPosition: { x: 650, y: 520 } });
  await expect(nodes(page)).toHaveCount(4);
  await expect(page.locator('[data-id="n3"] .graph-card-kind')).toContainText(
    "agent",
  );
  const added = (await page.locator('[data-id="n3"]').boundingBox())!;
  const box = (await pane.boundingBox())!;
  expect(Math.abs(added.x + added.width / 2 - (box.x + 650))).toBeLessThan(4);
  expect(Math.abs(added.y + 24 - (box.y + 520))).toBeLessThan(4);
  await expect(page.locator('[data-id="n3"]')).toHaveClass(/selected/);
});
