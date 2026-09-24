import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { setMode } from "./appearanceHelper";
import { login } from "./hub-auth";

/**
 * LaTeX math rendering in a fake-node transcript (c-math, D-053 addendum 15).
 *
 * The synthetic assistant message is the node's `echo:` reflection of a
 * multiline prompt (same dance as ux-code.hub.spec.ts): display softmax,
 * inline math, a currency sentence that must stay text, a broken formula,
 * and a deliberately wide display formula for the overflow checks.
 */

test.describe.configure({ mode: "serial" });

// Deliberately wider than the reading column at ANY viewport: 48 subscripted
// terms (~2.2 KB of TeX) so the block must own a horizontal scrollbar.
const WIDE_FORMULA =
  "$$" +
  Array.from({ length: 48 }, (_, k) => `x_{${k + 1}}`).join("+") +
  "$$";

const MATH_PROMPT = [
  "请看 softmax 公式:",
  "$$\\sigma(\\mathbf{z})_i = \\frac{e^{z_i}}{\\sum_{j=1}^{K} e^{z_j}}$$",
  "其中 inline $y_i = \\mathbf{w}^{\\top}\\mathbf{x}_i + b$ 是线性部分。",
  "花了 $5 和 $10 都不渲染。",
  "坏的 $$\\frac{$$ 结束。",
  WIDE_FORMULA,
].join("\n");

const evidence =
  process.env.REMUDA_EVIDENCE === "1"
    ? path.join(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence")
    : path.join(path.dirname(fileURLToPath(import.meta.url)), "../../test-results/evidence");

async function raiseCap(page: Page, to: number): Promise<{ hostId: string; previous: number } | null> {
  const hosts = await page.evaluate(async () => {
    const response = await fetch("/v1/hosts", { credentials: "include" });
    return response.json();
  });
  const host = (hosts.items ?? []).find((h: { label?: string }) => h.label === "e2e-fake-node");
  if (!host) return null;
  const hostId = (host.hostId ?? host.id) as string;
  const previous = (host.maxInstances ?? 8) as number;
  if (previous < to) {
    await page.evaluate(
      ({ id, value }) =>
        fetch(`/v1/hosts/${id}`, {
          method: "PATCH",
          credentials: "include",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ maxInstances: value }),
        }),
      { id: hostId, value: to },
    );
  }
  return { hostId, previous };
}

let cap: { hostId: string; previous: number } | null = null;
const created: string[] = [];

async function answerPendingApprovals(page: Page, id: string): Promise<void> {
  await page.evaluate(async (instanceId) => {
    const list = await fetch("/v1/interactions", { credentials: "include" });
    const body = (await list.json()) as {
      items?: {
        id: string;
        instanceId?: string;
        state?: string;
        request?: { inputDigest?: string; options?: { id: string }[] };
      }[];
    };
    for (const item of (body.items ?? []).filter(
      (it) => it.instanceId === instanceId && it.state === "pending",
    )) {
      const optionId = item.request?.options?.[0]?.id;
      if (!optionId) continue;
      await fetch(`/v1/interactions/${item.id}/answer`, {
        method: "POST",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          answer: { kind: "approval", optionId, inputDigest: item.request?.inputDigest ?? "" },
        }),
      });
    }
  }, id);
}

interface ReadyCounts {
  display: number;
  inline: number;
  error: number;
  skip?: number;
}

const DEFAULT_COUNTS: ReadyCounts = { display: 2, inline: 1, error: 1 };

