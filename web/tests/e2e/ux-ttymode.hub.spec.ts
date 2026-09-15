import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * 渲染方式 badge vs. real terminal mode (c-ttymode, 2026-09-15).
 *
 * The badge must follow the harness's actual screen, not the launch
 * preference: 待检测 before any observation, 行内渲染 when the attach reports
 * the primary buffer, 全屏渲染 after the harness enters the DEC alt screen
 * (`?1049h`), and back when it leaves.
 *
 * Fake Node only — the in-process harness in
 * crates/remuda-hub/examples/hub_e2e.rs. Typing TTYMODE_ALT_ON + Enter makes
 * the harness emit the raw DEC sequence plus a tty.mode notice, exactly like
 * the Node's herdr byte-stream scanner relay does for a real pane.
 */
const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence
  ? path.join(here, "../../../docs/design/evidence")
  : path.join(here, "../../test-results/evidence");

async function shot(page: Page, name: string) {
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

test.describe.configure({ mode: "serial" });

test.skip(process.env.HUB_E2E_EXTERNAL === "1", "Needs the in-process fake Node");

async function patchMaxInstances(page: Page, value: number): Promise<number | undefined> {
  return page.evaluate(async (next) => {
    const list = await fetch("/v1/hosts", { credentials: "include" });
    const body = (await list.json()) as {
      items?: { hostId?: string; maxInstances?: number }[];
    };
    const id = body.items?.find((host) => host.hostId)?.hostId;
    if (!id) return undefined;
    const previous = body.items?.find((host) => host.hostId === id)?.maxInstances;
    await fetch(`/v1/hosts/${id}`, {
      method: "PATCH",
      credentials: "include",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ maxInstances: next }),
    });
    return previous;
  }, value);
}

async function forceDeleteAllInstances(page: Page) {
  await page.evaluate(async () => {
    const list = await fetch("/v1/instances", { credentials: "include" });
    const body = (await list.json()) as { items?: { id: string }[] };
    await Promise.all(
      (body.items ?? []).map((instance) =>
        fetch(`/v1/instances/${instance.id}?force=1`, {
          method: "DELETE",
          credentials: "include",
        }).catch(() => undefined),
      ),
    );
  });
}

test.beforeAll(async ({ browser }) => {
  const setup = await browser.newPage();
  await login(setup);
  await patchMaxInstances(setup, 24);
  await setup.close();
});

test.afterAll(async ({ browser }) => {
  const setup = await browser.newPage();
  await login(setup);
  await patchMaxInstances(setup, 8);
  await forceDeleteAllInstances(setup).catch(() => undefined);
  await setup.close();
});

test.afterEach(async ({ page }) => {
  await forceDeleteAllInstances(page).catch(() => undefined);
});

/** Create a raw terminal session on the fake Node; resolves on its tty tab. */
async function startTerminalSession(page: Page): Promise<void> {
  await forceDeleteAllInstances(page);
  await page.goto("/sessions/new");
  await expect(page.getByTestId("new-session-sheet")).toBeVisible();
  await expect(page.getByTestId("new-session-host")).toContainText("e2e-fake-node", {
    timeout: 20_000,
  });
  const host = await page
    .getByTestId("new-session-host")
    .locator("option")
    .filter({ hasText: "e2e-fake-node" })
    .getAttribute("value");
  await page.getByTestId("new-session-host").selectOption(host!);
  await page.getByTestId("new-session-kind-terminal").click();
  await expect(page.getByTestId("new-session-start")).toBeEnabled();
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
  // The fake-node terminal opens on the structured tab by default; the TTY
  // surface is the explicit /tty route.
  const instanceId = new URL(page.url()).pathname.split("/")[2]!;
  await page.goto(`/s/${instanceId}/tty`);
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-view", "tty");
  await expect(page.locator("[data-tty-lab='1']")).toBeVisible();
}

/** xterm parks its input in a 3x10 helper textarea; focus it directly. */
async function focusTerminal(page: Page) {
  await page
    .locator(".xterm-helper-textarea")
    .first()
    .evaluate((el) => (el as HTMLTextAreaElement).focus());
}

test("渲染方式 badge follows the harness: 待检测 → 行内渲染 → 全屏渲染 → 行内渲染", async ({ page }) => {
  // Race-proof record of every badge state from first paint: the attach
  // tty.mode can land within milliseconds, so a one-shot "unknown" assertion
  // after navigation could miss it. A MutationObserver installed before the
  // session page loads captures the initial 待检测 deterministically.
  await page.addInitScript(() => {
    const w = window as unknown as { __ttyModes?: string[] };
    w.__ttyModes = [];
    const record = (value: string | null) => {
      if (!value) return;
      const seen = w.__ttyModes!;
      if (seen[seen.length - 1] !== value) seen.push(value);
    };
    // document_start can precede <html>; arm as soon as the root exists.
    const arm = () => {
      const root = document.documentElement;
      if (!root) return false;
      new MutationObserver(() => {
        const el = document.querySelector<HTMLElement>("[data-testid='tty-alt-screen']");
        if (el) record(el.getAttribute("data-alt-screen"));
      }).observe(root, {
        subtree: true,
        childList: true,
        attributes: true,
        attributeFilter: ["data-alt-screen"],
      });
      return true;
    };
    if (!arm()) {
      const wait = setInterval(() => {
        if (arm()) clearInterval(wait);
      }, 0);
    }
  });
  await login(page);
  await startTerminalSession(page);

  const pill = page.getByTestId("tty-alt-screen");

  // Attach reports the primary buffer: inline rendering.
  await expect(pill).toHaveAttribute("data-alt-screen", "false", { timeout: 20_000 });
  await expect(pill).toHaveText("行内渲染");
  await shot(page, "tty-mode-1-inline.png");

  // The harness enters the DEC alternate screen (?1049h).
  await focusTerminal(page);
  await page.keyboard.type("TTYMODE_ALT_ON");
  await page.keyboard.press("Enter");
  await expect(pill).toHaveAttribute("data-alt-screen", "true", { timeout: 15_000 });
  await expect(pill).toHaveText("全屏渲染");
  // The raw frame made xterm paint the switched buffer as well.
  await expect(page.getByTestId("tty-ansi-preview")).toContainText("FULLSCREEN_FAKE_HARNESS");
  await shot(page, "tty-mode-1-fullscreen.png");

  // Leaving the alt screen restores the inline badge.
  await page.keyboard.type("TTYMODE_ALT_OFF");
  await page.keyboard.press("Enter");
  await expect(pill).toHaveAttribute("data-alt-screen", "false", { timeout: 15_000 });
  await expect(pill).toHaveText("行内渲染");
  await shot(page, "tty-mode-1-back-inline.png");

  const observed = await page.evaluate(
    () => (window as unknown as { __ttyModes: string[] }).__ttyModes,
  );
  expect(observed[0], "the badge renders 待检测 before the attach observation").toBe("unknown");
  expect(observed).toContain("true");
  expect(observed[observed.length - 1]).toBe("false");
});
