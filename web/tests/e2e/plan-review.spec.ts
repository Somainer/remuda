import { expect, test } from "@playwright/test";

// Mock-level plan-review card: the plan body is readable, deny carries the
// optional reviewer feedback, oversized feedback is blocked before submit,
// and a legacy (no-inline-body) plan review points to the session. The
// answer payload's option/digest/feedback shape and the Node first-answer-wins
// CAS are covered by the Rust integration tests.
test.describe("plan review card", () => {
  test.beforeEach(async ({ page }) => {
    await page.goto("/approvals");
    await expect(page.getByTestId("approvals-page")).toBeVisible();
  });

  test("renders the inline plan body in a collapsible pre", async ({ page }) => {
    const row = page.getByTestId("approval-row").filter({ hasText: "实施计划" });
    await expect(row).toBeVisible();
    const details = row.locator("details");
    await expect(details).toBeVisible();
    // Collapsed by default; expand to read the inline plan.
    await expect(row.getByText("先读取 README")).toBeHidden();
    await details.locator("summary").click();
    await expect(row.getByText("先读取 README")).toBeVisible();
    await expect(row.getByText("汇报结果")).toBeVisible();
  });

  test("exposes approve and deny with an optional feedback field", async ({ page }) => {
    const row = page.getByTestId("approval-row").filter({ hasText: "实施计划" });
    await expect(row.getByRole("button", { name: "同意" })).toBeVisible();
    await expect(row.getByRole("button", { name: "拒绝" })).toBeVisible();
    const feedback = row.getByLabel("计划审批拒绝反馈（可选）");
    await expect(feedback).toBeVisible();
    await expect(feedback).toHaveValue("");
    await feedback.fill("先补错误处理再提交");
    await expect(feedback).toHaveValue("先补错误处理再提交");
  });

  test("approve settles the pending card", async ({ page }) => {
    const approvable = page
      .getByTestId("approval-row")
      .filter({ hasText: "实施计划" });
    await approvable.getByRole("button", { name: "同意" }).click();
    await expect(approvable).toHaveCount(0);
  });

  test("a plan review with no inline body points the reviewer to the session", async ({ page }) => {
    const row = page.getByTestId("approval-row").filter({ hasText: "无正文计划" });
    await expect(row).toBeVisible();
    // No collapsible body; the required guidance is shown instead.
    await expect(row.locator("details")).toHaveCount(0);
    await expect(row.getByText(/打开会话查看/)).toBeVisible();
    // The reviewer can still approve without an inline body.
    await expect(row.getByRole("button", { name: "同意" })).toBeVisible();
  });

  test("deny forwards whitespace-only feedback verbatim instead of null", async ({ page }) => {
    const row = page.getByTestId("approval-row").filter({ hasText: "实施计划" });
    const feedback = row.getByLabel("计划审批拒绝反馈（可选）");
    // Whitespace-only: must be submitted as that exact string, not replaced by
    // the driver default (which happens only for a null field).
    await feedback.fill("  ");
    await row.getByRole("button", { name: "拒绝" }).click();
    const submitted = await page.evaluate(
      () =>
        (window as unknown as { __lastInteractionAnswer?: { feedback: string | null } })
          .__lastInteractionAnswer,
    );
    expect(submitted?.feedback).toBe("  ");
  });

  test("deny feedback over 4 KiB UTF-8 is blocked with an actionable message", async ({ page }) => {
    const row = page.getByTestId("approval-row").filter({ hasText: "实施计划" });
    const feedback = row.getByLabel("计划审批拒绝反馈（可选）");
    // 3-byte Chinese characters: 1366 of them exceed the 4096-byte cap while
    // staying well under any character-count-based limit.
    await feedback.fill("中".repeat(1366));
    const error = row.getByRole("alert");
    await expect(error).toBeVisible();
    await expect(error).toContainText("4096");
    // The deny button is disabled until the reviewer shortens the note.
    await expect(row.getByRole("button", { name: "拒绝" })).toBeDisabled();
    // Shorten under the cap; the error clears and deny becomes available.
    await feedback.fill("再想想");
    await expect(error).toHaveCount(0);
    await expect(row.getByRole("button", { name: "拒绝" })).toBeEnabled();
  });

  test("coarse: the compact 查看计划 disclosure is a full 44px tap target", async ({ browser }) => {
    // UO-9 round-3: on /m/inbox the inline-plan summary must be 44px tall on a
    // coarse pointer, and taps 2px inside the top/bottom of that band (points
    // outside the glyph/text box) must both hit the disclosure and toggle it.
    const ctx = await browser.newContext({
      viewport: { width: 390, height: 844 },
      hasTouch: true,
      isMobile: true,
    });
    const page = await ctx.newPage();
    try {
      await page.goto("/m/inbox");
      await expect(page).toHaveURL(/\/m\/inbox/);
      const row = page.getByTestId("approval-row").filter({ hasText: "实施计划" });
      await expect(row).toBeVisible();
      const details = row.locator("details");
      const summary = details.locator("summary");
      await expect(summary).toBeVisible();
      const box = await summary.boundingBox();
      expect(box, "plan summary rendered").toBeTruthy();
      expect(box!.height).toBeGreaterThanOrEqual(43);

      const ownerAt = async (x: number, y: number) =>
        page.evaluate(
          ({ x, y }) => {
            const el = document.elementFromPoint(x, y) as HTMLElement | null;
            return el ? (el.closest("summary") != null ? "summary" : el.tagName) : null;
          },
          { x, y },
        );

      const cx = box!.x + box!.width / 2;
      const topY = box!.y + 2; // 2px inside the band top (above the text box)
      const bottomY = box!.y + box!.height - 2; // 2px inside the band bottom
      expect(await ownerAt(cx, topY)).toBe("summary");
      expect(await ownerAt(cx, bottomY)).toBe("summary");

      // Collapsed by default; a tap on the upper edge opens the plan.
      await expect(row.getByText("先读取 README")).toBeHidden();
      await page.mouse.click(cx, topY);
      await expect(details).toHaveAttribute("open", "");
      await expect(row.getByText("先读取 README")).toBeVisible();
      // A second tap on the lower edge closes it again.
      await page.mouse.click(cx, bottomY);
      await expect(row.getByText("先读取 README")).toBeHidden();
    } finally {
      await ctx.close();
    }
  });
});
