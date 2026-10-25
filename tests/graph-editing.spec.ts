import type { Locator, Page } from "@playwright/test";
import { test, expect, parent, callsFor } from "./support/workspace";

async function dragBetween(page: Page, source: Locator, target: Locator) {
  await expect(source).toBeVisible();
  await expect(target).toBeVisible();
  const start = await source.boundingBox();
  const end = await target.boundingBox();
  expect(start).not.toBeNull();
  expect(end).not.toBeNull();
  await page.mouse.move(
    start!.x + start!.width / 2,
    start!.y + start!.height / 2,
  );
  await page.mouse.down();
  await page.mouse.move(end!.x + end!.width / 2, end!.y + end!.height / 2, {
    steps: 12,
  });
  await page.mouse.up();
}

test("dragging named ports preserves distinct wires and rejects a duplicate connection", async ({
  page,
}) => {
  const body = page.locator('[data-id="n1"] [data-handleid="port:body"]');
  const heading = page.locator('[data-id="n1"] [data-handleid="port:heading"]');
  const input = page.locator('[data-id="n2"] [data-handleid="input"]');
  await dragBetween(page, body, input);
  await expect(page.locator(".react-flow__edge")).toHaveCount(3);
  await dragBetween(page, body, input);
  await dragBetween(page, heading, input);
  await expect(page.locator(".react-flow__edge")).toHaveCount(3);
  await page.getByRole("button", { name: "Save", exact: true }).click();
  const saved = (await callsFor(page, "save_graph")).at(-1)!.args.doc;
  expect(saved).toMatchObject({
    edges: [...parent.edges, { from: "n1", output: "body", to: "n2" }],
    outputs: parent.outputs,
  });
  await page
    .locator(".library")
    .getByRole("button", { name: "Demo", exact: true })
    .click();
  await expect(page.locator(".react-flow__edge")).toHaveCount(3);
  await expect(body).toBeVisible();
  await expect(heading).toBeVisible();
});

test("deleting a workflow node removes incoming wires, outgoing wires and published ports", async ({
  page,
}) => {
  await page.locator('[data-id="n1"] .graph-card-title').click();
  await page.getByLabel("Workflow graph editor", { exact: true }).focus();
  await page.keyboard.press("Delete");
  await expect(page.locator(".react-flow__node")).toHaveCount(2);
  await expect(page.locator('[data-id="n1"]')).toHaveCount(0);
  await expect(page.locator(".react-flow__edge")).toHaveCount(0);
  await page.getByRole("button", { name: "Save", exact: true }).click();
  const saved = (await callsFor(page, "save_graph")).at(-1)!.args.doc;
  expect(saved).toMatchObject({
    nodes: parent.nodes.filter((node) => node.id !== "n1"),
    edges: [],
    outputs: {},
  });
  await page
    .locator(".library")
    .getByRole("button", { name: "Demo", exact: true })
    .click();
  await expect(page.locator(".react-flow__node")).toHaveCount(2);
  await expect(page.locator(".react-flow__edge")).toHaveCount(0);
});

test("Backspace edits the selected node's text without deleting the node or its connections", async ({
  page,
}) => {
  await page.locator('[data-id="n0"] .graph-card-title').click();
  const name = page.getByLabel("Name", { exact: true });
  await name.fill("messageX");
  await name.press("End");
  await name.press("Backspace");
  await expect(name).toHaveValue("message");
  await expect(page.locator('[data-id="n0"]')).toHaveClass(/selected/);
  await expect(page.locator(".react-flow__node")).toHaveCount(3);
  await expect(page.locator(".react-flow__edge")).toHaveCount(2);
  await page.getByRole("button", { name: "Save", exact: true }).click();
  const saved = (await callsFor(page, "save_graph")).at(-1)!.args.doc;
  expect(saved).toMatchObject({
    nodes: parent.nodes,
    edges: parent.edges,
    outputs: parent.outputs,
  });
});

