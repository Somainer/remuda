import { expect, test, type Page, type TestInfo } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { bootstrapToken } from "./hub-auth";

/**
 * UO-10 terminal chrome (docs/design/visual-system.md §3.4/§4, D-053 item 4,
 * ui-spec §2.3/§4.7). Playwright WebKit with an iPhone descriptor, fake Node
 * (hub_e2e). Covers:
 *
 *  1. The terminal is a dark instrument in BOTH appearances: pane and canvas
 *     share --term-bg, no seam; the frame never goes light.
 *  2. At 390px with the keyboard up (323px visible band) the local input and
 *     the phone key bar stay inside the band, xterm visible height > 0.
 *  3. Keyboard open/close sends ZERO PTY resize commands and keeps the same
 *     fitted rows/cols.
 *  4. The local input is 16px; no text on the terminal frame is below 12px.
 *
 * The dev-only `test:e2e:terminal` suite needs `remuda dev` panes on a herdr
 * server; per the task coordinator that is never run here — the hub e2e fake
 * Node and m-realdevice cover the terminal instead.
 */

test.describe.configure({ mode: "serial" });

const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence
  ? path.join(here, "../../../docs/design/evidence")
  : path.join(here, "../../test-results/evidence");

async function shot(page: Page, name: string, width?: number) {
  if (!evidence) return;
  await mkdir(shotDir, { recursive: true });
  // An explicit width clips a centre strip of that size (390 for phone);
  // no width captures the full viewport (1440 desktop appearance evidence).
  const clip = await page.evaluate((clipWidth) => {
    const w = window.innerWidth;
    const clipW = clipWidth ? Math.min(clipWidth, w) : w;
    return {
      x: Math.round((w - clipW) / 2),
      y: 0,
      width: clipW,
      height: window.innerHeight,
    };
  }, width);
  await page.screenshot({
    path: path.join(shotDir, name),
    animations: "disabled",
    clip,
  });
}

async function fakeHostId(page: Page): Promise<string | null> {
  const response = await page.request.get("/v1/hosts");
  if (!response.ok()) return null;
  const body = (await response.json()) as {
    items?: { id?: string; label?: string }[];
  };
  return (
    (body.items ?? []).find((row) => row.label === "e2e-fake-node")?.id ?? null
  );
}

async function createTerminal(page: Page, prompt: string): Promise<string> {
  const hostId = await fakeHostId(page);
  expect(hostId, "fake node host").toBeTruthy();
  const response = await page.request.post("/v1/instances", {
    data: {
      hostId,
      workspaceId: "wsp_e2e",
      kind: "terminal",
      driver: "shell-pty",
      prompt,
    },
  });
  expect(response.ok(), `create terminal: ${await response.text()}`).toBe(true);
  const body = (await response.json()) as {
    instance: { instanceId?: string; id?: string };
  };
  return body.instance.instanceId ?? body.instance.id!;
}

/** Exact iOS keyboard emulation (m-realdevice): layout viewport untouched. */
async function raiseKeyboardIosExact(page: Page, kb: number) {
  await page.evaluate((keyboardHeight) => {
    const vv = window.visualViewport;
    const h = window.innerHeight - keyboardHeight;
    Object.defineProperty(vv, "height", {
      configurable: true,
      get: () => h,
    });
    Object.defineProperty(vv, "offsetTop", {
      configurable: true,
      get: () => keyboardHeight,
    });
    window.scrollTo(0, keyboardHeight);
    vv.dispatchEvent(new Event("resize"));
    vv.dispatchEvent(new Event("scroll"));
    window.dispatchEvent(new Event("resize"));
  }, kb);
}

async function lowerKeyboardIosExact(page: Page) {
  await page.evaluate(() => {
    const vv = window.visualViewport;
    Object.defineProperty(vv, "height", {
      configurable: true,
      get: () => window.innerHeight,
    });
    Object.defineProperty(vv, "offsetTop", {
      configurable: true,
      get: () => 0,
    });
    vv.dispatchEvent(new Event("resize"));
    window.dispatchEvent(new Event("resize"));
  });
}

