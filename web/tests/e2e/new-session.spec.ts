import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

const here = path.dirname(fileURLToPath(import.meta.url));
/**
 * A default run must not rewrite tracked files, so shots land in the
 * gitignored `test-results/`. Re-capture the committed evidence with
 * REMUDA_EVIDENCE=1.
 */
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence
  ? path.join(here, "../../../docs/design/evidence")
  : path.join(here, "../../test-results/new-session");

/**
 * This spec runs under both Playwright configs: the default mock-backed
 * server (VITE_MOCK=1) and `playwright.hub.config.ts` (fake Hub + fake Node,
 * marked via REMUDA_E2E_BACKEND in that config). The legacy slider/30-second
 * cases are mock-backed; batch B adds hub-live cases for the unknown-ACK path
 * that need a real HTTP POST to lose.
 */
const hubLive = process.env.REMUDA_E2E_BACKEND === "hub";

async function shot(page: Page, name: string) {
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

/** The effort card only, so no personal path or hostname can land in a shot. */
async function shotEffort(page: Page, name: string) {
  await mkdir(shotDir, { recursive: true });
  const card = page.getByTestId("new-session-effort");
  await card.scrollIntoViewIfNeeded();
  const box = await card.boundingBox();
  expect(box).toBeTruthy();
  const viewport = page.viewportSize() ?? { width: 1440, height: 900 };
  const x = Math.max(0, Math.floor(box!.x) - 8);
  const y = Math.max(0, Math.floor(box!.y) - 8);
  await page.screenshot({
    path: path.join(shotDir, name),
    animations: "disabled",
    clip: {
      x,
      y,
      width: Math.max(1, Math.min(viewport.width - x, Math.ceil(box!.width) + 16)),
      height: Math.max(1, Math.min(viewport.height - y, Math.ceil(box!.height) + 16)),
    },
  });
}

/** The pill's proportions, shared with the composer: ~40px track, ~36px knob. */
async function assertPill(page: Page) {
  const pill = await page.getByTestId("new-session-effort-track").boundingBox();
  const knob = await page.getByTestId("new-session-effort-knob").boundingBox();
  const fill = await page.getByTestId("new-session-effort-fill").boundingBox();
  expect(pill).toBeTruthy();
  expect(knob).toBeTruthy();
  expect(fill).toBeTruthy();
  expect(pill!.height).toBeGreaterThanOrEqual(36);
  expect(pill!.height).toBeLessThanOrEqual(42);
  expect(knob!.width).toBeGreaterThanOrEqual(32);
  expect(knob!.width).toBeLessThanOrEqual(38);
  // The fill runs under the knob to its far edge — no bare track beside the thumb.
  expect(fill!.x + fill!.width).toBeGreaterThanOrEqual(knob!.x + knob!.width - 1);
  expect(fill!.x).toBeLessThanOrEqual(pill!.x + 1);
}

async function openSheet(page: Page) {
  if (hubLive) await login(page);
  await page.goto("/sessions/new");
  await expect(page.getByTestId("new-session-sheet")).toBeVisible();
  if (hubLive) {
    await expect(page.getByTestId("new-session-host")).toContainText("e2e-fake-node", { timeout: 20_000 });
  }
}

async function listInstanceIds(page: Page): Promise<string[]> {
  const res = await page.request.get("/v1/instances");
  expect(res.ok()).toBe(true);
  const body = (await res.json()) as { items?: Array<{ instanceId?: string; id?: string }> };
  return (body.items ?? []).map((item) => item.instanceId ?? item.id ?? "").filter(Boolean);
}

/**
 * Playwright cannot raise the platform soft keyboard, which is what actually
 * resizes `visualViewport`. Dispatch the same event with a keyboard-height
 * viewport so the app's --workbench-height path runs exactly as on a phone.
 */
async function emulateSoftKeyboard(page: Page, height: number) {
  await page.evaluate((nextHeight) => {
    const viewport = window.visualViewport;
    if (!viewport) return;
    Object.defineProperty(viewport, "height", { configurable: true, value: nextHeight });
    Object.defineProperty(viewport, "offsetTop", { configurable: true, value: 0 });
    viewport.dispatchEvent(new Event("resize"));
  }, height);
}

test.describe("new session sheet (mock-backed)", () => {
  test.beforeEach(() => {
    test.skip(hubLive, "mock-backed sheet/slider behaviour; hub-live cases run in their own describe");
  });

  test("mobile 30s path: focus prompt, type, start", async ({ page }) => {
    await page.goto("/sessions/new");
    const prompt = page.getByTestId("new-session-prompt");
    await expect(page.getByTestId("new-session-sheet")).toBeVisible();
    await expect(prompt).toBeFocused();
    await expect(page.getByTestId("new-session-perm-bypassPermissions")).toBeVisible();
    await expect(page.getByTestId("new-session-delegation-none")).toBeVisible();
    await page.getByTestId("new-session-perm-bypassPermissions").click();
    await expect(page.getByTestId("new-session-yolo-hint")).toBeVisible();
    await prompt.fill("查 bolt TaskManager spill");
    await expect(page.getByTestId("new-session-start")).toBeEnabled();
    await page.getByTestId("new-session-start").click();
    await expect(page).toHaveURL(/\/s\//);
    await expect(page.getByTestId("session-page")).toBeVisible();
    await expect(page.getByTestId("session-page")).toHaveAttribute("data-status", "starting");
    await expect(page.getByTestId("session-page").getByTestId("message")).toContainText("查 bolt TaskManager spill");
  });

  test("kind terminal uses shell-pty and opens the terminal tab", async ({ page }) => {
    await page.goto("/sessions/new");
    await expect(page.getByTestId("new-session-sheet")).toBeVisible();
    await page.getByTestId("new-session-kind-terminal").click();
    await expect(page.getByTestId("new-session-terminal-driver")).toContainText("shell-pty");
    await expect(page.getByTestId("new-session-start")).toBeEnabled();
    await page.getByTestId("new-session-start").click();
    await expect(page).toHaveURL(/\/s\//);
    await expect(page.getByTestId("session-page")).toHaveAttribute("data-driver", "shell-pty");
    await expect(page.getByTestId("session-page")).toHaveAttribute("data-view", "tty");
    await expect(page.locator("[data-tty-lab='1']")).toBeVisible();
    await expect(page.locator("[data-tty-ready='1']")).toBeVisible({ timeout: 15_000 });
    await expect(page.getByTestId("view-switch")).toHaveAttribute("data-view", "tty");
    await expect(page.getByTestId("view-switch-tty")).toHaveAttribute("aria-checked", "true");
    await expect(page.getByTestId("view-switch-structured")).toHaveAttribute("aria-checked", "false");
    if (test.info().project.name === "chromium") {
      await mkdir(shotDir, { recursive: true });
      await page.screenshot({ path: path.join(shotDir, "terminal-1-new-session.png"), animations: "disabled" });
    }
  });

  test("effort is the inline layout-A slider and the keyboard value reaches the created instance", async ({ page }) => {
    await page.goto("/sessions/new");
    await expect(page.getByTestId("new-session-sheet")).toBeVisible();
    const slider = page.getByTestId("new-session-effort-slider");
    // Frameless inline layout, not a popover and not a bordered card.
    await expect(page.getByTestId("new-session-effort-slider-panel")).toHaveAttribute("data-variant", "inline");
    await expect(slider).toHaveAttribute("role", "slider");
    await expect(slider).toHaveAttribute("data-tiers", "low,medium,high,xhigh,max,ultracode");
    await expect(slider).toHaveAttribute("aria-valuemax", "5");
    await expect(slider).toHaveAttribute("data-name", "high");
    await expect(slider).toHaveAttribute("data-index", "2");
    // The helper sits in the muted field slot under the pill, never floated.
    await expect(page.getByTestId("new-session-effort")).toContainText("会话开始后仍可在会话内调整");
    await assertPill(page);

    // Six full tick labels under the six stops (desktop; the narrow breakpoint
    // swaps in short labels via CSS only).
    const ticks = page.locator(
      "[data-testid='new-session-effort-slider-panel'] [class*='effortTickFull']",
    );
    await expect(ticks).toHaveText(["low", "medium", "high", "xhigh", "max", "ultracode"]);
    // The standalone ultracode chip is gone; it is the last tick.
    await expect(page.getByTestId("new-session-effort-ultracode")).toHaveCount(0);

    // Set the tier by keyboard alone, the way a discrete slider must answer.
    await slider.focus();
    await page.keyboard.press("Home");
    await expect(slider).toHaveAttribute("data-name", "low");
    await expect(slider).toHaveAttribute("aria-valuenow", "0");
    await page.keyboard.press("ArrowRight");
    await page.keyboard.press("ArrowRight");
    await page.keyboard.press("ArrowRight");
    await expect(slider).toHaveAttribute("data-name", "xhigh");
    await expect(page.getByTestId("new-session-effort-title")).toContainText("xhigh");
    // Plain xhigh is not the ember tier.
    await expect(slider).toHaveAttribute("data-ember", "0");
    await page.keyboard.press("ArrowRight");
    await expect(slider).toHaveAttribute("data-name", "max");
    await expect(slider).toHaveAttribute("data-index", "4");
    await expect(slider).toHaveAttribute("data-ember", "1");
    await expect(page.getByTestId("new-session-effort-embers")).toBeVisible();
    await assertPill(page);
    // One more stop: ultracode — the xhigh tier plus the workflow flag.
    await page.keyboard.press("ArrowRight");
    await expect(slider).toHaveAttribute("data-name", "ultracode");
    await expect(slider).toHaveAttribute("data-index", "5");
    await expect(slider).toHaveAttribute("data-tier-index", "3");
    await expect(slider).toHaveAttribute("data-ultracode", "1");
    await expect(page.getByTestId("new-session-effort-title")).toContainText("ultracode");

    // Back to xhigh so the asserted value is not simply the End default.
    await page.keyboard.press("ArrowLeft");
    await page.keyboard.press("ArrowLeft");
    await expect(slider).toHaveAttribute("data-name", "xhigh");
    await expect(page.getByTestId("new-session-effort")).toHaveAttribute("data-effort", "xhigh");

    await page.getByTestId("new-session-prompt").fill("effort by keyboard");
    await page.getByTestId("new-session-start").click();
    await expect(page).toHaveURL(/\/s\//);
    // The created instance carries the tier the slider was left on.
    await expect(page.getByTestId("composer")).toHaveAttribute("data-effort", "xhigh");
    await expect(page.getByTestId("composer")).toHaveAttribute("data-effort-index", "3");
    await expect(page.getByTestId("model-effort-chip")).toContainText("xhigh");
  });

  test("the ultracode stop creates with the ultracode wire name and reads ultracode", async ({ page }) => {
    await page.goto("/sessions/new");
    const slider = page.getByTestId("new-session-effort-slider");
    await slider.focus();
    await page.keyboard.press("End");
    // The sixth stop: enabled slider, xhigh tier + ultracode flag, ember on.
    await expect(slider).toHaveAttribute("data-ultracode", "1");
    await expect(slider).toHaveAttribute("data-name", "ultracode");
    await expect(slider).toHaveAttribute("data-tier-index", "3");
    await expect(slider).toHaveAttribute("data-index", "5");
    await expect(slider).toHaveAttribute("data-ember", "1");
    await expect(slider).toHaveAttribute("aria-disabled", "false");
    // The same slider walks back off the stop; nothing is locked.
    await page.keyboard.press("ArrowLeft");
    await expect(slider).toHaveAttribute("data-name", "max");
    await expect(slider).toHaveAttribute("data-ultracode", "0");
    await page.keyboard.press("ArrowRight");
    await expect(slider).toHaveAttribute("data-name", "ultracode");
    await expect(page.getByTestId("new-session-effort-embers")).toHaveAttribute(
      "data-intensity",
      "ultra",
    );

    await page.getByTestId("new-session-prompt").fill("ultracode run");
    await page.getByTestId("new-session-start").click();
    await expect(page).toHaveURL(/\/s\//);
    await expect(page.getByTestId("composer")).toHaveAttribute("data-effort", "ultracode");
    await expect(page.getByTestId("composer")).toHaveAttribute("data-ultracode", "1");
    await expect(page.getByTestId("model-effort-chip")).toContainText("ultracode");
  });

  test("the slider re-snaps onto the harness selected above", async ({ page }) => {
    await page.goto("/sessions/new");
    const slider = page.getByTestId("new-session-effort-slider");
    // One stop before End: max (End itself is the Claude-only ultracode stop).
    await slider.focus();
    await page.keyboard.press("End");
    await page.keyboard.press("ArrowLeft");
    await expect(slider).toHaveAttribute("data-name", "max");

    // grok's table is three tiers; the top tier stays the top tier.
    await page.getByTestId("new-session-kind-grok").click();
    await expect(slider).toHaveAttribute("data-tiers", "quick,standard,max");
    await expect(slider).toHaveAttribute("data-name", "max");
    await expect(slider).toHaveAttribute("aria-valuemax", "2");
    await expect(slider).toHaveAttribute("data-ember", "1");
    // ultracode is Claude-only: no sixth stop and no chip.
    await expect(page.getByTestId("new-session-effort-ultracode")).toHaveCount(0);
    await slider.focus();
    await page.keyboard.press("Home");
    await expect(slider).toHaveAttribute("data-name", "quick");

    // ...and back to claude, renamed onto the six-stop table, ember dropped.
    await page.getByTestId("new-session-kind-claude").click();
    await expect(slider).toHaveAttribute("data-tiers", "low,medium,high,xhigh,max,ultracode");
    await expect(slider).toHaveAttribute("data-name", "low");
    await expect(slider).toHaveAttribute("data-ember", "0");

    // terminal has no effort axis at all, so the whole field unmounts.
    await page.getByTestId("new-session-kind-terminal").click();
    await expect(page.getByTestId("new-session-effort")).toHaveCount(0);
  });

  test("the inline effort field spans the same column width as the permission row", async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    await page.goto("/sessions/new");
    const pill = page.getByTestId("new-session-effort-track");
    const permRow = page.getByTestId("new-session-perm-row");
    const field = page.getByTestId("new-session-effort");
    await expect(pill).toBeVisible();
    const a = await pill.boundingBox();
    const b = await permRow.boundingBox();
    const f = await field.boundingBox();
    expect(a).toBeTruthy();
    expect(b).toBeTruthy();
    expect(f).toBeTruthy();
    // The pill and the permission row share the form column edges (within a px).
    expect(Math.abs(a!.x - b!.x)).toBeLessThanOrEqual(1);
    expect(Math.abs(a!.x + a!.width - (b!.x + b!.width))).toBeLessThanOrEqual(1);
    // The spec helper sits below the pill in the same column, never to its right.
    const foot = page.getByTestId("new-session-effort-foot");
    const footBox = await foot.boundingBox();
    expect(footBox).toBeTruthy();
    expect(footBox!.y).toBeGreaterThan(a!.y + a!.height);
    expect(footBox!.x).toBeGreaterThanOrEqual(f!.x - 1);
  });

  test("new session slider evidence: night/ledger at 1440 and 400", async ({ page }, info) => {
    test.skip(info.project.name !== "chromium", "evidence shots from chromium only");
    await page.emulateMedia({ reducedMotion: "reduce" });
    for (const theme of ["night", "ledger"] as const) {
      for (const [width, height, tag] of [
        [1440, 900, "1440"],
        [400, 844, "400"],
      ] as const) {
        await page.setViewportSize({ width, height });
        await page.goto("/sessions/new");
        await page.evaluate((next) => {
          document.documentElement.setAttribute("data-theme", next);
        }, theme);
        const slider = page.getByTestId("new-session-effort-slider");
        await expect(slider).toBeVisible();
        // mid = plain xhigh, max = the ember tier, ultra = the ultracode stop.
        for (const [state, keys] of [
          ["mid", ["Home", "ArrowRight", "ArrowRight", "ArrowRight"]],
          ["max", ["End", "ArrowLeft"]],
          ["ultra", ["End"]],
        ] as const) {
          await slider.focus();
          for (const key of keys) await page.keyboard.press(key);
          await expect(slider).toHaveAttribute(
            "data-name",
            state === "mid" ? "xhigh" : state === "max" ? "max" : "ultracode",
          );
          await expect(slider).toHaveAttribute("data-ember", state === "mid" ? "0" : "1");
          await assertPill(page);
          // The knob stays reachable at 400px.
          const knob = page.getByTestId("new-session-effort-knob");
          await expect(knob).toBeVisible();
          await shotEffort(page, `composer-slider-5-new-${state}-${theme}-${tag}.png`);
        }
      }
    }
  });

  test("ultracode evidence: the sixth stop at desktop and 400, night theme", async ({ page }, info) => {
    test.skip(info.project.name !== "chromium", "evidence shots from chromium only");
    await page.emulateMedia({ reducedMotion: "reduce" });
    for (const [width, height, tag] of [
      [1440, 900, "1440"],
      [400, 844, "400"],
    ] as const) {
      await page.setViewportSize({ width, height });
      await page.goto("/sessions/new");
      await page.evaluate(() => document.documentElement.setAttribute("data-theme", "night"));
      const slider = page.getByTestId("new-session-effort-slider");
      await slider.focus();
      await page.keyboard.press("End");
      await expect(slider).toHaveAttribute("data-name", "ultracode");
      await expect(slider).toHaveAttribute("data-index", "5");
      await expect(slider).toHaveAttribute("data-tier-index", "3");
      await expect(slider).toHaveAttribute("data-ultracode", "1");
      await expect(slider).toHaveAttribute("data-ember", "1");
      await expect(page.getByTestId("new-session-effort-embers")).toHaveAttribute(
        "data-intensity",
        "ultra",
      );
      await assertPill(page);
      await shotEffort(page, `composer-slider-5-new-ultra-night-${tag}.png`);
    }
  });
});

test.describe("batch B: origin, draft, vocabulary, keyboard", () => {
  test.beforeEach(() => {
    test.skip(hubLive, "covered against the mock backend");
  });

  test("Escape keeps the draft and reopening restores it; 丢弃草稿 removes it", async ({ page }) => {
    await page.goto("/sessions");
    await page.getByRole("link", { name: "新建" }).first().click();
    await expect(page.getByTestId("new-session-sheet")).toBeVisible();
    await page.getByTestId("new-session-prompt").fill("先查一半，等会接着写");
    // Escape closes through the focus-trap contract and returns to the origin.
    await page.keyboard.press("Escape");
    await expect(page).toHaveURL(/\/sessions$/);
    await expect(page.getByTestId("new-session-sheet")).toHaveCount(0);

    // Reopen: same host/workspace context restores the body.
    await page.goto("/sessions/new");
    await expect(page.getByTestId("new-session-prompt")).toHaveValue("先查一半，等会接着写");

    // The explicit discard path is the only one that erases the text.
    await page.getByTestId("new-session-discard").click();
    await expect(page).toHaveURL(/\/sessions$/);
    await page.goto("/sessions/new");
    await expect(page.getByTestId("new-session-prompt")).toHaveValue("");
  });

  test("the first layer uses user vocabulary; driver words live behind 高级设置", async ({ page }) => {
    await page.goto("/sessions/new");
    const sheet = page.getByTestId("new-session-sheet");
    await expect(sheet).toBeVisible();
    await expect(sheet).toContainText("要做什么");
    await expect(sheet).toContainText("工作目录");
    await expect(sheet).toContainText("执行 agent");
    await expect(sheet).toContainText("权限");
    await expect(sheet).toContainText("模型来源");
    for (const word of ["InstanceSpec", "shell-pty", "claude-print", "generic-pty", "carrier"]) {
      await expect(sheet).not.toContainText(word);
    }
    // The carrier matrix is still reachable, one disclosure away.
    await expect(page.getByTestId("new-session-driver-row")).toHaveCount(0);
    await page.getByTestId("new-session-advanced").click();
    await expect(page.getByTestId("new-session-driver-row")).toBeVisible();
    await expect(sheet).toContainText("shell-pty");
  });

  test("completes the whole create by keyboard alone (chromium)", async ({ page }, info) => {
    // Keyboard tab traversal is the desktop interaction; the 390px case
    // covers the touch surface separately.
    test.skip(info.project.name !== "chromium", "keyboard traversal on desktop chromium");
    await openSheet(page);
    const prompt = page.getByTestId("new-session-prompt");
    await expect(prompt).toBeFocused();
    await prompt.fill("纯键盘完成创建");
    // prompt -> close (first focusable) -> wraps to the last focusable, 开始.
    await page.keyboard.press("Shift+Tab");
    await page.keyboard.press("Shift+Tab");
    await expect(page.getByTestId("new-session-start")).toBeFocused();
    await page.keyboard.press("Enter");
    await expect(page).toHaveURL(/\/s\//);
    await expect(page.getByTestId("session-page")).toBeVisible();
    await expect(page.getByTestId("session-page").getByTestId("message")).toContainText("纯键盘完成创建");
  });

  test("390px: the primary action stays above the emulated soft keyboard", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await openSheet(page);
    await page.getByTestId("new-session-prompt").fill("手机软键盘不遮挡开始");
    // Keyboard opens: visual viewport loses ~340px (iPhone 13-class keyboard).
    await emulateSoftKeyboard(page, 504);
    const start = page.getByTestId("new-session-start");
    await expect(start).toBeVisible();
    const box = await start.boundingBox();
    expect(box).toBeTruthy();
    // The unobscured region is y 0..504; the whole primary button sits in it.
    expect(box!.y).toBeGreaterThanOrEqual(0);
    expect(box!.y + box!.height).toBeLessThanOrEqual(510);
    // And it remains the primary action, not pushed off-screen or disabled.
    expect(await start.isEnabled()).toBe(true);
    await shot(page, "workbench-b-newsession-keyboard-390.png");
    // Closing the keyboard re-grows the sheet without hiding the action.
    await emulateSoftKeyboard(page, 844);
    const after = await start.boundingBox();
    expect(after).toBeTruthy();
    expect(after!.y + after!.height).toBeLessThanOrEqual(848);
  });

  test("batch B evidence: first layer and advanced at 1440/390", async ({ page }, info) => {
    test.skip(!evidence, "set REMUDA_EVIDENCE=1 to refresh committed evidence");
    test.skip(info.project.name !== "chromium", "evidence shots from chromium only");
    await page.emulateMedia({ reducedMotion: "reduce" });
    for (const theme of ["night", "ledger"] as const) {
      for (const [width, height, tag] of [
        [1440, 900, "1440"],
        [390, 844, "390"],
      ] as const) {
        await page.setViewportSize({ width, height });
        await page.goto("/sessions/new");
        await page.evaluate((next) => document.documentElement.setAttribute("data-theme", next), theme);
        await page.getByTestId("new-session-prompt").fill("示例：整理 worktree 创建失败的重试路径");
        await shot(page, `workbench-b-newsession-layer1-${theme}-${tag}.png`);
        await page.getByTestId("new-session-advanced").click();
        const driverRow = page.getByTestId("new-session-driver-row");
        await expect(driverRow).toBeVisible();
        await driverRow.scrollIntoViewIfNeeded();
        await shot(page, `workbench-b-newsession-advanced-${theme}-${tag}.png`);
      }
    }
  });
});

test.describe("batch B against the fake Hub", () => {
  test.beforeEach(() => {
    test.skip(!hubLive, "requires the fake Hub from playwright.hub.config.ts");
  });

  const created: string[] = [];
  test.afterEach(async ({ page }) => {
    // The fake node caps instances at 8 for the whole serial suite; release
    // every slot this spec occupies. force=1 is a u8 (force=true 400s).
    for (const id of created.splice(0)) {
      await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    }
  });

  test("creates one instance by keyboard and returns into it", async ({ page }) => {
    await openSheet(page);
    const prompt = page.getByTestId("new-session-prompt");
    await expect(prompt).toBeFocused();
    await prompt.fill("hub keyboard create");
    await page.keyboard.press("Shift+Tab");
    await page.keyboard.press("Shift+Tab");
    await expect(page.getByTestId("new-session-start")).toBeFocused();
    const creating = page.waitForResponse(
      (response) =>
        response.request().method() === "POST" && new URL(response.url()).pathname === "/v1/instances",
    );
    await page.keyboard.press("Enter");
    const response = await creating;
    expect(response.ok()).toBe(true);
    const body = (await response.json()) as { instance?: { instanceId?: string } };
    const instanceId = body.instance?.instanceId;
    expect(instanceId).toBeTruthy();
    created.push(instanceId!);
    await expect(page).toHaveURL(`/s/${instanceId}`, { timeout: 20_000 });
  });

  test("unknown ACK: shows 状态待确认 and never creates a second instance", async ({ page }) => {
    await openSheet(page);
    const before = await listInstanceIds(page);
    let posts = 0;
    await page.route("**/v1/instances", async (route) => {
      if (route.request().method() !== "POST") {
        await route.continue();
        return;
      }
      posts += 1;
      // Let the real Hub (and fake Node) create the instance, then lose the
      // response: this is the unknown-ACK condition — the client must not
      // assume either success or failure.
      const serverResponse = await route.fetch();
      await page.waitForTimeout(300);
      await route.fulfill({
        response: serverResponse,
        status: 504,
        contentType: "application/json",
        body: JSON.stringify({ code: "GATEWAY_ACK_LOST", error: "create ACK lost" }),
      });
    });

    await page.getByTestId("new-session-prompt").fill("unknown ack probe");
    await page.getByTestId("new-session-start").click();

    const unknown = page.getByTestId("new-session-unknown");
    await expect(unknown).toBeVisible({ timeout: 20_000 });
    await expect(unknown).toHaveAttribute("role", "status");
    await expect(unknown).toContainText("状态待确认");
    // Exactly one POST; the sheet stays open instead of navigating.
    expect(posts).toBe(1);
    await expect(page).toHaveURL(/\/sessions\/new/);
    await expect(page.getByTestId("new-session-start")).toBeDisabled();
    await expect(page.getByTestId("new-session-client-request-id")).toContainText("creq_");

    // The reconciliation action is read-only: one GET round, no second POST.
    await page.getByTestId("new-session-check").click();
    await expect(page.getByTestId("new-session-check-note")).toBeVisible();
    await page.waitForTimeout(500);
    expect(posts).toBe(1);

    // Server-side, the one ambiguous request materialized exactly one instance.
    const after = await listInstanceIds(page);
    const added = after.filter((id) => !before.includes(id));
    expect(added).toHaveLength(1);
    created.push(added[0]);
    if (evidence) {
      await unknown.scrollIntoViewIfNeeded();
      await shot(page, "workbench-b-newsession-unknown-ack-1440.png");
    }
  });

  test("390px soft keyboard: primary action stays visible against the fake Hub", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await openSheet(page);
    await page.getByTestId("new-session-prompt").fill("hub keyboard geometry");
    await emulateSoftKeyboard(page, 504);
    const start = page.getByTestId("new-session-start");
    await expect(start).toBeVisible();
    const box = await start.boundingBox();
    expect(box).toBeTruthy();
    expect(box!.y).toBeGreaterThanOrEqual(0);
    expect(box!.y + box!.height).toBeLessThanOrEqual(510);
  });
});