async function createReadySession(
  page: Page,
  prompt: string,
  mobile = false,
  counts: ReadyCounts = DEFAULT_COUNTS,
): Promise<string> {
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  await expect(hostPicker).toContainText("e2e-fake-node", { timeout: 20_000 });
  const hostId = await hostPicker.locator("option").filter({ hasText: "e2e-fake-node" }).getAttribute("value");
  expect(hostId).toBeTruthy();
  await hostPicker.selectOption(hostId!);
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0, {
    timeout: 20_000,
  });
  await page.getByTestId("new-session-prompt").fill("math-render fixture");
  const creating = page.waitForResponse(
    (response) =>
      response.request().method() === "POST" && new URL(response.url()).pathname === "/v1/instances",
  );
  await page.getByTestId("new-session-start").click();
  const res = await creating;
  expect(res.ok(), `instance create failed: ${res.status()}`).toBe(true);
  const instanceId = (await res.json()).instance.instanceId as string;
  created.push(instanceId);
  await expect(page).toHaveURL(new RegExp(`/s/${instanceId}`), { timeout: 20_000 });

  await answerPendingApprovals(page, instanceId);
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 30_000 });

  const composer = page.getByTestId("composer-input");
  await composer.fill(prompt);
  if (mobile) {
    await page.getByTestId("composer-send").click();
  } else {
    await composer.press("Enter");
  }
  await expect(page.getByTestId("math-display")).toHaveCount(counts.display, { timeout: 30_000 });
  await expect(page.getByTestId("math-inline")).toHaveCount(counts.inline);
  await expect(page.getByTestId("math-error")).toHaveCount(counts.error);
  if (counts.skip) await expect(page.getByTestId("math-skip")).toHaveCount(counts.skip);
  // Let the lazy KaTeX chunk, CSS and fonts settle (both display blocks).
  await expect
    .poll(() => page.locator('[data-testid="math-display"] .katex-display').count())
    .toBe(counts.display);
  await expect
    .poll(() => page.locator('[data-testid="math-inline"] .katex').count())
    .toBe(counts.inline);
  await page.evaluate(() => (document as Document & { fonts?: { ready: Promise<unknown> } }).fonts?.ready);
  await page.waitForTimeout(400);
  return instanceId;
}

/**
 * The assistant (echo) row carrying the math fixture. The fake node also
 * echoes the instance-creation prompt ("math-render fixture"), so the math
 * message is the LAST assistant row, not the first.
 */
function assistantRow(page: Page) {
  return page.locator('[data-testid="transcript-row"][data-role="assistant"]').last();
}

/** Text nodes carrying the raw source must live in the hidden MathML tree. */
async function rawSourceLeak(page: Page): Promise<string | null> {
  return assistantRow(page).evaluate((root) => {
    const needle = "\\sigma(\\mathbf{z})";
    const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
    let node: Node | null;
    while ((node = walker.nextNode())) {
      const text = node.nodeValue ?? "";
      if (!text.includes(needle)) continue;
      const element = node.parentElement;
      if (element?.closest(".katex-mathml")) continue;
      return text.slice(0, 120);
    }
    return null;
  });
}

/** Delimiter text ("$$" / "\[" / "\(") must not survive in the assistant row. */
async function rawDelimiterLeak(page: Page): Promise<string | null> {
  return assistantRow(page).evaluate((root) => {
    const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
    let node: Node | null;
    while ((node = walker.nextNode())) {
      const text = node.nodeValue ?? "";
      if (text.includes("$$") || text.includes("\\[") || text.includes("\\(")) {
        return text.slice(0, 120);
      }
    }
    return null;
  });
}

async function settleAndShootMath(page: Page, screenshotPath: string): Promise<void> {
  await expect(
    async () => {
      const target = page.getByTestId("math-display").first();
      const first = (await target.boundingBox())?.height ?? 0;
      await page.waitForTimeout(120);
      const second = (await target.boundingBox())?.height ?? 0;
      if (!(first > 0 && first === second)) throw new Error("math layout is still settling");
      await target.screenshot({ path: screenshotPath, timeout: 5_000 });
    },
    { message: `math settles and captures ${path.basename(screenshotPath)}` },
  ).toPass({ timeout: 20_000 });
}

test.beforeEach(async ({ page }) => {
  await login(page);
  if (!cap) cap = await raiseCap(page, 24);
});

