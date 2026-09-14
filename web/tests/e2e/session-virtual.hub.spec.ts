import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * Workbench batch E — Hub + fake-node invariants for in-transcript search.
 *
 * Runs ONLY under playwright.hub.config.ts (file name matches
 * /\.hub\.spec\.ts$/); the mock/Vite geometry cases live in
 * session-virtual.spec.ts. Everything here goes through the fake node from
 * crates/remuda-hub/examples/hub_e2e.rs — no real model is ever called.
 *
 * - searching is read-only: it never writes the journal or drives the native
 *   session;
 * - reading position and follow state survive leaving and re-entering;
 * - a streamed open/append/close turn produces a bounded number of
 *   polite-live-region changes and never leaks body text into it.
 */

/** Every mutating native-driving request; searching must add none. */
const NATIVE_ACTION = /\/v1\/instances\/[^/]+\/(?:commands|input|interrupt)$|\/v1\/interactions\/[^/]+\/answer$/;
const created: string[] = [];
// afterAll has no TestInfo; remember whether hub tests actually ran so the
// restore only happens in a live Hub run.
let hubRan = false;

test.beforeEach(async ({ page }) => {
  await login(page);
  await raiseCap(page, 24);
  hubRan = true;
});

test.afterEach(async ({ page }) => {
  for (const id of created.splice(0)) {
    await page.evaluate(async (instanceId) => {
      await fetch(`/v1/instances/${instanceId}?force=1`, { method: "DELETE", credentials: "include" }).catch(() => {});
    }, id);
  }
});

test.afterAll(async ({ browser }) => {
  if (!hubRan) return;
  // Restore the shared fake node's fixture default (maxInstances:8) for the
  // specs that run later in the serial suite.
  const page = await browser.newPage();
  try {
    await login(page);
    const hosts = await page.evaluate(async () => {
      const response = await fetch("/v1/hosts", { credentials: "include" });
      return response.json() as Promise<{ items?: { hostId?: string; id?: string; label?: string }[] }>;
    });
    const host = (hosts.items ?? []).find((h) => h.label === "e2e-fake-node");
    if (host) {
      await page.evaluate(
        async (id) => {
          await fetch(`/v1/hosts/${id}`, {
            method: "PATCH",
            credentials: "include",
            headers: { "content-type": "application/json" },
            body: JSON.stringify({ maxInstances: 8 }),
          }).catch(() => {});
        },
        (host.hostId ?? host.id) as string,
      );
    }
  } finally {
    await page.close();
  }
});

async function createSession(page: Page, prompt: string): Promise<string> {
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  await expect(hostPicker).toContainText("e2e-fake-node", { timeout: 20_000 });
  const hostId = await hostPicker.locator("option").filter({ hasText: "e2e-fake-node" }).getAttribute("value");
  expect(hostId).toBeTruthy();
  await hostPicker.selectOption(hostId!);
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0, { timeout: 20_000 });
  await page.getByTestId("new-session-prompt").fill(prompt);
  await expect(page.getByTestId("new-session-start")).toBeEnabled();
  const creating = page.waitForResponse(
    (response) => response.request().method() === "POST" && new URL(response.url()).pathname === "/v1/instances",
  );
  await page.getByTestId("new-session-start").click();
  const response = await creating;
  expect(response.ok()).toBe(true);
  const instanceId = (await response.json()).instance.instanceId as string;
  created.push(instanceId);
  await expect(page).toHaveURL(new RegExp(`/s/${instanceId}`), { timeout: 20_000 });
  return instanceId;
}

