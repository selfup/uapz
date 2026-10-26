import { test, expect, queueReply, parent } from "./support/workspace";

test("a malformed graph response preserves the open document and allows a retry", async ({
  page,
}) => {
  await queueReply(page, {
    method: "load_graph",
    args: { name: "Extract" },
    value: { ...parent, nodes: "invalid" },
  });
  const open = page
    .locator(".library")
    .getByRole("button", { name: "Extract", exact: true });
  await open.click();
  await expect(page.getByRole("alert")).toContainText(
    "Invalid load_graph response",
  );
  await expect(page.getByLabel("Workflow name", { exact: true })).toHaveValue(
    "Demo",
  );
  await expect(page.locator(".react-flow__node")).toHaveCount(3);
  await open.click();
  await expect(page.getByLabel("Workflow name", { exact: true })).toHaveValue(
    "Extract",
  );
  await expect(page.getByRole("alert")).toHaveCount(0);
});

test("a malformed status poll does not destroy active execution controls", async ({
  page,
}) => {
  await page.getByRole("button", { name: "Run workflow", exact: true }).click();
  await page.getByRole("button", { name: "Start run", exact: true }).click();
  await expect(
    page.getByRole("region", { name: "Review file changes" }),
  ).toBeVisible();
  await queueReply(page, {
    method: "runtime_state",
    value: { running: true, marks: [] },
  });
  await expect(page.locator(".error-banner")).toContainText(
    "Invalid runtime_state response",
  );
  const review = page.getByRole("region", { name: "Review file changes" });
  await expect(review).toBeVisible();
  await review.getByRole("button", { name: "Cancel run", exact: true }).click();
  await expect(review).toHaveCount(0);
  await expect(page.locator(".status-bar [role=status]")).toHaveText(
    "Run cancelled",
  );
});
