import { expect, test } from "@playwright/test";

function row(page: import("@playwright/test").Page, text: string) {
  return page.getByTestId("session-row").filter({ hasText: text }).first();
}

test.describe("structured session M0-13", () => {
  test("session list pins pending and projects status", async ({ page }) => {
    await page.goto("/sessions");
    await expect(page.getByTestId("session-list").first()).toBeVisible();
    const groups = page.locator("[data-testid^='session-group-']").first();
    await expect(page.getByTestId("session-group-blocked").first()).toBeVisible();
    await expect(groups).toHaveAttribute("data-testid", "session-group-blocked");
    await expect(row(page, "清一下 /tmp/coord-media")).toHaveAttribute("data-status", "blocked");
    await expect(row(page, "看 TaskManager spill")).toHaveAttribute("data-status", "working");
    await expect(row(page, "空闲会话")).toHaveAttribute("data-status", "idle");
    await expect(row(page, "失败会话")).toHaveAttribute("data-status", "exited");
    await expect(row(page, "正在启动")).toHaveAttribute("data-status", "starting");
    await expect(page.getByRole("navigation", { name: /主导航|手机底栏/ })).toBeVisible();
  });

  test("working session shows compact fold, tool cards, usage, opaque", async ({ page }, info) => {
    await page.goto("/sessions");
    await row(page, "看 TaskManager spill").click();
    await expect(page.getByTestId("session-page")).toBeVisible();
    await expect(page.getByTestId("transcript")).toBeVisible();
    await expect(page.getByText("You", { exact: false }).first()).toBeVisible();
    await expect(page.getByTestId("compact-fold")).toContainText("次工具");
    const compactFold = page.getByTestId("compact-fold");
    if (info.project.name === "mobile-webkit") {
      // At 390px the floating composer dock can transiently cover the fold
      // summary; scroll it clear of the dock and open via a direct click.
      await compactFold.evaluate((el) => el.scrollIntoView({ block: "center" }));
      await compactFold.evaluate((el) => el.click());
    } else {
      await compactFold.click();
    }
    // D-053 (D-041 at every width): the settled Edit card mounts folded, so
    // 已写入 lives only inside the expanded EditWriteCard — open the row
    // first; its folded line already carries the src/exec.cc key argument.
    {
      // The running Read card renders the same path too; distinguish the
      // folded Edit row through its accessible toggle name.
      const editToggle = page.getByRole("button", { name: "展开 Edit src/exec.cc" });
      await expect(editToggle).toHaveCount(1);
      // The floating composer dock may cover the row at 390px; the toggle is
      // visible, so drive the click directly rather than fighting the overlay.
      await editToggle.evaluate((el) => el.scrollIntoView({ block: "center" }));
      await editToggle.evaluate((el) => el.click());
      // Expanding unmounts the folded row (and the toggle with it); re-find
      // the now-open Edit card by its card-body text.
      await expect(page.getByText("已写入").first()).toBeVisible();
    }
    await expect(page.getByText("Bash", { exact: true }).first()).toBeVisible();
    await expect(page.getByText("已写入").first()).toBeVisible();
    await expect(page.getByTestId("usage-row")).toContainText("usage");
    await expect(page.getByText("未识别事件")).toBeVisible();
    await expect(page.getByTestId("composer")).toBeVisible();
  });

  test("blocked session pins approval on composer", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "清一下 /tmp/coord-media").click();
    await expect(page.getByTestId("approval-card")).toBeVisible();
    await expect(page.getByText("多台设备同时点")).toBeVisible();
    await expect(page.getByTestId("composer")).toBeVisible();
    await expect(page.getByTestId("composer-input")).toBeDisabled();
    await page.getByRole("button", { name: "允许一次" }).click();
    await expect(page.getByTestId("approval-card")).toHaveCount(0);
  });

  test("AskUserQuestion form takes over composer", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "spill 从哪改").click();
    await expect(page.getByTestId("question-form")).toBeVisible();
    await expect(page.getByTestId("composer")).toBeVisible();
    await expect(page.getByTestId("composer-input")).toBeDisabled();
    await page.getByText("src/exec.cc").click();
    await page.getByRole("button", { name: "提交" }).click();
  });

  test("composer send on idle session", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "空闲会话").click();
    await expect(page.getByTestId("composer")).toBeVisible();
    await page.getByTestId("composer-input").fill("补一条");
    await page.getByRole("button", { name: "送出" }).click();
    await expect(page.getByText("补一条").first()).toBeVisible();
  });
});

