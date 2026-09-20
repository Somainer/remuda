import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * c-mkeybar (mobile-ui plan task 6, B.3.2 / ui-spec §4.7): the compact
 * /s/:id/tty bottom strip is the nine-key action bar.
 *
 * 390px:
 *  - nine keys in B.3.2 order; each is a real >=44px box whose four hit-zone
 *    corners resolve to itself (ux-touchhit geometry probe), and the raw
 *    byte keys (arrows / PgUp / PgDn / alt / ⌃C) stay reachable on the 键
 *    second row with the same tty-key-* testids;
 *  - the git key does a /files round trip and returns to the same live
 *    terminal (the xterm unmounts across the route change; see evidence §5
 *    for the scrolled-line-restore caveat in headless xterm v6);
 *  - the 结构 key and the top-bar ViewSwitch share one navigation, so URL and
 *    viewPref agree.
 * 1440px: regression only — no phone bar; the desktop toolbar AuxKeys keep
 * all eleven keys exactly as before.
 *
 * Geometry — not computed styles — is the contract (same rationale as
 * ux-touchhit.hub.spec.ts).
 */
test.describe.configure({ mode: "serial" });

const TOUCH = 44;
const HALF = TOUCH / 2;

const NINE = ["ctrl", "esc", "tab", "git", "jump", "clip", "history", "view", "keyboard"] as const;
const RAW_KEYS = ["esc", "tab", "ctrl", "alt", "up", "down", "left", "right", "pgup", "pgdn", "ctrl-c"] as const;

const created: string[] = [];

const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence
  ? path.join(here, "../../../docs/design/evidence")
  : path.join(here, "../../test-results/evidence");

