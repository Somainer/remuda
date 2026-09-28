import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * c-inboxfu — desktop /approvals gaps from the UO-9 review:
 *
 *  - the middle 进行中 · 最近 tier the single-shell spec requires (ui-spec
 *    §2.5, D-052): live working/idle instances from the SAME
 *    deriveRecentInstances projection the compact inbox renders;
 *  - ?focus=<id> deep links must SCROLL (not only ring-highlight) a row that
 *    mounts beyond the progressive-mount first slice (12 rows/frame).
 *
 * The matching compact coverage lives in m-inbox.hub.spec.ts.
 */
test.describe.configure({ mode: "serial" });

// playwright.hub.config.ts renders Desktop Chrome at 1280x720.
const VIEWPORT_H = 720;

const created: string[] = [];

test.beforeEach(async ({ page }) => {
  await login(page);
});

test.afterEach(async ({ page }) => {
  for (const id of created.splice(0)) {
    await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
  }
});

async function createSession(page: Page, prompt: string): Promise<string> {
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
  await page.getByTestId("new-session-prompt").fill(prompt);
  await page.getByTestId("new-session-start").click();
  await page.waitForURL(/\/s\//, { timeout: 20_000 });
  const id = new URL(page.url()).pathname.split("/").pop()!;
  created.push(id);
  return id;
}

type ListItem = {
  id: string;
  instanceId?: string;
  state?: string;
  updatedAt?: string;
  createdAt?: string;
  request?: { kind?: string; inputDigest?: string; options?: { id: string }[] };
};

async function listInteractions(page: Page): Promise<ListItem[]> {
  const body = await page.evaluate(async () => {
    const res = await fetch("/v1/interactions", { credentials: "include" });
    return (await res.json()) as { items?: ListItem[] };
  });
  return body.items ?? [];
}

/** Answer the first still-pending approval of one instance through the API. */
async function answerInstanceApprovals(page: Page, instanceId: string): Promise<void> {
  await expect
    .poll(
      async () => {
        const items = (await listInteractions(page)).filter(
          (item) => item.instanceId === instanceId && item.state === "pending",
        );
        for (const item of items) {
          const optionId = item.request?.options?.[0]?.id;
          if (!optionId) continue;
          await page.evaluate(
            async ({ iid, optionId, digest }) => {
              await fetch(`/v1/interactions/${iid}/answer`, {
                method: "POST",
                credentials: "include",
                headers: { "content-type": "application/json" },
                body: JSON.stringify({
                  answer: { kind: "approval", optionId, inputDigest: digest ?? "" },
                }),
              });
            },
            { iid: item.id, optionId, digest: item.request?.inputDigest },
          );
        }
        return items.length;
      },
      { timeout: 20_000, message: "launch approval answered" },
    )
    .toBeGreaterThan(0);
}

test("进行中 · 最近 tier lists the answered (now idle) instance", async ({ page }) => {
  const instanceId = await createSession(page, "desktop inbox recent tier");
  await answerInstanceApprovals(page, instanceId);

  await page.goto("/approvals");
  await expect(page.getByTestId("approvals-page")).toBeVisible();

  // The middle tier the compact inbox already had: same derivation, desktop
  // chrome. The row links at the live session and carries its instance id.
  const tier = page.getByTestId("approvals-recent");
  await expect(tier).toBeVisible({ timeout: 20_000 });
  await expect(page.getByTestId("approvals-tier-recent")).toContainText("进行中 · 最近");
  const row = page
    .getByTestId("approvals-recent-row")
    .filter({ has: page.locator(`a[href="/s/${instanceId}"]`) });
  await expect(row).toBeVisible();
  await expect(row).toHaveAttribute("data-instance-id", instanceId);
});

test("?focus= scrolls a deep-linked row into the desktop viewport", async ({ page }) => {
  // 16 pending cards on ONE instance (inbox-focus sentinel): the target sorts
  // below the 12-rows-per-frame first slice AND below the 720px fold, so a
  // highlight-only implementation leaves it off-screen.
  const instanceId = await createSession(page, "inbox-focus:16");

  await expect
    .poll(
      async () =>
        (await listInteractions(page)).filter(
          (item) => item.instanceId === instanceId && item.state === "pending",
        ).length,
      { timeout: 20_000, message: "16 parked approval cards" },
    )
    .toBe(16);

  // The row the UI sorts LAST globally (newest-first, id tiebreak) is the
  // deepest deep link; recompute over ALL pending rows exactly like
  // inboxRows.byRecency so leftovers from sibling specs cannot fool the rank.
  const allPending = (await listInteractions(page)).filter((item) => item.state === "pending");
  const asc = [...allPending].sort((a, b) => {
    const ta = Date.parse(a.updatedAt || a.createdAt || "");
    const tb = Date.parse(b.updatedAt || b.createdAt || "");
    if (ta !== tb) return ta - tb;
    return a.id < b.id ? -1 : a.id > b.id ? 1 : 0;
  });
  const target = asc[0]!.id;
  expect(target).toBeTruthy();

  await page.goto(`/approvals?focus=${target}`);
  const focused = page.locator(`[data-interaction-id="${target}"]`);
  await expect(focused).toHaveAttribute("data-focus", "true", { timeout: 20_000 });

  // Highlight alone is not enough: the focus effect re-runs as the rAF slices
  // grow and must scroll the row's border box inside the 720px viewport.
  await expect
    .poll(
      async () => {
        const box = await focused.boundingBox();
        if (!box) return null;
        return { y: box.y, bottom: box.y + box.height };
      },
      { timeout: 10_000 },
    )
    .toMatchObject({ y: expect.any(Number), bottom: expect.any(Number) });
  const box = await focused.boundingBox();
  expect(box, "focused row rendered").toBeTruthy();
  expect(box!.y).toBeGreaterThanOrEqual(0);
  expect(box!.y + box!.height).toBeLessThanOrEqual(VIEWPORT_H);

  // Without the focus query the ring is gone (and no scroll is requested).
  await page.goto("/approvals");
  await expect(page.locator("[data-focus='true']")).toHaveCount(0);
});
