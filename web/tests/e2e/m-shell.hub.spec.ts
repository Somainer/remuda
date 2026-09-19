import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * c-mshell / D-049: the phone-first /m route tree, its viewport redirect
 * layer, and the phone bottom bar.
 *
 * - 390px: post-login lands on /m; the four bottom-bar entries exist and each
 *   owns at least 44px of hit area; /sessions -> /m and /approvals?focus= ->
 *   /m/inbox?focus= with the query preserved verbatim; shared routes
 *   (/s/:id, /sessions/new, /settings) are never rewritten.
 * - 1440px: /m* bounces to /sessions; start_url "/" lands on /sessions.
 *
 * /m renders the c-mhome phone home (HomeList) and /m/inbox renders the
 * c-minbox phone inbox (Inbox); the interim SessionsPage/ApprovalsPage
 * placeholders are gone.
 */
test.describe.configure({ mode: "serial" });

const TOUCH = 44;

const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence
  ? path.join(here, "../../../docs/design/evidence")
  : path.join(here, "../../test-results/evidence");

const created: string[] = [];

async function shot(page: Page, name: string) {
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

async function createSession(page: Page, prompt: string, workspaceId?: string): Promise<string> {
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  await expect(hostPicker).toContainText("e2e-fake-node", { timeout: 20_000 });
  const hostId = await hostPicker
    .locator("option")
    .filter({ hasText: "e2e-fake-node" })
    .getAttribute("value");
  expect(hostId).toBeTruthy();
  await hostPicker.selectOption(hostId!);
  await page.getByTestId("new-session-kind-claude").click();
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0, {
    timeout: 20_000,
  });
  if (workspaceId) await page.getByTestId("new-session-workspace").selectOption(workspaceId);
  await page.getByTestId("new-session-prompt").fill(prompt);
  await page.getByTestId("new-session-start").click();
  await page.waitForURL(/\/s\//, { timeout: 20_000 });
  const id = new URL(page.url()).pathname.split("/").pop()!;
  created.push(id);
  return id;
}

/** First pending interaction id on a created instance. */
async function pendingInteractionId(page: Page, instanceId: string): Promise<string> {
  const id = await page.evaluate(async (iid) => {
    const body = await (await fetch("/v1/interactions", { credentials: "include" })).json();
    const found = (body.items ?? []).find(
      (item: { instanceId?: string; state?: string; interactionId?: string; id?: string }) =>
        item.instanceId === iid && item.state === "pending",
    );
    return found?.interactionId ?? found?.id ?? null;
  }, instanceId);
  expect(id, "the fake node raises a pending approval for a fresh session").toBeTruthy();
  return id as string;
}

async function forceDeleteAllInstances(page: Page) {
  // Mirrors native-progress.hub.spec.ts: the raw list key is `instanceId`
  // (not `id`) and the call must be an explicit DELETE — a GET to the
  // item URL 405s, and a silently swept-under catch leaves live rows
  // holding fake-node placement slots for later serial specs.
  await page.evaluate(async () => {
    const list = await fetch("/v1/instances", { credentials: "include" });
    const body = (await list.json()) as {
      items?: { instanceId?: string; lifecycle?: string }[];
    };
    await Promise.all(
      (body.items ?? [])
        .filter((instance) => instance.instanceId)
        .filter(
          (instance) =>
            instance.lifecycle !== "exited" &&
            instance.lifecycle !== "failed" &&
            instance.lifecycle !== "closed",
        )
        .map(async (instance) => {
          const response = await fetch(
            `/v1/instances/${instance.instanceId}?force=1`,
            { method: "DELETE", credentials: "include" },
          );
          if (!response.ok) throw new Error(`force delete failed: ${response.status}`);
        }),
    );
  });
}

test.describe("390px phone", () => {
  test.use({
    viewport: { width: 390, height: 844 },
    hasTouch: true,
    isMobile: true,
  });

  test.beforeEach(async ({ page }) => {
    await login(page);
  });

  test.afterEach(async ({ page }) => {
    for (const id of created.splice(0)) {
      await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    }
  });

  test("after login the app lands on /m and the bottom bar has four 44px entries", async ({
    page,
  }) => {
    await expect(page).toHaveURL(/\/m$/);
    const bar = page.getByRole("navigation", { name: "手机底栏" });
    await expect(bar).toBeVisible();

    // Exactly four entries, in the D-049 order.
    const entries = ["phone-nav-home", "phone-nav-inbox", "phone-nav-new", "phone-nav-more"];
    for (const testid of entries) {
      const target = page.getByTestId(testid);
      await expect(target).toBeVisible();
      const box = await target.boundingBox();
      expect(box, `${testid} rendered`).toBeTruthy();
      expect(box!.width, `${testid} width >= ${TOUCH}px`).toBeGreaterThanOrEqual(TOUCH);
      expect(box!.height, `${testid} height >= ${TOUCH}px`).toBeGreaterThanOrEqual(TOUCH);
      // The whole hit box stays inside the viewport.
      expect(box!.x).toBeGreaterThanOrEqual(0);
      expect(box!.x + box!.width).toBeLessThanOrEqual(390);
    }
    await expect(page.getByTestId("phone-nav-home")).toContainText("会话");
    await expect(page.getByTestId("phone-nav-inbox")).toContainText("收件箱");
    await expect(page.getByTestId("phone-nav-new")).toHaveAttribute("aria-label", "新建");
    await expect(page.getByTestId("phone-nav-more")).toContainText("更多");

    // m-home: /m renders the phone HomeList groups (the placeholder
    // SessionsPage list now only lives at /sessions on desktop).
    await expect(page.getByTestId("home-list")).toBeVisible();

    // start_url "/" resolves to the phone home in compact.
    await page.goto("/");
    await expect(page).toHaveURL(/\/m$/);
  });

  test("bottom bar entries navigate, the badge counts pending interactions, and deep links keep ?focus=", async ({
    page,
  }) => {
    const instanceId = await createSession(page, "m shell badge synthetic");
    const interactionId = await pendingInteractionId(page, instanceId);

    await page.goto("/m");
    // Badge rule identical to Shell.tsx: pending interactions only.
    await expect(page.getByTestId("phone-inbox-badge")).toHaveText("1", { timeout: 15_000 });

    // 收件箱 opens the phone inbox (c-minbox replaced the interim
    // ApprovalsPage placeholder here; the desktop centre is unchanged).
    await page.getByTestId("phone-nav-inbox").click();
    await expect(page).toHaveURL(/\/m\/inbox$/);
    await expect(page.getByTestId("m-inbox")).toBeVisible();

    // Deep link: /approvals?focus= is carried to /m/inbox?focus= verbatim,
    // and the phone inbox marks the focused interaction row.
    await page.goto(`/approvals?focus=${interactionId}`);
    await expect(page).toHaveURL(`/m/inbox?focus=${interactionId}`);
    await expect(page.getByTestId("m-inbox")).toBeVisible();
    await expect(page.locator(`[data-interaction-id="${interactionId}"][data-focus="true"]`)).toBeVisible();

    // The /sessions redirect preserves its query as well. The list itself
    // prunes host=/workspace= filters that its fixed Space scope cannot
    // honour (existing desktop behaviour), so assert with a status filter
    // plus an unrelated param, which the list never rewrites on mount.
    await page.goto("/sessions?status=idle&focus=int_keep");
    await expect(page).toHaveURL(/\/m\?status=idle&focus=int_keep$/);

    // Compact /sessions itself redirects.
    await page.goto("/sessions");
    await expect(page).toHaveURL(/\/m$/);

    // 新建 keeps the shared new-session route (no redirect either side);
    // from home it carries the active space like the desktop rail's +.
    // Select a known space first (this test's session lives in another
    // space), then assert the carry concretely rather than accepting the
    // bare route.
    await page.goto("/m");
    const changedChip = page
      .getByTestId("space-chip")
      .filter({ hasText: /^changed/ })
      .first();
    await changedChip.click();
    await expect(page).toHaveURL(/\/m$/);
    await expect(changedChip).toHaveAttribute("aria-pressed", "true");
    await page.getByTestId("phone-nav-new").click();
    await expect(page).toHaveURL(/\/sessions\/new\?/);
    const newUrl = new URL(page.url());
    expect(newUrl.searchParams.get("workspace")).toBe("wsp_g2_changed");
    expect(newUrl.searchParams.get("host")).toBeTruthy();
    await expect(page.getByTestId("new-session-host")).toBeVisible();
    await expect(page.getByTestId("new-session-workspace")).toHaveValue("wsp_g2_changed");

    // 更多 opens the workbench destinations; 更多 closes on navigation.
    await page.goto("/m");
    await page.getByTestId("phone-nav-more").click();
    const menu = page.getByRole("menu");
    await expect(menu).toBeVisible();
    await expect(menu.getByRole("menuitem", { name: "设置" })).toBeVisible();
    await menu.getByRole("menuitem", { name: "设置" }).click();
    await expect(page).toHaveURL(/\/settings$/);

    // Shared routes are never rewritten in compact.
    await page.goto(`/s/${instanceId}`);
    await expect(page).toHaveURL(new RegExp(`/s/${instanceId}$`));
    await expect(page.getByTestId("session-page")).toBeVisible();
    await page.goto("/sessions/new");
    await expect(page).toHaveURL(/\/sessions\/new$/);
    await expect(page.getByTestId("new-session-host")).toBeVisible();
  });

  test("returning to the foreground refreshes the home (Shell visibilitychange equivalence)", async ({
    page,
  }) => {
    await page.goto("/m");
    await expect(page.getByTestId("home-list")).toBeVisible();

    // Sync to a poll boundary so the timer-driven refresh is ~2s away and the
    // only fetch that can follow the dispatched event is PhoneShell's own
    // visibilitychange -> hubStore.refresh() listener.
    await page.waitForRequest((r) => r.url().includes("/v1/instances") && r.method() === "GET");
    const sawEventDrivenRefresh = await page.evaluate(async () => {
      const calls: string[] = [];
      const original = window.fetch;
      window.fetch = (input: RequestInfo | URL, init?: RequestInit) => {
        calls.push(String(typeof input === "string" ? input : input instanceof URL ? input.href : input.url));
        return original(input, init);
      };
      Object.defineProperty(document, "visibilityState", { configurable: true, get: () => "visible" });
      document.dispatchEvent(new Event("visibilitychange"));
      await new Promise((resolve) => setTimeout(resolve, 400));
      window.fetch = original;
      return calls.some((url) => url.includes("/v1/instances"));
    });
    expect(sawEventDrivenRefresh, "visibilitychange triggers hubStore.refresh() on /m").toBe(true);
  });

  test("evidence: phone home at 390px with a pending approval", async ({ page }) => {
    test.skip(!evidence, "set REMUDA_EVIDENCE=1 to capture the committed screenshot");
    await page.emulateMedia({ reducedMotion: "reduce" });
    await createSession(page, "m shell evidence shot", "wsp_g2_changed");
    await page.goto("/m");
    await expect(page.getByTestId("phone-inbox-badge")).toHaveText("1", { timeout: 15_000 });
    await expect(page.getByTestId("home-row")).toHaveCount(1, { timeout: 15_000 });
    await shot(page, "mobile-ui-2-home-390.png");
  });
});

test.describe("1440px desktop", () => {
  test.beforeEach(async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    await login(page);
  });

  // Same per-test drain as the 390 describe: without it the evidence
  // test's instance outlives the test and holds a fake-node placement
  // slot for later serial specs.
  test.afterEach(async ({ page }) => {
    for (const id of created.splice(0)) {
      await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    }
  });

  test.afterAll(async ({ browser }) => {
    const page = await browser.newPage();
    await page.setViewportSize({ width: 1440, height: 900 });
    await login(page);
    await forceDeleteAllInstances(page).catch(() => undefined);
    await page.close();
  });

  test("/m and /m/inbox bounce to /sessions and the start_url lands on /sessions", async ({
    page,
  }) => {
    await page.goto("/m");
    await expect(page).toHaveURL(/\/sessions$/);
    await expect(page.getByTestId("session-list")).toBeVisible();

    await page.goto("/m/inbox?focus=int_demo");
    await expect(page).toHaveURL(/\/sessions$/);

    await page.goto("/");
    await expect(page).toHaveURL(/\/sessions$/);

    // Desktop keeps the desktop approvals centre; the query stays.
    await page.goto("/approvals?focus=int_demo");
    await expect(page).toHaveURL(/\/approvals\?focus=int_demo$/);
  });

  test("evidence: desktop /sessions at 1440px with the same data", async ({ page }) => {
    test.skip(!evidence, "set REMUDA_EVIDENCE=1 to capture the committed screenshot");
    await page.emulateMedia({ reducedMotion: "reduce" });
    await createSession(page, "m shell evidence shot", "wsp_g2_changed");
    await page.goto("/sessions");
    await expect(page.getByTestId("session-row")).toHaveCount(1, { timeout: 15_000 });
    await shot(page, "mobile-ui-2-desktop-1440.png");
  });
});
