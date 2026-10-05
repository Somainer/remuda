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
  // 16 pending cards on ONE instance (inbox-focus sentinel). The desktop
  // queue keeps the store's order (it does NOT sort byRecency the way the
  // compact tier does), so the target must be chosen by the order the page
  // ACTUALLY renders — a card past the 12-rows-per-frame first slice that is
  // already below the 720px fold — never by guessing an id ordering.
  const instanceId = await createSession(page, "inbox-focus:16");

  // All 16 cards reach interaction.list.
  await expect
    .poll(
      async () =>
        (await listInteractions(page)).filter(
          (item) => item.instanceId === instanceId && item.state === "pending",
        ).length,
      { timeout: 20_000, message: "16 parked approval cards" },
    )
    .toBe(16);

  // On the UNFOCUSED page, read pending rows in DOM render order (queue
  // cards carry data-state pending/answering/paused; 已离队 rows render after
  // the other tiers and are excluded). The 打开会话 link identifies the
  // owning instance.
  await page.goto("/approvals");
  await expect(page.getByTestId("approvals-page")).toBeVisible();

  type DomRow = { id: string | null; instance: string | null };
  const readDomRows = async (): Promise<DomRow[]> =>
    page.evaluate(() =>
      Array.from(document.querySelectorAll<HTMLElement>('[data-testid="approval-row"]'))
        .filter((el) => {
          const state = el.getAttribute("data-state");
          return state === "pending" || state === "answering" || state === "paused";
        })
        .map((el) => ({
          id: el.getAttribute("data-interaction-id"),
          instance: el.querySelector<HTMLAnchorElement>('a[href^="/s/"]')?.pathname.slice(3) ?? null,
        })),
    );

  // Progressive mount ends with at least 13 pending rows (our 16 guarantee
  // this even with zero leftovers from sibling specs).
  let rows: DomRow[] = [];
  await expect
    .poll(async () => ((rows = await readDomRows()).length), {
      timeout: 20_000,
      message: "first slice plus the next mounted",
    })
    .toBeGreaterThanOrEqual(13);

  // DOM index 12 = the first row past the 12-card first slice. It must be one
  // of THIS instance's cards (the sentinel's 16 are the newest rows in the
  // shared store, appended together, so they tail the pending list).
  const globalIndex = new Map(rows.map((row, index) => [row.id, index]));
  const ownDeep = rows.find(
    (row) => row.instance === instanceId && (globalIndex.get(row.id) ?? -1) >= 12,
  );
  expect(ownDeep, "an own card sits past the first progressive slice").toBeTruthy();
  const target = ownDeep!.id!;

  // Before the deep link that row is rendered but BELOW the fold: a
  // highlight-only implementation would leave the test passing only if the
  // target were visible, so prove it is not.
  const targetRow = page.locator(`[data-interaction-id="${target}"]`);
  await expect(targetRow).toHaveAttribute("data-state", "pending");
  const before = await targetRow.boundingBox();
  expect(before, "deep row rendered").toBeTruthy();
  expect(before!.y, "the unlinked target starts below the fold").toBeGreaterThanOrEqual(VIEWPORT_H);

  // Deep link: the ring is set and the focus effect re-runs as every tier's
  // rAF slices grow, scrolling the border box INSIDE the viewport. The
  // in-viewport predicate is a retrying poll (not a single bounding-box
  // read), so late slices cannot fool it.
  await page.goto(`/approvals?focus=${target}`);
  const focused = page.locator(`[data-interaction-id="${target}"]`);
  await expect(focused).toHaveAttribute("data-focus", "true", { timeout: 20_000 });
  await expect
    .poll(
      async () => {
        const box = await focused.boundingBox();
        if (!box) return false;
        return box.y >= 0 && box.y + box.height <= VIEWPORT_H;
      },
      { timeout: 10_000, message: "deep-linked row scrolled inside the viewport" },
    )
    .toBe(true);

  // Without the focus query the ring is gone (and no scroll is requested).
  await page.goto("/approvals");
  await expect(page.locator("[data-focus='true']")).toHaveCount(0);
});