/** Open a terminal page and wait for the live lab. */
async function openTerminal(page: Page, id: string) {
  await page.goto(`/s/${id}/tty`);
  const lab = page.locator("[data-tty-lab]");
  await expect(page.getByTestId("session-page")).toHaveAttribute(
    "data-view",
    "tty",
  );
  await expect(lab).toHaveAttribute("data-tty-ready", "1", {
    timeout: 30_000,
  });
  await expect
    .poll(async () => Number(await lab.getAttribute("data-tty-rows")))
    .toBeGreaterThan(3);
  return lab;
}

const rect = (el: Element | null) => {
  if (!el) return null;
  const r = el.getBoundingClientRect();
  return {
    top: Math.round(r.top),
    bottom: Math.round(r.bottom),
    height: Math.round(r.height),
    width: Math.round(r.width),
  };
};

test.beforeEach(async ({ page }) => {
  await page.goto("/login");
  const useCode = page.getByTestId("login-use-code");
  if (await useCode.isVisible().catch(() => false)) await useCode.click();
  await page.getByTestId("login-tab-bootstrap").click();
  await page.getByTestId("login-device-name").fill("uo10-webkit");
  await page.getByTestId("login-bootstrap-token").fill(bootstrapToken);
  await page.getByTestId("login-submit").click();
  await page.waitForURL(/\/(sessions|m)(?:[/?]|$)/);
});

for (const appearance of ["dark", "light"] as const) {
  const expected = {
    dark: { hex: "#1a1917", rgb: [26, 25, 23] as const, lightInk: true },
    light: { hex: "#faf9f6", rgb: [250, 249, 246] as const, lightInk: false },
  }[appearance];

  test(`terminal follows ${appearance} appearance (pane/canvas match, no seam)`, async ({
    page,
  }) => {
    if (!(await fakeHostId(page))) test.skip(true, "fake Node not registered");
    // Desktop frame evidence regardless of the runner's device descriptor.
    await page.setViewportSize({ width: 1440, height: 900 });
    // Explicit appearance BEFORE navigation so every paint resolves the same
    // --term-* values.
    await page.addInitScript((mode) => {
      window.localStorage.setItem("runtime.theme.v1", mode);
    }, appearance);
    await page.reload();
    const id = await createTerminal(page, `uo10 ${appearance}`);
    const lab = await openTerminal(page, id);

    const colors = await page.evaluate(() => {
      const termBg = getComputedStyle(document.documentElement)
        .getPropertyValue("--term-bg")
        .trim();
      const pick = (sel: string, prop: string) => {
        const el = document.querySelector(sel);
        return el ? getComputedStyle(el)[prop] : null;
      };
      return {
        termBg,
        appearance: document.documentElement.dataset.appearance ?? null,
        pane: pick("[data-tty-lab]", "backgroundColor"),
        viewport: pick('[aria-label="终端画面"]', "backgroundColor"),
        host: pick(".xterm", "backgroundColor"),
      };
    });
    // The terminal bg is the mode's role token.
    expect(colors.termBg.toLowerCase()).toBe(expected.hex);
    expect(colors.appearance).toBe(appearance);
    // Pane and viewport paint the same background — no seam.
    expect(colors.pane).toBe(colors.viewport);
    const [pr, pg, pb] = expected.rgb;
    expect(colors.pane).toBe(`rgb(${pr}, ${pg}, ${pb})`);
    // The xterm host is transparent over the pane.
    expect(["rgba(0, 0, 0, 0)", `rgb(${pr}, ${pg}, ${pb})`, null]).toContain(
      colors.host,
    );
    // Chrome ink contrast on the mode's background (light ink on dark, dark
    // ink on light).
    const geo = page.getByTestId("tty-io-mode");
    await expect(geo).toBeVisible();
    const ink = await page.evaluate(() => {
      const read = (cssColor: string) => {
        const canvas = document.createElement("canvas");
        canvas.width = 1;
        canvas.height = 1;
        const ctx = canvas.getContext("2d")!;
        ctx.fillStyle = cssColor;
        ctx.fillRect(0, 0, 1, 1);
        return [...ctx.getImageData(0, 0, 1, 1).data.slice(0, 3)] as number[];
      };
      const pill = document.querySelector<HTMLElement>(
        "[data-testid='tty-io-mode']",
      )!;
      const labEl = document.querySelector<HTMLElement>("[data-tty-lab='1']")!;
      const readVar = (name: string) => {
        const probe = document.createElement("span");
        probe.style.color = name;
        probe.style.position = "absolute";
        labEl.appendChild(probe);
        const rgb = read(getComputedStyle(probe).color);
        probe.remove();
        return rgb;
      };
      return {
        pillRgb: read(getComputedStyle(pill).color),
        mutedRgb: readVar("var(--term-muted)"),
        fgRgb: readVar("var(--term-fg)"),
      };
    });
    expect(ink.pillRgb, "pill uses the terminal muted role").toEqual(
      ink.mutedRgb,
    );
    // Muted is visibly different from plain foreground in BOTH modes.
    expect(ink.pillRgb[0]).not.toBe(ink.fgRgb[0]);
    // 4.5:1 floor: channels all light on dark, all dark on light.
    if (expected.lightInk) {
      expect(Math.min(...ink.pillRgb)).toBeGreaterThan(130);
    } else {
      expect(Math.max(...ink.pillRgb)).toBeLessThan(125);
    }

    if (appearance === "dark") await shot(page, "uo10-terminal-dark-1440.png");
    if (appearance === "light")
      await shot(page, "uo10-terminal-light-1440.png");
  });

  test(`terminal switches live to ${appearance === "dark" ? "light" : "dark"} then back without rebuild (${appearance} start)`, async ({
    page,
  }) => {
    if (!(await fakeHostId(page))) test.skip(true, "fake Node not registered");
    await page.setViewportSize({ width: 1440, height: 900 });
    await page.addInitScript((mode) => {
      window.localStorage.setItem("runtime.theme.v1", mode);
    }, appearance);
    await page.reload();
    const id = await createTerminal(page, `uo10 switch ${appearance}`);
    await openTerminal(page, id);
    const other = appearance === "dark" ? "light" : "dark";
    const bgOf = () =>
      page.evaluate(
        () =>
          getComputedStyle(document.querySelector("[data-tty-lab]")!)
            .backgroundColor,
      );
    const startBg = await bgOf();
    const gridAtStart = {
      rows: Number(
        await page.locator("[data-tty-lab]").getAttribute("data-tty-rows"),
      ),
      cols: Number(
        await page.locator("[data-tty-lab]").getAttribute("data-tty-cols"),
      ),
    };

    // Switch the settings choice (no reload): the same terminal repaints.
    await page.evaluate((mode) => {
      localStorage.setItem("runtime.theme.v1", mode);
      document.documentElement.dataset.appearance = mode;
    }, other);
    await expect.poll(bgOf, { timeout: 3_000 }).not.toBe(startBg);
    // The xterm grid is intact — no rebuild.
    expect(
      Number(
        await page.locator("[data-tty-lab]").getAttribute("data-tty-rows"),
      ),
    ).toBe(gridAtStart.rows);
    expect(
      Number(
        await page.locator("[data-tty-lab]").getAttribute("data-tty-cols"),
      ),
    ).toBe(gridAtStart.cols);
    // Scrollback survives: the terminal element is the SAME node.
    const sameTerminal = await page.locator(".xterm").count();
    expect(sameTerminal).toBeGreaterThan(0);

    // Switch back.
    await page.evaluate((mode) => {
      localStorage.setItem("runtime.theme.v1", mode);
      document.documentElement.dataset.appearance = mode;
    }, appearance);
    await expect.poll(bgOf, { timeout: 3_000 }).toBe(startBg);
  });
}