async function answerPending(page: Page, instanceId: string): Promise<void> {
  // Every instance.create raises a pending approval that disables the
  // composer; answer it before sending.
  await page.evaluate(async (id) => {
    const list = await fetch("/v1/interactions", { credentials: "include" });
    const body = (await list.json()) as {
      items?: {
        id: string;
        instanceId?: string;
        state?: string;
        request?: { inputDigest?: string; options?: { id: string }[] };
      }[];
    };
    for (const item of body.items ?? []) {
      if (item.instanceId !== id || item.state !== "pending") continue;
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
}

async function readySession(page: Page, prompt: string): Promise<string> {
  const instanceId = await createSession(page, prompt);
  await answerPending(page, instanceId);
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 30_000 });
  return instanceId;
}

async function sendTurn(page: Page, prompt: string): Promise<void> {
  const composer = page.getByTestId("composer-input");
  await composer.fill(prompt);
  await composer.press("Enter");
  await expect(page.getByTestId("transcript")).toContainText(`echo: ${prompt}`, { timeout: 30_000 });
}

test("search is read-only: it never writes the journal or drives the native session", async ({ page }) => {
  const marker = "zulu-4173";
  await readySession(page, `seed ${marker}`);
  await sendTurn(page, `followup ${marker}`);

  const nativeWrites: string[] = [];
  page.on("request", (request) => {
    if (NATIVE_ACTION.test(new URL(request.url()).pathname) && request.method() !== "GET") {
      nativeWrites.push(request.method());
    }
  });

  await page.getByTestId("transcript-search-open").click();
  const input = page.getByTestId("transcript-search-input");
  await input.fill(marker);
  await expect(page.getByTestId("transcript-search-count")).not.toHaveText("0/0");
  await input.press("Enter");
  await expect(page.locator('[data-search-current="1"]').first()).toBeVisible();
  await page.getByTestId("transcript-search-prev").click();
  await page.getByTestId("transcript-search-next").click();
  await page.getByTestId("transcript-search-close").click();
  await page.waitForTimeout(500);
  expect(nativeWrites, `search issued native actions: ${nativeWrites.join(", ")}`).toEqual([]);
});

test("reading position and follow state survive leaving and re-entering", async ({ page }) => {
  const instanceId = await readySession(page, "batch-e restore seed");
  // Make the transcript scrollable WITHOUT relying on multiple turns: the
  // fake node keeps reporting working after a send, so a second Enter
  // becomes a queue rather than a turn. One long wrapping prompt produces a
  // tall user bubble and an equally tall echo from a single instance.send.
  const filler = `restore-filler ${Array.from({ length: 200 }, (_, i) => `line-${i}-padding`).join(" ")}`;
  const composer = page.getByTestId("composer-input");
  await composer.fill(filler);
  await composer.press("Enter");
  await expect(page.getByTestId("transcript")).toContainText("echo: restore-filler", { timeout: 30_000 });
  const scroller = page.getByTestId("transcript-scroller");
  await expect.poll(async () => scroller.evaluate((el) => el.scrollHeight - el.clientHeight)).toBeGreaterThan(600);

  await scroller.evaluate((el) => {
    el.scrollTop = 0;
    el.dispatchEvent(new Event("scroll", { bubbles: true }));
  });
  await expect(page.getByTestId("jump-latest")).toBeVisible();
  await page.waitForTimeout(300);

  await page.goto("/sessions");
  await expect(page.getByTestId("session-list")).toBeVisible();
  await page.goto(`/s/${instanceId}`);
  await expect(page.getByTestId("transcript-row")).not.toHaveCount(0, { timeout: 15_000 });
  expect(await scroller.evaluate((el) => el.scrollTop)).toBeLessThan(160);

  // Re-pin and confirm follow itself is what restores.
  await scroller.evaluate((el) => {
    el.scrollTop = el.scrollHeight;
    el.dispatchEvent(new Event("scroll", { bubbles: true }));
  });
  await page.waitForTimeout(300);
  await page.goto("/sessions");
  await page.goto(`/s/${instanceId}`);
  await expect(page.getByTestId("transcript-row")).not.toHaveCount(0, { timeout: 15_000 });
  const atBottom = await scroller.evaluate((el) => el.scrollHeight - el.scrollTop - el.clientHeight < 64);
  expect(atBottom).toBe(true);
});

test("streaming transcript text produces a bounded number of live-region changes", async ({ page }) => {
  await readySession(page, "batch-e stream seed");
  const region = page.getByTestId("live-region");
  await expect(region).toHaveAttribute("aria-live", "polite");
  await expect(page.getByTestId("transcript")).toHaveAttribute("aria-live", "off");

  // Count polite-region DOM mutations across the streamed turn. The counter
  // lives on window because an evaluate() return value cannot keep a
  // MutationObserver alive.
  await page.evaluate(() => {
    const el = document.querySelector('[data-testid="live-region"]');
    const w = window as unknown as { __batchELiveChanges?: number };
    w.__batchELiveChanges = 0;
    if (el) {
      new MutationObserver(() => {
        w.__batchELiveChanges = (w.__batchELiveChanges ?? 0) + 1;
      }).observe(el, { childList: true, characterData: true, subtree: true });
    }
  });

  await sendTurn(page, "stream bounded-region-delta-9931");
  // Let any debounced announcement settle.
  await page.waitForTimeout(800);
  const count = await page.evaluate(() => (window as unknown as { __batchELiveChanges?: number }).__batchELiveChanges ?? 0);
  // The whole open/append/close chain streamed body text: the polite region
  // may carry unrelated app noise, but must not machine-gun once per chunk.
  expect(count).toBeLessThanOrEqual(4);
  const announced = (await region.textContent()) ?? "";
  expect(announced).not.toContain("bounded-region-delta");
});

/** maxInstances:8 is shared across every hub spec; raise it for this file. */
async function raiseCap(page: Page, to: number): Promise<void> {
  const hosts = await page.evaluate(async () => {
    const response = await fetch("/v1/hosts", { credentials: "include" });
    return response.json() as Promise<{
      items?: { hostId?: string; id?: string; label?: string; maxInstances?: number }[];
    }>;
  });
  const host = (hosts.items ?? []).find((h) => h.label === "e2e-fake-node");
  if (!host) return;
  const hostId = (host.hostId ?? host.id) as string;
  if ((host.maxInstances ?? 8) >= to) return;
  await page.evaluate(
    async ({ id, value }) => {
      await fetch(`/v1/hosts/${id}`, {
        method: "PATCH",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ maxInstances: value }),
      });
    },
    { id: hostId, value: to },
  );
}