test("read-only child graphs ignore deletion, node drags and canvas double-click creation", async ({
  page,
}) => {
  await page.locator('[data-id="n1"] .graph-card-title').dblclick();
  const canvas = page.getByLabel("Read-only workflow graph", { exact: true });
  const node = canvas.locator('[data-id="n4"]');
  await expect(node).toBeVisible();
  await node.locator(".graph-card-title").click();
  await canvas.focus();
  await page.keyboard.press("Delete");
  await page.keyboard.press("Backspace");
  await expect(node).toBeVisible();
  const originalPosition = await node.boundingBox();
  expect(originalPosition).not.toBeNull();
  await page.mouse.move(originalPosition!.x + 70, originalPosition!.y + 45);
  await page.mouse.down();
  await page.mouse.move(originalPosition!.x + 150, originalPosition!.y + 95, {
    steps: 12,
  });
  await page.mouse.up();
  await expect
    .poll(async () => {
      const position = await node.boundingBox();
      return { x: position?.x, y: position?.y };
    })
    .toEqual({ x: originalPosition!.x, y: originalPosition!.y });
  await canvas
    .locator(".react-flow__pane")
    .dblclick({ position: { x: 30, y: 300 } });
  await expect(canvas.locator(".react-flow__node")).toHaveCount(1);
  await expect(
    page.getByRole("button", { name: "Add node", exact: true }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: "Save", exact: true }),
  ).toBeDisabled();
  expect(await callsFor(page, "save_graph")).toHaveLength(0);
  await page
    .getByRole("navigation", { name: "Workflow breadcrumbs" })
    .getByRole("button", { name: "Demo", exact: true })
    .click();
  await page.getByRole("button", { name: "Save", exact: true }).click();
  const saved = (await callsFor(page, "save_graph")).at(-1)!.args.doc;
  expect(saved).toEqual(parent);
});

test("dragging a node saves its new graph coordinates and restores them on reopen", async ({
  page,
}) => {
  const node = page.locator('[data-id="n0"]');
  const title = node.locator(".graph-card-title");
  const initialPosition = await node.boundingBox();
  const initial = await title.boundingBox();
  expect(initialPosition).not.toBeNull();
  expect(initial).not.toBeNull();
  await page.mouse.move(
    initial!.x + initial!.width / 2,
    initial!.y + initial!.height / 2,
  );
  await page.mouse.down();
  await page.mouse.move(
    initial!.x + initial!.width / 2 + 73,
    initial!.y + initial!.height / 2 + 41,
    {
      steps: 12,
    },
  );
  await page.mouse.up();
  const movedPosition = await node.boundingBox();
  expect(movedPosition).not.toBeNull();
  const scale = parent.view.zoom ?? 1;
  const displacement = {
    x: (movedPosition!.x - initialPosition!.x) / scale,
    y: (movedPosition!.y - initialPosition!.y) / scale,
  };
  expect(displacement.x).toBeGreaterThan(50);
  expect(displacement.y).toBeGreaterThan(20);
  await page.getByRole("button", { name: "Save", exact: true }).click();
  const saved = (await callsFor(page, "save_graph")).at(-1)!.args
    .doc as typeof parent;
  const moved = saved.nodes.find((item) => item.id === "n0")!;
  expect(moved.x).toBeCloseTo(parent.nodes[0].x + displacement.x, 1);
  expect(moved.y).toBeCloseTo(parent.nodes[0].y + displacement.y, 1);
  expect(saved.view).toEqual(parent.view);
  expect(saved.edges).toEqual(parent.edges);
  expect(saved.outputs).toEqual(parent.outputs);
  await page
    .locator(".library")
    .getByRole("button", { name: "Demo", exact: true })
    .click();
  await expect
    .poll(async () => {
      const position = await node.boundingBox();
      return { x: position?.x, y: position?.y };
    })
    .toEqual({ x: movedPosition!.x, y: movedPosition!.y });
});