test.afterEach(async ({ page }) => {
  await page.unrouteAll({ behavior: "ignoreErrors" }).catch(() => {});
  for (const id of created.splice(0, created.length)) {
    await page
      .evaluate((value) => fetch(`/v1/instances/${value}?force=1`, { method: "DELETE", credentials: "include" }), id)
      .catch(() => {});
  }
});

test.afterAll(async ({ browser }) => {
  if (!cap) return;
  const page = await browser.newPage();
  try {
    await login(page);
    await page.evaluate(
      ({ id, value }) =>
        fetch(`/v1/hosts/${id}`, {
          method: "PATCH",
          credentials: "include",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ maxInstances: value }),
        }),
      { id: cap!.hostId, value: cap!.previous },
    );
  } finally {
    await page.close();
  }
});

test.describe("1440px", () => {
  test.use({ viewport: { width: 1440, height: 900 } });

  for (const theme of ["dark", "light"] as const) {
    test(`renders math via KaTeX in ${theme} with no raw TeX and no page overflow`, async ({ page }) => {
      const mathRequests: string[] = [];
      page.on("request", (req) => {
        if (/katex/i.test(new URL(req.url()).pathname)) mathRequests.push(req.url());
      });

      await createReadySession(page, MATH_PROMPT);
      await setMode(page, theme);
      await page.waitForTimeout(200);

      const display = page.getByTestId("math-display").first();
      await expect(display.locator(".katex-display")).toHaveCount(1);
      await expect(display.locator(".mfrac")).toHaveCount(1);
      await expect(display.locator(".mop")).not.toHaveCount(0);
      await expect(page.getByTestId("math-inline").locator(".katex")).toHaveCount(1);

      // The lazy KaTeX chunk really was fetched for a message with math.
      expect(mathRequests.length).toBeGreaterThan(0);

      // Delimiters are consumed; the raw softmax source only exists in the
      // clipped MathML a11y tree, never as assistant-row text. The user's own
      // bubble legitimately shows what they typed, so scope to the echo row.
      expect(await rawDelimiterLeak(page)).toBeNull();
      expect(await rawSourceLeak(page)).toBeNull();

      // Currency stayed text, shell-safe and untouched. It shares a
      // paragraph with the broken-formula lead line, so match by substring.
      await expect(
        assistantRow(page).getByText(/花了 \$5 和 \$10 都不渲染。/),
      ).toBeVisible();

      // Broken formula: raw source in the error role, message still alive.
      const error = page.getByTestId("math-error");
      await expect(error).toBeVisible();
      expect(await error.textContent()).toBe("\\frac{");
      await expect(assistantRow(page).getByText("结束。")).toBeVisible();

      // KaTeX paints with the reading ink (currentColor) in both appearances.
      const colors = await page.evaluate(() => {
        const katex = document.querySelector('[data-testid="math-inline"] .katex');
        const surface = document.querySelector('[data-testid="math-inline"]');
        return {
          katex: katex ? getComputedStyle(katex).color : null,
          wrapper: surface ? getComputedStyle(surface).color : null,
        };
      });
      expect(colors.katex).toBeTruthy();
      expect(colors.katex).toBe(colors.wrapper);

      // Wide formula scrolls INSIDE its block, never moving the page.
      const geometry = await page.getByTestId("math-display").nth(1).evaluate((el) => ({
        scrollWidth: el.scrollWidth,
        clientWidth: el.clientWidth,
        docWidth: document.documentElement.scrollWidth,
        winWidth: window.innerWidth,
      }));
      expect(geometry.scrollWidth).toBeGreaterThan(geometry.clientWidth + 24);
      expect(geometry.docWidth).toBeLessThanOrEqual(geometry.winWidth + 1);

      if (process.env.REMUDA_EVIDENCE === "1") {
        await mkdir(evidence, { recursive: true });
        await settleAndShootMath(page, path.join(evidence, `transcript-math-1-${theme}-1440.png`));
      }
    });
  }
});

