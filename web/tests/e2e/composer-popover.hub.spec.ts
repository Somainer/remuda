import { expect, test, type Page } from "@playwright/test";

/**
 * c-composerpop: the context chip's hover-close timer must not kill the
 * effort/permission menus (RC2), the chip opens an empty-state card with no
 * rollup (RC3), and menus stay hit-testable above the notification stack.
 *
 * Drives the in-process fake node. Fake-node sessions carry no usage rollup
 * (the current live shape), which is exactly the RC3 empty case.
 */
test.describe.configure({ mode: "serial" });

type NotifyLab = {
  notify: (input: { kind?: "info" | "blocking"; title?: string; text?: string }) => string;
  dismissAllBlocking: () => void;
};

const created: string[] = [];

async function cleanup(page: Page) {
  for (const id of created.splice(0)) {
    await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => {});
  }
}

async function createSession(page: Page, prompt: string): Promise<string> {
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  await expect(hostPicker).toContainText("e2e-fake-node", { timeout: 20_000 });
  const host = await hostPicker.locator("option").filter({ hasText: "e2e-fake-node" }).getAttribute("value");
  expect(host).toBeTruthy();
  hostPicker.selectOption(host!);
  await page.getByTestId("new-session-prompt").fill(prompt);
  await page.getByTestId("new-session-start").click();
  await page.waitForURL(/\/s\//, { timeout: 20_000 });
  const id = new URL(page.url()).pathname.split("/").pop()!;
  created.push(id);
  return id;
}

test.beforeEach(async ({ page }) => {
  await page.goto("http://127.0.0.1:58880/v2/devices/pair");
  await expect(page.getByTitle("新建")).toBeVisible({ timeout: 20_000 });
  await page.getByTitle("新建", { exact: true }).click();
  const code = await page.evaluate(async () => {
    const res = await fetch("/v1/login", { method: "POST" });
    return (await res.json()) as { deviceId?: string };
  });
  await page.evaluate((id) => {
    document.cookie = `device=${id}; path=/`;
  }, code.deviceId ?? "");
  await page.reload();
});

test.describe("context chip does not close other menus (RC2)", () => {
  test("crossing the context chip keeps the permission menu open and topmost", async ({ page }) => {
    await createSession(page, "composer popover permission");
    await page.clock.install();
    await page.getByTestId("permission-chip").click();
    const permMenu = page.getByTestId("permission-menu");
    await expect(permMenu).toBeVisible();

    // Move across the context chip and dwell, then on to the lower-left row.
    const chipBox = (await page.getByTestId("context-chip").boundingBox())!;
    await page.mouse.move(chipBox.x + 4, chipBox.y + chipBox.height / 2, { steps: 8 });
    const row = permMenu.locator("[data-testid^='permission-option-']").last();
    const rowBox = (await row.boundingBox())!;
    await page.mouse.move(rowBox.x + 8, rowBox.y + rowBox.height - 6, { steps: 12 });
    await page.clock().runFor("00:00:30");

    await expect(permMenu).toBeVisible();
    // The bottom row is hit-testable on the menu, not dismissed.
    await row.click({ trial: true });
    await expect(permMenu).toBeVisible();
    await cleanup(page);
  });

  test("resting on the context chip then clicking effort keeps effort open", async ({ page }) => {
    await createSession(page, "composer popover effort");
    await page.clock.install();
    await page.getByTestId("context-chip").hover();
    await page.clock().runFor("00:00:01");
    await page.getByTestId("model-effort-chip").click();
    const panel = page.getByTestId("effort-slider-panel");
    await expect(panel).toBeVisible();
    await page.clock().runFor("00:00:30");
    await expect(panel).toBeVisible();
    await cleanup(page);
  });
});

test.describe("context chip empty state (RC3)", () => {
  test("desktop click opens an explanatory empty-state card with no rollup", async ({ page }) => {
    await createSession(page, "composer popover empty desktop");
    await expect(page.getByTestId("context-chip")).toHaveTextContent("—");
    await page.getByTestId("context-chip").click();
    await expect(page.getByTestId("context-usage-empty-note")).toBeVisible();
    await cleanup(page);
  });

  test("desktop hover opens the empty-state card with no rollup", async ({ page }) => {
    await createSession(page, "composer popover empty hover");
    await page.getByTestId("context-chip").hover();
    await expect(page.getByTestId("context-usage-empty-note")).toBeVisible();
    await cleanup(page);
  });

  test("mobile 390: tapping the context row in the sheet shows the explanation", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await createSession(page, "composer popover empty mobile");
    await page.getByTestId("composer-options-trigger").click();
    await page.getByTestId("context-chip").click();
    await expect(page.getByTestId("context-usage-empty-note")).toBeVisible();
    await cleanup(page);
  });
});

test.describe("notification stack does not cover menus (item 8)", () => {
  test("open menus stay hit-testable above a blocking notification", async ({ page }) => {
    await createSession(page, "composer popover zorder");
    await page.evaluate(() => {
      const lab = (window as unknown as { __notifyLab?: NotifyLab }).__notifyLab;
      lab?.notify({ kind: "blocking", title: "blocking test", text: "standing error" });
    });
    await expect(page.getByText("standing error")).toBeVisible();

    // Permission menu bottom row is clickable even with the blocking stack up.
    await page.getByTestId("permission-chip").click();
    const permMenu = page.getByTestId("permission-menu");
    await expect(permMenu).toBeVisible();
    const permRow = permMenu.locator("[data-testid^='permission-option-']").first();
    await permRow.hover();
    const permBox = (await permRow.boundingBox())!;
    const onPerm = await page.evaluate((b) => {
      const el = document.elementFromPoint(b.x + b.width / 2, b.y + b.height / 2);
      return !!el?.closest("[data-testid='permission-menu']");
    }, permBox);
    expect(onPerm).toBe(true);
    await page.keyboard.press("Escape");

    // Effort slider panel likewise.
    await page.getByTestId("model-effort-chip").click();
    const slider = page.getByTestId("effort-slider");
    await expect(slider).toBeVisible();
    const sBox = (await slider.boundingBox())!;
    const onSlider = await page.evaluate((b) => {
      const el = document.elementFromPoint(b.x + b.width / 2, b.y + b.height / 2);
      return !!el?.closest("[data-testid='effort-slider'], [data-testid='effort-slider-panel']");
    }, sBox);
    expect(onSlider).toBe(true);

    await page.evaluate(() => {
      (window as unknown as { __notifyLab?: NotifyLab }).__notifyLab?.dismissAllBlocking();
    });
    await cleanup(page);
  });
});
