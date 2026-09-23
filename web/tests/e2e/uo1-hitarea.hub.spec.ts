import { expect, test, type Locator, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * Coarse-pointer hit areas of the shared chip and segmented controls
 * (visual-system.md §6.2, ui-spec.md §3.4): 24px chips and 26px segment
 * items reach 44px through an invisible `::after`, and those zones must never
 * overlap — including between wrapped rows. Geometry is the contract:
 * elementFromPoint at every corner of a control's 44px zone must land on that
 * control. If two zones overlapped, the later-painted one would own the
 * shared corner.
 */

test.use({ hasTouch: true });

const TOUCH = 44;
/** Stay a pixel inside the zone so sub-pixel rounding cannot miss it. */
const IN = 1;

let instanceId = "";

async function createSession(page: Page): Promise<string> {
  return page.evaluate(async () => {
    const hosts = (await (await fetch("/v1/hosts", { credentials: "include" })).json()) as {
      items?: { hostId?: string; label?: string }[];
    };
    const hostId = hosts.items?.find((host) => host.label === "e2e-fake-node")?.hostId ?? hosts.items?.[0]?.hostId;
    const response = await fetch("/v1/instances", {
      method: "POST",
      credentials: "include",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ hostId, workspaceId: "/tmp", kind: "claude", prompt: "UO-1 hit areas" }),
    });
    if (!response.ok) throw new Error(`create ${response.status}`);
    return ((await response.json()) as { instance: { instanceId: string } }).instance.instanceId;
  });
}

test.beforeEach(async ({ page }) => {
  await login(page);
});

test.afterAll(async ({ browser }) => {
  if (!instanceId) return;
  const page = await browser.newPage();
  try {
    await login(page);
    await page.request.delete(`/v1/instances/${instanceId}?force=1`).catch(() => undefined);
  } finally {
    await page.close();
  }
});

/** Every corner of each control's vertical 44px zone belongs to that control. */
async function assertOwnedZones(controls: Locator, name: string): Promise<number[]> {
  const report = await controls.evaluateAll(
    (elements, { touch, inset }) =>
      elements.map((el, index) => {
        const box = el.getBoundingClientRect();
        const cy = box.top + box.height / 2;
        const zoneH = Math.max(box.height, touch);
        const top = cy - zoneH / 2 + inset;
        const bottom = cy + zoneH / 2 - inset;
        const corners = [
          [box.left + inset, top],
          [box.right - inset, top],
          [box.left + inset, bottom],
          [box.right - inset, bottom],
        ] as const;
        const lost = corners
          .filter(([x, y]) => {
            const hit = document.elementFromPoint(x, y);
            return !(hit && (hit === el || el.contains(hit)));
          })
          .map(([x, y]) => {
            const hit = document.elementFromPoint(x, y);
            return `${Math.round(x)},${Math.round(y)}→${hit ? `${hit.tagName.toLowerCase()}:${hit.textContent?.slice(0, 20)}` : "none"}`;
          });
        return { index, text: el.textContent?.trim() ?? "", top: Math.round(box.top), lost };
      }),
    { touch: TOUCH, inset: IN },
  );
  const failures = report.filter((entry) => entry.lost.length);
  expect(failures, `${name}: every zone corner is owned by its control`).toEqual([]);
  return report.map((entry) => entry.top);
}

test("wrapped raw-event filter chips own their 44px zones at 390", async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.emulateMedia({ reducedMotion: "reduce" });
  instanceId = await createSession(page);
  await page.goto(`/s/${instanceId}/events`);
  const events = page.getByTestId("raw-events");
  await expect(events).toBeVisible({ timeout: 20_000 });
  // The filter row is the first block under the count line.
  const filters = events.locator("> div").first().locator("button");
  await expect(filters.first()).toBeVisible();

  const tops = await assertOwnedZones(filters, "raw-events filters");
  // The case under test: the chips really wrap onto several rows.
  expect(new Set(tops).size, "filter chips wrap onto more than one row").toBeGreaterThan(1);
});

test("the settings appearance segments own their 44px zones at 390", async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.emulateMedia({ reducedMotion: "reduce" });
  await page.goto("/settings");
  const seg = page.getByTestId("settings-appearance");
  await expect(seg).toBeVisible();
  await seg.scrollIntoViewIfNeeded();
  await assertOwnedZones(seg.getByRole("radio"), "appearance segments");
});