test.describe("390px", () => {
  test.use({ viewport: { width: 390, height: 844 }, hasTouch: true, isMobile: true });

  for (const theme of ["dark", "light"] as const) {
    test(`math stays in-column at 390px in ${theme} with internal scroll`, async ({ page }) => {
      await createReadySession(page, MATH_PROMPT, true);
      await setMode(page, theme);
      await page.waitForTimeout(200);

      const noPageOverflow = await page.evaluate(() => ({
        doc: document.documentElement.scrollWidth,
        win: window.innerWidth,
      }));
      expect(noPageOverflow.doc).toBeLessThanOrEqual(noPageOverflow.win + 1);

      const wide = page.getByTestId("math-display").nth(1);
      await expect
        .poll(async () => wide.evaluate((el) => el.scrollWidth - el.clientWidth))
        .toBeGreaterThan(24);
      // The softmax block also never pushes the column wider than the screen.
      expect(
        await page
          .getByTestId("math-display")
          .first()
          .evaluate((el) => el.getBoundingClientRect().right <= window.innerWidth + 1),
      ).toBe(true);

      if (process.env.REMUDA_EVIDENCE === "1") {
        await mkdir(evidence, { recursive: true });
        await settleAndShootMath(page, path.join(evidence, `transcript-math-1-${theme}-390.png`));
      }
    });
  }
});

