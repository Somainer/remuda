import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * Code-block toolbar + syntax highlighting, against the fake node only.
 *
 * The synthetic assistant message is the node's `echo:` reflection of a
 * multiline prompt containing fenced code (no real model is ever called).
 * The first prompt line is ordinary prose on purpose: the fake node
 * prepends `echo: ` on the same line, so a fence on line one would lose its
 * column-0 opening and render as text.
 */

test.describe.configure({ mode: "serial" });

const FENCED_PROMPT = [
  "请看代码:",
  "```ts",
  "export function greet(name: string): string {",
  "  const paddingProbe = alpha + beta + gamma + delta + epsilon + zeta + eta + theta + iota + kappa + lambda + mu + nu + xi + omicron + pi + rho + sigma + tau + upsilon + phi + chi + psi + omega;",
  "  return \"hello, \" + name;",
  "}",
  "```",
  "",
  "```bash",
  "echo \"deploy\" && cargo test",
  "```",
].join("\n");

// Committed evidence is refreshed only on request (REMUDA_EVIDENCE=1); every other run —
// including the merge gate, whose verify-tree step rejects a dirty worktree — writes
// the same screenshots under the gitignored test-results/ instead.
const evidence = process.env.REMUDA_EVIDENCE === "1"
  ? path.join(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence")
  : path.join(path.dirname(fileURLToPath(import.meta.url)), "../../test-results/evidence");

/** Same fake-node dance as ux-status.spec: raise the shared 8-instance cap. */
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

async function createReadySession(page: Page, prompt: string, mobile = false): Promise<string> {
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  await expect(hostPicker).toContainText("e2e-fake-node", { timeout: 20_000 });
  const hostId = await hostPicker.locator("option").filter({ hasText: "e2e-fake-node" }).getAttribute("value");
  expect(hostId).toBeTruthy();
  await hostPicker.selectOption(hostId!);
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0, { timeout: 20_000 });
  await page.getByTestId("new-session-prompt").fill("ux-code fixture");
  const creating = page.waitForResponse(
    (response) => response.request().method() === "POST" && new URL(response.url()).pathname === "/v1/instances",
  );
  await page.getByTestId("new-session-start").click();
  const res = await creating;
  expect(res.ok(), `instance create failed: ${res.status()}`).toBe(true);
  const instanceId = (await res.json()).instance.instanceId as string;
  created.push(instanceId);
  await expect(page).toHaveURL(new RegExp(`/s/${instanceId}`), { timeout: 20_000 });

  // Every create raises a pending approval that disables the composer.
  await page.evaluate(async (id) => {
    const list = await fetch("/v1/interactions", { credentials: "include" });
    const body = (await list.json()) as {
      items?: { id: string; instanceId?: string; state?: string; request?: { inputDigest?: string; options?: { id: string }[] } }[];
    };
    for (const item of (body.items ?? []).filter((it) => it.instanceId === id && it.state === "pending")) {
      const optionId = item.request?.options?.[0]?.id;
      if (!optionId) continue;
      await fetch(`/v1/interactions/${item.id}/answer`, {
        method: "POST",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ answer: { kind: "approval", optionId, inputDigest: item.request?.inputDigest ?? "" } }),
      });
    }
  }, instanceId);
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 30_000 });

  const composer = page.getByTestId("composer-input");
  await composer.fill(prompt);
  // Mobile Composer deliberately ignores Enter (the on-screen keyboard has
  // its own newline); the submit button is the send action there.
  if (mobile) {
    await page.getByTestId("composer-send").click();
  } else {
    await composer.press("Enter");
  }
  await expect(page.getByTestId("code-block")).toHaveCount(2, { timeout: 30_000 });
  // The echo arrives as an append chain; wait for streaming + virtual-window
  // settling so a later scroll nudge cannot move the block out from under a
  // hovering pointer mid-assertion.
  await page.waitForTimeout(1500);
  return instanceId;
}

