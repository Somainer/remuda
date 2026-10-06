import { expect, test } from "@playwright/test";

function row(page: import("@playwright/test").Page, text: string) {
  return page.getByTestId("session-row").filter({ hasText: text }).first();
}

test.describe("workflow tree, task track, events drawer", () => {
  test("working session shows workflow tree and task track", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "看 TaskManager spill").click();
    await expect(page.getByTestId("workflow-tree")).toBeVisible();
    await expect(page.getByTestId("workflow-phase")).toContainText("compile");
    // Members are Claude sub-sessions inside this session: every row opens the
    // subagent drill-in (never a /s/<child> instance link).
    const members = page.getByTestId("workflow-member");
    await expect(members.first()).toBeVisible();
    await expect(members.first().locator("a")).toBeVisible();
    await expect(page.getByTestId("workflow-member").filter({ hasText: "sonnet-cold" })).toBeVisible();
    await expect(page.getByTestId("task-track")).toContainText("summarize");
    await expect(page.getByTestId("usage-row").first()).toContainText("usage");
    await expect(page.getByTestId("opaque-row")).toBeVisible();
    await page.getByTestId("opaque-row").locator("summary").click();
    await expect(page.getByTestId("opaque-json")).toContainText("rate_limit_event");
  });

  test("two phase-summary clicks keep the native details in sync with React state", async ({ page }) => {
    // Round 3 codex: the native <summary> used to toggle details.open itself
    // while React also flipped state, so the second click desynced the real
    // details.open / body visibility / chevron / aria-expanded. React now
    // preventDefaults and owns the disclosure; assert all four in a real
    // browser after open → close → open.
    await page.goto("/sessions");
    await row(page, "看 TaskManager spill").click();
    const details = page.getByTestId("workflow-phase").first();
    const summary = details.locator("summary").first();
    const body = details.locator("ul").first();

    // Initially open (running run/phase).
    await expect(summary).toHaveAttribute("aria-expanded", "true");
    await expect(body).toBeVisible();

    // Click 1: the native details.open, the body and aria-expanded all close.
    await summary.click();
    await expect(summary).toHaveAttribute("aria-expanded", "false");
    await expect(body).toBeHidden();
    expect(await details.evaluate((el) => (el as HTMLDetailsElement).open)).toBe(false);

    // Click 2: they all reopen together (the previously-desynced click).
    await summary.click();
    await expect(summary).toHaveAttribute("aria-expanded", "true");
    await expect(body).toBeVisible();
    expect(await details.evaluate((el) => (el as HTMLDetailsElement).open)).toBe(true);
  });

  test("native auto-expand (find-in-page) re-syncs React state so one click collapses", async ({ page }) => {
    // Round 4: something OTHER than the summary click (browser find-in-page,
    // form restore) can set details.open = true natively while React state
    // stays collapsed. onToggle now follows the DOM state; after such an
    // auto-expand, aria-expanded is true and a single summary click collapses.
    await page.goto("/sessions");
    await row(page, "看 TaskManager spill").click();
    const details = page.getByTestId("workflow-phase").first();
    const summary = details.locator("summary").first();
    const body = details.locator("ul").first();

    // Collapse via the controlled summary.
    await summary.click();
    await expect(summary).toHaveAttribute("aria-expanded", "false");
    expect(await details.evaluate((el) => (el as HTMLDetailsElement).open)).toBe(false);

    // Simulate find-in-page / an a11y action forcing the native disclosure
    // open WITHOUT a summary click.
    await details.evaluate((el) => {
      (el as HTMLDetailsElement).open = true;
      el.dispatchEvent(new Event("toggle"));
    });
    // React state follows the DOM: aria-expanded/body catch up.
    await expect(summary).toHaveAttribute("aria-expanded", "true");
    await expect(body).toBeVisible();

    // One summary click collapses (it would have failed before onToggle).
    await summary.click();
    await expect(summary).toHaveAttribute("aria-expanded", "false");
    expect(await details.evaluate((el) => (el as HTMLDetailsElement).open)).toBe(false);
    await expect(body).toBeHidden();
  });

  test("raw events drawer filters by kind", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "看 TaskManager spill").click();
    await page.getByRole("button", { name: "原始事件" }).click();
    await expect(page).toHaveURL(/\/events$/);
    await expect(page.getByTestId("raw-events")).toBeVisible();
    await page.getByRole("button", { name: "workflow.member" }).click();
    const rows = page.getByTestId("raw-event-row");
    await expect(rows.first()).toHaveAttribute("data-kind", "workflow.member");
    await rows.first().click();
    await expect(page.getByTestId("raw-event-json")).toContainText("workflowId");
  });

  test("every workflow member drills into the agent route", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "看 TaskManager spill").click();
    const cold = page.getByTestId("workflow-member").filter({ hasText: "sonnet-cold" });
    const link = cold.locator("a").first();
    await expect(link).toHaveAttribute("href", /\/agents\/agent-2$/);
  });
});