test.describe("round-2 hardening", () => {
  test.use({ viewport: { width: 1440, height: 900 } });

  test("bounded work, structure, provenance, literal streaming and inline geometry", async ({
    page,
  }) => {
    const HUGE = "x+1".repeat(25_000); // 100 KB
    // The unclosed \[ swallows everything after it (streaming rule), so it
    // must be the last line.
    const prompt = [
      "r2 引用块：",
      "> $$q^2$$",
      "r2 列表：",
      "- $$l^2$$",
      "r2 缩进代码：",
      "",
      "    $$indented$$",
      "r2 math fence：",
      "```math",
      "fenced^2",
      "```",
      "r2 超宽规则： $$\\rule{100000em}{100000em}$$",
      "r2 宏炸弹： $\\def\\a{\\a}\\a$",
      "r2 超高 inline：前后 $\\dfrac{1}{\\dfrac{1}{x}}$ 文字",
      `r2 百KB： $${HUGE}$`,
      "r2 未闭合： intro \\[a *b* + \\{c\\}",
    ].join("\n");

    // 3 display (quote, list, clamped rule), 1 good inline (tall frac),
    // 1 error (macro bomb), 1 size-skip (100 KB).
    await createReadySession(page, prompt, false, { display: 3, inline: 1, error: 1, skip: 1 });

    // (#4) unclosed \[ renders literal with stars/braces/delimiters intact.
    await expect(assistantRow(page).getByText(/intro \\?\[a \*b\* \+ \\?\{c\\?\}/)).toBeVisible();
    expect(assistantRow(page).locator("em")).toHaveCount(0);

    // (#2) $$ stays inside the blockquote and the list item.
    await expect(page.locator("blockquote [data-testid='math-display']")).toBeVisible();
    await expect(page.locator("ul > li [data-testid='math-display']")).toBeVisible();

    // (#2) indented $$ is a code block.
    const codeBlocks = page.getByTestId("code-block");
    await expect(codeBlocks.first()).toBeVisible();
    const codeTexts = await codeBlocks.allInnerTexts();
    expect(codeTexts.some((t) => t.includes("fenced^2"))).toBe(true);
    // The math-fenced block is never a math node.
    expect(await page.locator("pre code.language-mathdisplay").count()).toBe(0);

    // (#1) the 100000em rule is clamped to 20em (never 100000em tall): the
    // rule's own border box is at the cap, and the whole display block stays
    // bounded (20em + display line-leading), not astronomically tall.
    const geom = await assistantRow(page)
      .locator('[data-testid="math-display"]')
      .filter({ has: page.locator(".katex-rule") })
      .first()
      .evaluate((el) => {
        const rule = el.querySelector(".katex-rule") as HTMLElement;
        const cs = getComputedStyle(rule);
        return {
          borderTop: cs.borderTopWidth ? Number.parseFloat(cs.borderTopWidth) : 0,
          borderRight: cs.borderRightWidth ? Number.parseFloat(cs.borderRightWidth) : 0,
          wrap: el.getBoundingClientRect().height,
          ruleFont: Number.parseFloat(getComputedStyle(rule).fontSize),
        };
      });
    expect(geom.borderTop).toBeLessThanOrEqual(geom.ruleFont * 20 + 0.5);
    expect(geom.borderRight).toBeLessThanOrEqual(geom.ruleFont * 20 + 0.5);
    // Unclamped 100000em would be > 1.6 million px; a clamped block is < 30em.
    expect(geom.wrap).toBeLessThan(geom.ruleFont * 30);

    // (#1) macro bomb is an error node, not an infinite expansion.
    await expect(page.getByTestId("math-error")).toBeVisible();

    // (#1) the 100 KB formula is size-skipped (raw source), no KaTeX node.
    await expect(page.getByTestId("math-skip")).toBeVisible();

    // (#8) a tall nested inline fraction does not enlarge its line box:
    // the inline node itself is capped, and its margin box stays at one line
    // (inline-block overflows visibly, not via height). The tall \dfrac is
    // the only good inline math node in this message.
    const tallInline = page.getByTestId("math-inline");
    await expect(tallInline).toHaveCount(1);
    const inlineCap = await tallInline.evaluate((el) => {
      const wrap = el as HTMLElement;
      const cs = getComputedStyle(wrap);
      return {
        display: cs.display,
        maxHeightPx: Number.parseFloat(cs.maxHeight),
        wrapHeight: wrap.getBoundingClientRect().height,
        fontPx: Number.parseFloat(cs.fontSize),
      };
    });
    expect(inlineCap.display).toBe("inline-block");
    expect(inlineCap.maxHeightPx).toBeCloseTo(1.4 * inlineCap.fontPx, 1);
    expect(inlineCap.wrapHeight).toBeLessThanOrEqual(1.4 * inlineCap.fontPx + 2);
  });
});

test("never requests the KaTeX chunk for a session without math", async ({ page }) => {
  const mathRequests: string[] = [];
  page.on("request", (req) => {
    if (/katex/i.test(new URL(req.url()).pathname)) mathRequests.push(req.url());
  });

  await page.goto("/sessions/new");
  if (!cap) cap = await raiseCap(page, 24);
  const hostPicker = page.getByTestId("new-session-host");
  await expect(hostPicker).toContainText("e2e-fake-node", { timeout: 20_000 });
  const hostId = await hostPicker.locator("option").filter({ hasText: "e2e-fake-node" }).getAttribute("value");
  await hostPicker.selectOption(hostId!);
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0, {
    timeout: 20_000,
  });
  await page.getByTestId("new-session-prompt").fill("no math fixture");
  const creating = page.waitForResponse(
    (response) =>
      response.request().method() === "POST" && new URL(response.url()).pathname === "/v1/instances",
  );
  await page.getByTestId("new-session-start").click();
  const res = await creating;
  const instanceId = (await res.json()).instance.instanceId as string;
  created.push(instanceId);
  await expect(page).toHaveURL(new RegExp(`/s/${instanceId}`), { timeout: 20_000 });
  await answerPendingApprovals(page, instanceId);
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 30_000 });
  await page.getByTestId("composer-input").fill("just plain text, no formulas here");
  await page.getByTestId("composer-input").press("Enter");
  // The prompt appears in the user bubble and in the node echo; both are fine.
  await expect(page.getByText("just plain text, no formulas here").first()).toBeVisible({
    timeout: 30_000,
  });
  // Streaming append + chunk scheduling grace period.
  await page.waitForTimeout(1500);
  expect(mathRequests).toEqual([]);
});