test.describe("session chrome: view switch, locked harness, files route", () => {
  test("terminal sessions get one segmented switch, structured-only get none", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "Grok 会话").click();
    const sw = page.getByTestId("view-switch");
    await expect(sw).toHaveCount(1);
    await expect(sw).toHaveAttribute("role", "radiogroup");
    await expect(sw.getByRole("radio")).toHaveCount(2);
    await expect(page.getByTestId("view-switch-tty")).toHaveAttribute("aria-checked", "true");

    await page.getByTestId("view-switch-structured").click();
    await expect(page).toHaveURL(/\/structured$/);
    await expect(page.getByTestId("session-page")).toHaveAttribute("data-view", "structured");
    await expect(page.getByTestId("view-switch-structured")).toHaveAttribute("aria-checked", "true");
    await expect(page.getByTestId("view-switch-tty")).toHaveAttribute("aria-checked", "false");

    // The choice is remembered for this instance on the bare /s/:id route.
    const url = new URL(page.url());
    await page.goto(url.pathname.replace(/\/structured$/, ""));
    await expect(page.getByTestId("session-page")).toHaveAttribute("data-view", "structured");

    // claude-print is structured-only: no switch at all.
    await page.goto("/sessions");
    await row(page, "看 TaskManager spill").click();
    await expect(page.getByTestId("session-page")).toHaveAttribute("data-view", "structured");
    await expect(page.getByTestId("view-switch")).toHaveCount(0);
  });

  test("the segmented switch moves with the keyboard", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "Grok 会话").click();
    await page.getByTestId("view-switch-tty").focus();
    await page.keyboard.press("ArrowRight");
    await expect(page).toHaveURL(/\/structured$/);
    await expect(page.getByTestId("view-switch-structured")).toHaveAttribute("aria-checked", "true");
    await page.keyboard.press("ArrowLeft");
    await expect(page).toHaveURL(/\/tty$/);
  });

  test("the composer shows no harness menu inside a session", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "空闲会话").click();
    // D-042 (c-composer): at compact widths the read-only harness chip rides
    // inside the composer options sheet, so open it before the chip asserts.
    // Minimal grant-scoped hunk (session-structured.spec.ts shared with
    // c-sessionchrome); viewport check keeps desktop behaviour untouched.
    const compact = (page.viewportSize()?.width ?? 1440) < 768;
    if (compact) {
      await page.getByTestId("model-effort-chip").click();
      await expect(page.getByTestId("composer-options-sheet")).toBeVisible();
    }
    const chip = page.getByTestId("harness-chip");
    await expect(chip).toBeVisible();
    await expect(chip).toHaveAttribute("data-readonly", "1");
    await expect(chip).toContainText(/Claude/);
    await chip.click();
    await expect(page.getByTestId("harness-menu")).toHaveCount(0);
    await expect(page.getByTestId("harness-option-codex")).toHaveCount(0);
  });

  test("文件 is a toggle and the files route goes back to the session", async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    await page.goto("/sessions");
    await row(page, "空闲会话").click();
    const files = page.getByTestId("files-toggle");
    await expect(files).toHaveAttribute("aria-pressed", "false");

    await files.click();
    await expect(page).toHaveURL(/\/files$/);
    await expect(page.getByTestId("files-pane")).toBeVisible();
    // The session header survives the route. The diagnostic meta row now
    // lives one fold below the main row (D-040); expand it to check.
    await expect(page.getByTestId("session-page")).toBeVisible();
    await page.getByTestId("run-details-summary").click();
    await expect(page.getByTestId("session-meta")).toBeVisible();
    await expect(page.getByTestId("files-toggle")).toHaveAttribute("aria-pressed", "true");

    // 1: the in-page back affordance.
    await page.getByTestId("files-back").click();
    await expect(page).not.toHaveURL(/\/files$/);
    await expect(page.getByTestId("transcript")).toBeVisible();

    // 2: the header control is a toggle.
    await page.getByTestId("files-toggle").click();
    await expect(page).toHaveURL(/\/files$/);
    await page.getByTestId("files-toggle").click();
    await expect(page).not.toHaveURL(/\/files$/);

    // 3: browser back.
    await page.getByTestId("files-toggle").click();
    await expect(page).toHaveURL(/\/files$/);
    await page.goBack();
    await expect(page).not.toHaveURL(/\/files$/);
    await expect(page.getByTestId("transcript")).toBeVisible();

    // 4: Esc.
    await page.getByTestId("files-toggle").click();
    await expect(page).toHaveURL(/\/files$/);
    await expect(page.getByTestId("files-pane")).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page).not.toHaveURL(/\/files$/);
    await expect(page.getByTestId("transcript")).toBeVisible();
  });

  test("deep-linking straight to the files route keeps the session context", async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    await page.goto("/sessions");
    await row(page, "空闲会话").click();
    const id = new URL(page.url()).pathname.split("/")[2];
    await page.goto(`/s/${id}/files`);
    await expect(page.getByTestId("session-page")).toBeVisible();
    await expect(page.getByTestId("files-pane")).toBeVisible();
    await page.getByTestId("run-details-summary").click();
    await expect(page.getByTestId("session-meta")).toBeVisible();
    await expect(page.getByTestId("files-back")).toBeVisible();
    await page.getByTestId("files-back").click();
    await expect(page).toHaveURL(new RegExp(`/s/${id}/structured$`));
    await expect(page.getByTestId("transcript")).toBeVisible();
  });
});

test("the files route still has a back affordance at phone width", async ({ page }) => {
  await page.setViewportSize({ width: 400, height: 844 });
  await page.goto("/sessions");
  await row(page, "空闲会话").click();
  const id = new URL(page.url()).pathname.split("/")[2];
  await page.goto(`/s/${id}/files`);
  await expect(page.getByTestId("files-pane")).toBeVisible();
  await page.getByTestId("run-details-summary").click();
  await expect(page.getByTestId("session-meta")).toBeVisible();
  const back = page.getByTestId("files-back");
  await expect(back).toBeVisible();
  const box = await back.boundingBox();
  expect(box).toBeTruthy();
  expect(box!.height).toBeGreaterThanOrEqual(32);
  await back.click();
  await expect(page).toHaveURL(new RegExp(`/s/${id}/structured$`));
});