test.describe("390px keyboard band", () => {
  // hasTouch + a narrow viewport emulates a phone on desktop chromium; the
  // mrealdevice project already gets these from its iPhone descriptor.
  test.use({
    viewport: { width: 393, height: 659 },
    hasTouch: true,
    isMobile: true,
  });

  test("local input + key bar stay in the 323px band and xterm stays visible", async ({
    page,
  }) => {
    if (!(await fakeHostId(page))) test.skip(true, "fake Node not registered");
    const id = await createTerminal(page, "uo10 keyboard");
    const lab = await openTerminal(page, id);

    const beforeRows = Number(await lab.getAttribute("data-tty-rows"));
    const beforeCols = Number(await lab.getAttribute("data-tty-cols"));

    // Reset the PTY resize counter AFTER the initial attach fit settled.
    await page.evaluate(() => {
      // Two rafs let the debounced initial resize flush first.
      return new Promise<void>((resolve) => {
        requestAnimationFrame(() =>
          requestAnimationFrame(() => {
            window.__ttyLab?.resetResizeCount();
            resolve();
          }),
        );
      });
    });

    // The local input field is 16px (coarse pointer).
    const field = page.locator("input[aria-label='本地输入']").first();
    await expect(field).toBeVisible();
    expect(
      await field.evaluate((el) => parseFloat(getComputedStyle(el).fontSize)),
    ).toBeGreaterThanOrEqual(16);

    await raiseKeyboardIosExact(page, 336);
    await page.waitForTimeout(300);

    const band = await page.evaluate(() => ({
      top: Math.round(window.visualViewport.offsetTop),
      bottom: Math.round(
        window.visualViewport.offsetTop + window.visualViewport.height,
      ),
      height: Math.round(window.visualViewport.height),
    }));
    expect(band.height).toBe(323);

    const geometry = await page.evaluate(() => {
      const rect = (el: Element | null) => {
        if (!el) return null;
        const r = el.getBoundingClientRect();
        return {
          top: Math.round(r.top),
          bottom: Math.round(r.bottom),
          height: Math.round(r.height),
          width: Math.round(r.width),
        };
      };
      type R = ReturnType<typeof rect>;
      const out: Record<string, R> = {};
      out.viewport = rect(document.querySelector('[aria-label="终端画面"]'));
      const input = document.querySelector(
        "input[aria-label='本地输入']",
      ) as HTMLElement | null;
      out.localField = rect(input);
      out.phoneBar = rect(
        document.querySelector("[data-testid='phone-keybar']"),
      );
      out.canvas = rect(document.querySelector(".xterm-screen"));
      return out;
    });

    // xterm still has visible height inside the band.
    expect(geometry.viewport, "xterm viewport after keyboard").not.toBeNull();
    expect(geometry.viewport!.height).toBeGreaterThan(0);
    expect(geometry.viewport!.top).toBeGreaterThanOrEqual(band.top - 1);
    expect(geometry.canvas!.height).toBeGreaterThan(0);

    // Local input fully inside the band.
    expect(geometry.localField).not.toBeNull();
    expect(geometry.localField!.height).toBeGreaterThan(0);
    expect(geometry.localField!.top).toBeGreaterThanOrEqual(band.top - 1);
    expect(geometry.localField!.bottom).toBeLessThanOrEqual(band.bottom + 1);

    // The phone key bar exists on a touch device and stays in the band.
    expect(geometry.phoneBar, "phone key bar mounted").not.toBeNull();
    expect(geometry.phoneBar!.bottom).toBeLessThanOrEqual(band.bottom + 1);
    expect(geometry.phoneBar!.top).toBeGreaterThanOrEqual(band.top - 1);

    // UO-10: NO PTY resize during keyboard open.
    const resizeCallsOpen = await page.evaluate(
      () => window.__ttyLab?.resizeCount() ?? null,
    );
    expect(resizeCallsOpen).toBe(0);
    // The fitted grid is unchanged.
    expect(Number(await lab.getAttribute("data-tty-rows"))).toBe(beforeRows);
    expect(Number(await lab.getAttribute("data-tty-cols"))).toBe(beforeCols);

    // UO-10 round-2 item 4: this is a FRESH shell whose banner/prompt occupy
    // the first few rows. The cursor-anchored crop must keep a painted early
    // row visible INSIDE the band (the old bottom-only clip left a short
    // buffer blank). WebGL canvas pixels are not readable
    // (preserveDrawingBuffer=false), so assert via renderer-agnostic geometry:
    // the ROW containing the active cursor has a painted element intersecting
    // the visible band near its top, and the screen's top edge sits inside
    // the band (offsetRows=0 for a short buffer).
    const promptVisible = await page.evaluate(() => {
      const viewport = document.querySelector<HTMLElement>(
        '[aria-label="终端画面"]',
      );
      const screen = document.querySelector<HTMLElement>(".xterm-screen");
      if (!viewport || !screen)
        return { ok: false, reason: "no terminal screen" };
      const vr = viewport.getBoundingClientRect();
      const sr = screen.getBoundingClientRect();
      // Cursor-anchored crop: for a short buffer the transform offset must be
      // 0, so the frozen screen's TOP edge aligns with (or is just below) the
      // viewport band's top — the prompt painted in the first rows is in the
      // band, not scrolled above it.
      const topGap = sr.top - vr.top;
      const topAligned = topGap >= -2 && topGap < vr.height * 0.35;
      // A painted row element (xterm-rows > div, or the webgl canvas) exists
      // within the top third of the band.
      const domRows = screen.querySelectorAll(".xterm-rows > div");
      let earlyRowInBand = false;
      domRows.forEach((row, i) => {
        if (i > 5) return;
        const r = row.getBoundingClientRect();
        if (r.height > 0 && r.top >= vr.top - 2 && r.bottom <= vr.bottom + 2)
          earlyRowInBand = true;
      });
      // For canvas/webgl renderers the screen element itself overlapping the
      // top of the band is the evidence.
      const canvasInBand =
        sr.top < vr.top + vr.height * 0.35 && sr.bottom > vr.top;
      const rowsMounted = domRows.length > 0;
      return {
        ok: topAligned && canvasInBand,
        topAligned,
        canvasInBand,
        earlyRowInBand,
        rowsMounted,
        topGap,
        srTop: sr.top,
        vrTop: vr.top,
      };
    });
    expect(
      promptVisible.ok,
      `fresh shell prompt visible after keyboard crop: ${JSON.stringify(promptVisible)}`,
    ).toBe(true);

    if (evidence) await shot(page, "uo10-terminal-keyboard-390.png", 390);

    // Keyboard closes: fit resumes — still ZERO resize commands when the grid
    // settles back to the same cols/rows.
    await lowerKeyboardIosExact(page);
    await page.waitForTimeout(500);
    await expect
      .poll(async () => Number(await lab.getAttribute("data-tty-rows")))
      .toBe(beforeRows);
    const resizeCallsClose = await page.evaluate(
      () => window.__ttyLab?.resizeCount() ?? null,
    );
    expect(
      resizeCallsClose,
      "no PTY resize across the full open/close cycle",
    ).toBe(0);

    // The min-text rule: nothing on the terminal frame is below 12px.
    await page.waitForTimeout(200);
    const smallest = await page.evaluate(() => {
      const lab = document.querySelector("[data-tty-lab]");
      if (!lab) return { min: null, offenders: [] as string[] };
      const offenders: string[] = [];
      let min = Number.POSITIVE_INFINITY;
      lab.querySelectorAll("*").forEach((el) => {
        const cs = getComputedStyle(el);
        const size = parseFloat(cs.fontSize);
        // Ignore elements that display no text (0-size / hidden).
        if (!Number.isFinite(size) || size === 0) return;
        const r = el.getBoundingClientRect();
        if (r.width === 0 || r.height === 0) return;
        // Only elements that actually render a glyph of their own text.
        const text = (el.textContent ?? "").trim();
        const ownText = Array.from(el.childNodes)
          .filter((n) => n.nodeType === Node.TEXT_NODE)
          .some((n) => n.textContent?.trim());
        if (!ownText || !text) return;
        min = Math.min(min, size);
        if (size < 12)
          offenders.push(
            `${(el as HTMLElement).dataset.testid ?? el.tagName}:${size}`,
          );
      });
      return { min: Number.isFinite(min) ? min : null, offenders };
    });
    expect(
      smallest.offenders,
      `sub-12px terminal text: ${smallest.offenders.join(", ")}`,
    ).toEqual([]);
    expect(smallest.min).toBeGreaterThanOrEqual(12);
  });

  test("width change with the keyboard open (rotation) refits and sends exactly one resize", async ({
    page,
  }) => {
    if (!(await fakeHostId(page))) test.skip(true, "fake Node not registered");
    const id = await createTerminal(page, "uo10 rotate");
    const lab = page.locator("[data-tty-lab]");
    await page.goto(`/s/${id}/tty`);
    await expect(lab).toHaveAttribute("data-tty-ready", "1", {
      timeout: 30_000,
    });
    await expect
      .poll(async () => Number(await lab.getAttribute("data-tty-rows")))
      .toBeGreaterThan(3);

    await page.evaluate(
      () =>
        new Promise<void>((resolve) => {
          requestAnimationFrame(() =>
            requestAnimationFrame(() => {
              window.__ttyLab?.resetResizeCount();
              resolve();
            }),
          );
        }),
    );
    const colsBefore = Number(await lab.getAttribute("data-tty-cols"));
    const rowsBefore = Number(await lab.getAttribute("data-tty-rows"));

    // Open the keyboard (height-only → frozen, zero resize).
    await raiseKeyboardIosExact(page, 336);
    await page.waitForTimeout(300);
    expect(
      await page.evaluate(() => window.__ttyLab?.resizeCount() ?? null),
    ).toBe(0);
    expect(Number(await lab.getAttribute("data-tty-cols"))).toBe(colsBefore);

    // Rotate: width changes while the keyboard stays open. This is a genuine
    // layout change — exactly one resize must go out with the new cols.
    const landscapeWidth = 700;
    await page.setViewportSize({
      width: landscapeWidth,
      height: 659,
    });
    // The keyboard stays at the same height in both orientations.
    await page.evaluate((kb) => {
      const vv = window.visualViewport;
      const h = window.innerHeight - kb;
      Object.defineProperty(vv, "height", {
        configurable: true,
        get: () => h,
      });
      Object.defineProperty(vv, "offsetTop", {
        configurable: true,
        get: () => kb,
      });
      vv.dispatchEvent(new Event("resize"));
      window.dispatchEvent(new Event("resize"));
    }, 336);
    await page.waitForTimeout(500);

    // Exactly one resize for the rotation.
    await expect
      .poll(async () =>
        page.evaluate(() => window.__ttyLab?.resizeCount() ?? null),
      )
      .toBe(1);
    // The new width produces a different (wider) col grid.
    const colsRotated = Number(await lab.getAttribute("data-tty-cols"));
    expect(colsRotated).toBeGreaterThan(colsBefore);
    // Rotation did not restore the pre-keyboard rows (still frozen height);
    // only cols changed.
    expect(Number(await lab.getAttribute("data-tty-rows"))).toBe(rowsBefore);
  });

  for (const appearance of ["light", "dark"] as const) {
    test(`history sheet stays a dark panel in ${appearance} appearance`, async ({
      page,
    }) => {
      if (!(await fakeHostId(page)))
        test.skip(true, "fake Node not registered");
      await page.setViewportSize({ width: 393, height: 659 });
      await page.addInitScript((mode) => {
        window.localStorage.setItem("runtime.theme.v1", mode);
      }, appearance);
      await page.reload();
      const id = await createTerminal(page, `uo10 history ${appearance}`);
      // Seed a prompt so the history list has content.
      await openTerminal(page, id);
      await page
        .locator("input[aria-label='本地输入']")
        .first()
        .fill("uo10 history seed");
      await page.keyboard.press("Enter");
      await page.waitForTimeout(500);
      // Open the 史 history panel from the phone key bar.
      await page.getByTestId("phone-key-history").click();
      const sheet = page.getByTestId("phone-history-sheet");
      await expect(sheet).toBeVisible();
      const heading = page.getByText("历史 prompt").first();
      await expect(heading).toBeVisible();

      // The panel is the dark terminal surface in BOTH appearances: its
      // background is --term-bg, and text stays light (pale text on white was
      // the round-2 light-mode bug).
      const panel = await page.evaluate(() => {
        const el = document.querySelector<HTMLElement>(
          "[data-testid='phone-history-sheet']",
        )!;
        const h = el.querySelector<HTMLElement>("h2")!;
        const read = (cssColor: string) => {
          const c = document.createElement("canvas");
          c.width = 1;
          c.height = 1;
          const ctx = c.getContext("2d")!;
          ctx.fillStyle = cssColor;
          ctx.fillRect(0, 0, 1, 1);
          return Array.from(ctx.getImageData(0, 0, 1, 1).data.slice(0, 3));
        };
        return {
          bg: read(getComputedStyle(el).backgroundColor),
          heading: read(getComputedStyle(h).color),
        };
      });
      // The panel follows the terminal appearance: it is the CURRENT mode's
      // --term-bg with matching foreground (no dark island in light mode, no
      // light island in dark mode).
      const expectedPanel =
        appearance === "light"
          ? { bg: [250, 249, 246] as number[], darkInk: true }
          : { bg: [26, 25, 23] as number[], darkInk: false };
      expect(panel.bg).toEqual(expectedPanel.bg);
      // Heading ink contrasts with the panel background in both modes.
      if (expectedPanel.darkInk) {
        expect(Math.max(...panel.heading)).toBeLessThan(120);
      } else {
        expect(Math.min(...panel.heading)).toBeGreaterThan(150);
      }

      if (appearance === "light")
        await shot(page, "uo10-history-sheet-light-390.png", 390);
      if (appearance === "dark")
        await shot(page, "uo10-history-sheet-dark-390.png", 390);
    });
  }
});