async function shot(page: Page, name: string) {
  if (!evidence) return;
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

async function resolveHost(page: Page): Promise<string> {
  const response = await page.request.get("/v1/hosts");
  expect(response.ok()).toBe(true);
  const body = (await response.json()) as { items?: { id?: string; label?: string }[] };
  const host = (body.items ?? []).find((row) => row.label === "e2e-fake-node") ?? body.items?.[0];
  expect(host?.id, "fake node host").toBeTruthy();
  return host!.id!;
}

async function createTerminal(page: Page): Promise<string> {
  const hostId = await resolveHost(page);
  const response = await page.request.post("/v1/instances", {
    data: { hostId, workspaceId: "wsp_e2e", kind: "terminal", driver: "shell-pty", prompt: "m keybar strip" },
  });
  expect(response.ok(), `create terminal: ${response.status()} ${await response.text()}`).toBe(true);
  const body = (await response.json()) as { instance: { instanceId?: string; id?: string } };
  const id = body.instance.instanceId ?? body.instance.id;
  expect(id).toBeTruthy();
  created.push(id!);
  return id!;
}

async function patchCap(page: Page, value: number): Promise<void> {
  const response = await page.request.get("/v1/hosts");
  const body = (await response.json()) as {
    items?: { id?: string; maxInstances?: number; label?: string }[];
  };
  const host = (body.items ?? []).find((row) => row.label === "e2e-fake-node") ?? body.items?.[0];
  if (host?.id && (host.maxInstances ?? 8) < value) {
    await page.request.patch(`/v1/hosts/${host.id}`, { data: { maxInstances: value } });
  }
}

async function forceDeleteAllInstances(page: Page) {
  await page.evaluate(async () => {
    const body = (await (await fetch("/v1/instances", { credentials: "include" })).json()) as {
      items?: { instanceId?: string; id?: string }[];
    };
    await Promise.all(
      (body.items ?? []).map((i) =>
        fetch(`/v1/instances/${i.instanceId ?? i.id}?force=1`, {
          method: "DELETE",
          credentials: "include",
        }).catch(() => undefined),
      ),
    );
  });
}

/**
 * Scroll the key into the horizontal strip, then prove its box owns all four
 * corners of a centred 44×44 zone. Adjacent hot areas cannot resolve at a
 * corner (D-039), so non-overlap is tested, not just min-width.
 */
async function assertHitGeometry(page: Page, selector: string): Promise<void> {
  await page.evaluate((sel) => {
    const el = document.querySelector<HTMLElement>(sel);
    if (!el) throw new Error(`geometry target missing: ${sel}`);
    el.scrollIntoView({ block: "nearest", inline: "center" });
  }, selector);
  await page.evaluate(({ sel, half }) => {
    const el = document.querySelector<HTMLElement>(sel);
    if (!el) throw new Error(`geometry target missing: ${sel}`);
    const rect = el.getBoundingClientRect();
    if (rect.width < 44 - 0.5 || rect.height < 44 - 0.5) {
      throw new Error(`${sel}: box ${rect.width.toFixed(1)}x${rect.height.toFixed(1)} < 44px`);
    }
    const vw = window.innerWidth;
    const vh = window.innerHeight;
    const inside = (x: number, y: number) => x >= 0 && y >= 0 && x <= vw && y <= vh;
    if (!inside(rect.left, rect.top) || !inside(rect.right, rect.bottom)) {
      throw new Error(`${sel}: visual box outside ${vw}x${vh}`);
    }
    const cx = rect.left + rect.width / 2;
    const cy = rect.top + rect.height / 2;
    const corners = [
      ["tl", cx - half + 0.5, cy - half + 0.5],
      ["tr", cx + half - 0.5, cy - half + 0.5],
      ["bl", cx - half + 0.5, cy + half - 0.5],
      ["br", cx + half - 0.5, cy + half - 0.5],
    ] as const;
    for (const [name, x, y] of corners) {
      if (!inside(x, y)) throw new Error(`${sel}: ${name} corner outside viewport`);
      const hit = document.elementFromPoint(x, y)?.closest("button");
      if (hit !== el) {
        throw new Error(
          `${sel}: ${name} corner resolved to "${hit?.getAttribute("data-testid") ?? hit?.tagName ?? "none"}"`,
        );
      }
    }
  }, { sel: selector, half: HALF });
}

async function gotoTty(page: Page, id: string) {
  await page.goto(`/s/${id}/tty`);
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-view", "tty");
  await expect(page.locator("[data-tty-ready='1']")).toBeVisible({ timeout: 20_000 });
  await expect(page.locator(".xterm")).toBeVisible();
}

test.describe("390px compact nine-key bar", () => {
  // hasTouch without isMobile, same context shape as ux-touchhit: the
  // Playwright mobile UA emulation reserves a sub-pixel dead strip on the
  // very bottom edge (elementFromPoint resolves null for the last 0.5px of
  // the viewport even when every box reports bottom=innerHeight), which the
  // four-corner probe cannot certify. Compact layout is width-driven
  // (COMPACT_WORKBENCH_QUERY) and the local-dock default keys off the coarse
  // pointer, so geometry and behaviour are identical to a real phone.
  test.use({ viewport: { width: 390, height: 844 }, hasTouch: true });

  test.beforeEach(async ({ page }) => {
    await login(page);
  });

  test.afterEach(async ({ page }) => {
    for (const id of created.splice(0)) {
      await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    }
  });

  test.beforeAll(async ({ browser }) => {
    const setup = await browser.newPage();
    await login(setup);
    await patchCap(setup, 24);
    await setup.close();
  });

  test.afterAll(async ({ browser }) => {
    const cleanup = await browser.newPage();
    await login(cleanup);
    await forceDeleteAllInstances(cleanup).catch(() => undefined);
    await patchCap(cleanup, 8).catch(() => undefined);
    await cleanup.close();
  });

  test("nine keys in B.3.2 order own disjoint 44px hit areas; raw keys stay on the 键 row", async ({
    page,
  }) => {
    const id = await createTerminal(page);
    await gotoTty(page, id);

    const bar = page.getByTestId("phone-keybar");
    await expect(bar).toBeVisible();
    const keys = page.locator("[data-testid^='phone-key-']");
    await expect(keys).toHaveCount(9);
    expect(await keys.evaluateAll((nodes) => nodes.map((n) => n.getAttribute("data-testid")))).toEqual(
      NINE.map((key) => `phone-key-${key}`),
    );

    // Evidence shot in the pristine collapsed state before geometry probes
    // scroll the row horizontally.
    await page.emulateMedia({ reducedMotion: "reduce" });
    await shot(page, "mobile-ui-6-terminal-390.png");

    // Four-corner geometry for every key: the strip scrolls horizontally, so
    // each key is centred first; a neighbour may never own its corners.
    for (const key of NINE) {
      await assertHitGeometry(page, `[data-testid='phone-key-${key}']`);
    }

    // Only one tty-keybar exists when the raw-key row is closed.
    await expect(page.getByTestId("tty-keybar")).toHaveCount(0);
    await page.getByTestId("phone-key-keyboard").click();
    await expect(page.getByTestId("tty-keybar")).toBeVisible();
    // The complete BAR set is back — capability does not regress.
    for (const keyId of RAW_KEYS) {
      const raw = page.getByTestId(`tty-key-${keyId}`);
      await expect(raw).toBeVisible();
      const box = await raw.boundingBox();
      expect(box, keyId).toBeTruthy();
      expect(box!.height, keyId).toBeGreaterThanOrEqual(44);
      expect(box!.width, keyId).toBeGreaterThanOrEqual(44);
    }
    // Sticky Ctrl is shared: turning it on in the action row lights the raw
    // row's Ctrl and vice versa.
    await page.getByTestId("phone-key-keyboard").click();
    await page.getByTestId("phone-key-ctrl").click();
    await expect(page.getByTestId("phone-key-ctrl")).toHaveAttribute("aria-pressed", "true");
    await page.getByTestId("phone-key-keyboard").click();
    await expect(page.getByTestId("tty-key-ctrl")).toHaveAttribute("aria-pressed", "true");
  });

  test("structure key switches in lock-step with the top-bar ViewSwitch (URL + viewPref)", async ({
    page,
  }) => {
    const id = await createTerminal(page);
    await gotoTty(page, id);
    const prefKey = `runtime.session-view.${id}`;

    await expect(page.getByTestId("view-switch")).toHaveAttribute("data-view", "tty");
    await page.getByTestId("phone-key-view").click();
    await expect(page).toHaveURL(new RegExp(`/s/${id}/structured$`));
    await expect(page.getByTestId("session-page")).toHaveAttribute("data-view", "structured");
    await expect(page.getByTestId("view-switch")).toHaveAttribute("data-view", "structured");
    expect(await page.evaluate((key) => localStorage.getItem(key), prefKey)).toBe("structured");

    // Back via the header segment: same route shape, preference follows.
    await page.getByTestId("view-switch-tty").click();
    await expect(page).toHaveURL(new RegExp(`/s/${id}/tty$`));
    await expect(page.getByTestId("view-switch")).toHaveAttribute("data-view", "tty");
    expect(await page.evaluate((key) => localStorage.getItem(key), prefKey)).toBe("tty");
    await expect(page.getByTestId("phone-keybar")).toBeVisible();
  });
});

test.describe("git key round trip (390px compact)", () => {
  // Compact phone layout is width-driven; this is the §3.4 "390 无触控"
  // dimension (narrow window, mouse), which keeps the nine-key bar mounted.
  test("navigates /files and returns to the live terminal with its buffer intact", async ({ browser }) => {
    const context = await browser.newContext({ viewport: { width: 390, height: 844 } });
    const page = await context.newPage();
    let id = "";
    try {
      await context.grantPermissions(["clipboard-read", "clipboard-write"]);
      await login(page);
      id = await createTerminal(page);
      await gotoTty(page, id);
      await expect(page.getByTestId("phone-keybar")).toBeVisible();

      // Put content into the PTY through the nine-key 贴 key (acceptance 4).
      const clip = page.getByTestId("phone-key-clip");
      await expect(clip).toBeEnabled({ timeout: 5_000 });
      const marker = `mkey-git-roundtrip-${Date.now()}`;
      await page.evaluate((text) => navigator.clipboard.writeText(text), marker);
      await clip.click();
      await expect(page.getByTestId("tty-raw-tail")).toContainText(marker, { timeout: 10_000 });

      // The git key uses the existing /s/:id/files navigation (acceptance 2).
      await page.getByTestId("phone-key-git").click();
      await expect(page).toHaveURL(new RegExp(`/s/${id}/files$`));
      await expect(page.getByTestId("files-pane")).toBeVisible();

      // FilesView's existing back control returns to /tty; the xterm remounts.
      await page.getByTestId("files-back").click();
      await expect(page).toHaveURL(new RegExp(`/s/${id}/tty$`));
      await expect(page.locator("[data-tty-ready='1']")).toBeVisible({ timeout: 20_000 });
      await expect(page.getByTestId("phone-keybar")).toBeVisible();

      // The fresh attach replayed the Hub screen snapshot: the pasted marker
      // is back in the terminal after the xterm unmount/remount, proving the
      // route round trip returns to the same live PTY. The capture/replay of
      // a scrolled LINE also runs on this remount — see evidence §5 for the
      // headless-xterm caveat that stops this spec asserting baseY=0.
      await expect(page.getByTestId("tty-raw-tail")).toContainText(marker);
    } finally {
      if (id) await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
      await context.close();
    }
  });
});

test.describe("1440px desktop regression", () => {
  test("toolbar AuxKeys render exactly as before; no phone bar", async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    await login(page);
    const id = await createTerminal(page);
    try {
      await page.goto(`/s/${id}/tty`);
      await expect(page.locator("[data-tty-ready='1']")).toBeVisible({ timeout: 20_000 });

      // The phone bar is compact-only.
      await expect(page.getByTestId("phone-keybar")).toHaveCount(0);

      // The unchanged desktop toolbar strip keeps all eleven tty-key-* ids.
      const bar = page.getByTestId("tty-keybar");
      await expect(bar).toBeVisible();
      for (const keyId of RAW_KEYS) {
        await expect(page.getByTestId(`tty-key-${keyId}`)).toBeVisible();
      }
      // Toolbar variant's compact 28px visual row (not the 44px mobile strip).
      const box = await bar.boundingBox();
      expect(box).toBeTruthy();
      expect(box!.height).toBeLessThanOrEqual(30);
    } finally {
      await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
      const still = created.indexOf(id);
      if (still >= 0) created.splice(still, 1);
    }
  });
});