test.beforeEach(async ({ page, context }) => {
  await login(page);
  if (!cap) cap = await raiseCap(page, 24);
  await context.grantPermissions(["clipboard-read", "clipboard-write"]);
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

test("fenced code: hover toolbar, copy, highlighting and wrap", async ({ page }) => {
  await createReadySession(page, FENCED_PROMPT);

  const blocks = page.getByTestId("code-block");
  const first = blocks.first();
  const toolbar = first.getByTestId("code-toolbar");

  // Language label + real token spans (lazy grammar chunk loaded).
  await expect(first.getByTestId("code-lang")).toHaveText("TypeScript");
  await expect(first.locator(".hljs-keyword").first()).toBeVisible();
  await expect(blocks.nth(1).getByTestId("code-lang")).toHaveText("Bash");

  // Low contrast until hover.
  await expect(toolbar).toHaveCSS("opacity", "0");
  await first.hover();
  await expect(toolbar).toHaveCSS("opacity", "1");

  // Accessible names and tooltips on both buttons.
  for (const name of ["换行", "复制"]) {
    const button = first.getByRole("button", { name });
    await expect(button).toHaveAttribute("title", name);
  }

  // Long lines scroll INSIDE the pre; the page itself never scrolls sideways.
  const geometry = await first.getByTestId("code-pre").evaluate((pre) => ({
    scrollWidth: pre.scrollWidth,
    clientWidth: pre.clientWidth,
    docWidth: document.documentElement.scrollWidth,
    winWidth: window.innerWidth,
  }));
  expect(geometry.scrollWidth).toBeGreaterThan(geometry.clientWidth + 24);
  expect(geometry.docWidth).toBeLessThanOrEqual(geometry.winWidth + 1);

  // Copy writes the raw fence text and flips the button to a transient check.
  await first.getByRole("button", { name: "复制" }).click();
  await expect(first.getByRole("button", { name: "已复制" })).toBeVisible();
  await expect
    .poll(() => page.evaluate(() => navigator.clipboard.readText()))
    .toContain("export function greet");
  await expect(first.getByRole("button", { name: "复制" })).toBeVisible({ timeout: 5_000 });

  // Soft wrap changes the pre geometry and persists on the block.
  await first.getByRole("button", { name: "换行" }).click();
  await expect(first).toHaveAttribute("data-wrap", "on");
  await expect
    .poll(() =>
      first.getByTestId("code-pre").evaluate((pre) => pre.scrollWidth - pre.clientWidth),
    )
    .toBeLessThanOrEqual(2);
  expect(await page.evaluate(() => localStorage.getItem("runtime.code-wrap"))).toBe("1");

  // Evidence shows the default state (scroll inside the block, toolbar shown).
  await first.getByRole("button", { name: "取消换行" }).click();
  await expect(first).toHaveAttribute("data-wrap", "off");
  const jump = page.getByTestId("jump-latest");
  if (await jump.isVisible().catch(() => false)) await jump.click();
  await first.hover();
  await mkdir(evidence, { recursive: true });
  for (const theme of ["night", "ledger"]) {
    await page.evaluate((value) => document.documentElement.setAttribute("data-theme", value), theme);
    await page.waitForTimeout(150);
    // Re-query the block right before capturing: a late follow re-render can
    // detach the element resolved earlier in the test. toBeVisible retries
    // until the freshly-resolved locator is attached and stable.
    const shot = blocks.first();
    await expect(shot).toBeVisible();
    await shot.screenshot({ path: path.join(evidence, `workbench-code-1-${theme}-1440.png`) });
  }
});

test.describe("390px", () => {
  test.use({
    viewport: { width: 390, height: 844 },
    hasTouch: true,
    isMobile: true,
  });

  test("toolbar stays visible with 44px targets and no page-wide overflow", async ({ page }) => {
    await createReadySession(page, FENCED_PROMPT, true);
    const first = page.getByTestId("code-block").first();

    // Touch: no hover-reveal gating.
    await expect(first.getByTestId("code-toolbar")).toHaveCSS("opacity", "1");

    for (const name of ["换行", "复制"]) {
      // The transcript re-renders while live observations arrive; poll the box
      // instead of sampling it once (a mid-render sample returns null).
      const button = first.getByRole("button", { name });
      await expect(button, `${name} button must be visible`).toBeVisible();
      await expect
        .poll(async () => (await button.boundingBox())?.width ?? 0, { message: `${name} button width` })
        .toBeGreaterThanOrEqual(44);
      await expect
        .poll(async () => (await button.boundingBox())?.height ?? 0, { message: `${name} button height` })
        .toBeGreaterThanOrEqual(44);
    }

    const noPageOverflow = await page.evaluate(() => ({
      doc: document.documentElement.scrollWidth,
      win: window.innerWidth,
    }));
    expect(noPageOverflow.doc).toBeLessThanOrEqual(noPageOverflow.win + 1);

    // Default is horizontal scroll kept inside the pre, not wrapped.
    await expect(first).toHaveAttribute("data-wrap", "off");
    const preGeometry = await first.getByTestId("code-pre").evaluate((pre) => ({
      scrollWidth: pre.scrollWidth,
      clientWidth: pre.clientWidth,
    }));
    expect(preGeometry.scrollWidth).toBeGreaterThan(preGeometry.clientWidth + 24);

    // Toggling wrap kills the inner overflow instead of moving it to the page.
    await first.getByRole("button", { name: "换行" }).click();
    await expect(first).toHaveAttribute("data-wrap", "on");
    await expect
      .poll(() => first.getByTestId("code-pre").evaluate((pre) => pre.scrollWidth - pre.clientWidth))
      .toBeLessThanOrEqual(2);
    const afterWrap = await page.evaluate(() => ({
      doc: document.documentElement.scrollWidth,
      win: window.innerWidth,
    }));
    expect(afterWrap.doc).toBeLessThanOrEqual(afterWrap.win + 1);

    // Evidence shows the default state with both messages in view.
    await first.getByRole("button", { name: "取消换行" }).click();
    await expect(first).toHaveAttribute("data-wrap", "off");
    const jump = page.getByTestId("jump-latest");
    if (await jump.isVisible().catch(() => false)) await jump.click();
    await page.waitForTimeout(200);

    await mkdir(evidence, { recursive: true });
    for (const theme of ["night", "ledger"]) {
      await page.evaluate((value) => document.documentElement.setAttribute("data-theme", value), theme);
      await page.waitForTimeout(150);
      await first.screenshot({ path: path.join(evidence, `workbench-code-1-${theme}-390.png`) });
    }
  });
});
