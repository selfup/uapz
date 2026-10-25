import { test, expect, queueReply, callsFor } from "./support/workspace";

async function prepare(page: import("@playwright/test").Page) {
  await page.getByRole("button", { name: "Run workflow", exact: true }).click();
  await page.getByLabel("Run input message").fill("Keep these values");
}

async function start(page: import("@playwright/test").Page) {
  await prepare(page);
  await page.getByRole("button", { name: "Start run", exact: true }).click();
  await expect(
    page.getByRole("region", { name: "Review file changes" }),
  ).toBeVisible();
}

test("a rejected start retains run inputs for a successful retry", async ({
  page,
}) => {
  await prepare(page);
  await queueReply(page, {
    method: "start_run",
    error: "Input file is unavailable",
  });
  await page.getByRole("button", { name: "Start run", exact: true }).click();
  await expect(page.getByRole("alert")).toHaveText("Input file is unavailable");
  await expect(page.getByLabel("Run input message")).toHaveValue(
    "Keep these values",
  );
  await expect(
    page.getByRole("button", { name: "Start run", exact: true }),
  ).toBeEnabled();
  await page.getByRole("button", { name: "Start run", exact: true }).click();
  await expect(
    page.getByRole("region", { name: "Review file changes" }),
  ).toBeVisible();
  const calls = await callsFor(page, "start_run");
  expect(calls).toHaveLength(2);
  expect(calls[0].args.inputs).toEqual({ message: "Keep these values" });
  expect(calls[1].args).toEqual(calls[0].args);
});

for (const decision of ["Reject", "Cancel run"] as const) {
  test(`${decision} closes a pending review and restores editing controls`, async ({
    page,
  }) => {
    await start(page);
    await expect(
      page.getByRole("button", { name: "New", exact: true }),
    ).toBeDisabled();
    await page
      .getByRole("region", { name: "Review file changes" })
      .getByRole("button", { name: decision, exact: true })
      .click();
    await expect(
      page.getByRole("region", { name: "Review file changes" }),
    ).toHaveCount(0);
    await expect(page.getByRole("status")).toHaveText(
      decision === "Reject" ? "Run failed" : "Run cancelled",
    );
    await expect(
      page.getByRole("button", { name: "New", exact: true }),
    ).toBeEnabled();
    const calls = await callsFor(
      page,
      decision === "Reject" ? "review" : "cancel_run",
    );
    expect(calls).toHaveLength(1);
    expect(calls[0].args).toEqual(
      decision === "Reject"
        ? { run_id: "live-1", review_id: "review-1", decision: "reject" }
        : { run_id: "live-1" },
    );
    await page.getByRole("tab", { name: "History", exact: true }).click();
    await expect(page.locator(".ins-run")).toHaveCount(1);
  });
}

test("a failed review decision keeps the diff available and cancellation working", async ({
  page,
}) => {
  await start(page);
  await queueReply(page, {
    method: "review",
    error: "The file changed after review preparation",
  });
  await page
    .getByRole("button", { name: "Apply changes", exact: true })
    .click();
  await expect(page.getByRole("alert")).toContainText(
    "The file changed after review preparation",
  );
  const review = page.getByRole("region", { name: "Review file changes" });
  await expect(review).toBeVisible();
  await expect(review).toContainText("+ Example");
  await review.getByRole("button", { name: "Cancel run", exact: true }).click();
  await expect(review).toHaveCount(0);
  await expect(page.getByRole("status")).toHaveText("Run cancelled");
});
