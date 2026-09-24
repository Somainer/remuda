import { expect, test, type Locator, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * Coarse-pointer hit areas for the provider model-catalog controls (D-053
 * §8.2): under pointer: coarse the enable label, default-model label and the
 * remove × must each own a non-overlapping 44px-HIGH band. Visible glyph
 * sizes stay compact; the row/label stretches or an ::after supplies reach.
 *
 * Geometry, not getComputedStyle (same contract as ux-touchhit.hub.spec):
 * the label's band reaches 44px high and its edges resolve to itself.
 */
test.describe.configure({ mode: "serial" });

const upstream = process.env.VITE_E2E_UPSTREAM ?? `http://${process.env.HUB_E2E_UPSTREAM_LISTEN ?? "127.0.0.1:58881"}`;
const token = "sk-touch-qqqq";
const TOUCH = 44;

async function openCatalog(page: Page): Promise<void> {
  await login(page, "e2e-provider-touch");
  await page.goto("/providers");
  await expect(page.getByTestId("providers-page")).toBeVisible();
  await page.getByTestId("provider-add").click();
  await expect(page.getByTestId("provider-form")).toBeVisible();
  await page.getByTestId("provider-name").fill("e2e-touch-upstream");
  await page.getByTestId("provider-base-url").fill(`${upstream}/v1`);
  await page.getByTestId("provider-token").fill(token);
  await page.getByTestId("provider-discover").click();
  await expect(page.getByTestId("provider-model-row").first()).toBeVisible();
}

/**
 * A label owns a 44px-high band on a coarse device: its rect is >=44 tall
 * (the row stretches), and a point 21px above/below the centre but within
 * the row lands on the label, not on a neighbour.
 */
async function assertVerticalBand(label: Locator) {
  const rect = await label.boundingBox();
  expect(rect, "label must have a box").not.toBeNull();
  expect(rect!.height).toBeGreaterThanOrEqual(TOUCH - 0.5);
}

test("model catalog enable/default labels and remove × own 44px-high coarse bands", async ({ browser }) => {
  const context = await browser.newContext({
    viewport: { width: 390, height: 844 },
    hasTouch: true,
    isMobile: true,
  });
  const page = await context.newPage();
  try {
    await openCatalog(page);

    const row = page.getByTestId("provider-model-row").first();
    const enableLabel = row.getByTestId("provider-model-enabled").locator("xpath=ancestor::label[1]");
    const defaultLabel = row.getByTestId("provider-model-default").locator("xpath=ancestor::label[1]");
    const removeBtn = row.getByTestId("provider-model-remove");

    // Labels stretch through the 44px row.
    await assertVerticalBand(enableLabel);
    await assertVerticalBand(defaultLabel);

    // The remove × keeps its compact glyph but its pseudo hit zone extends
    // the reach: the row band is 44 and the button is vertically centred,
    // so the 44px zone around its centre stays inside the row.
    const rowRect = await row.boundingBox();
    const removeRect = await removeBtn.boundingBox();
    expect(rowRect!.height).toBeGreaterThanOrEqual(TOUCH - 0.5);
    expect(removeRect!.height).toBeLessThan(TOUCH);
    const centreY = removeRect!.y + removeRect!.height / 2;
    expect(centreY - TOUCH / 2).toBeGreaterThanOrEqual(rowRect!.y - 0.5);
    expect(centreY + TOUCH / 2).toBeLessThanOrEqual(rowRect!.y + rowRect!.height + 0.5);

    // Taps at the upper/lower edges of the enable band land on its label.
    const enableRect = await enableLabel.boundingBox();
    const cx = enableRect!.x + Math.min(20, enableRect!.width / 2);
    const upper = enableRect!.y + 2;
    const lower = enableRect!.y + enableRect!.height - 2;
    for (const y of [upper, lower]) {
      const owner = await page.evaluate(
        ({ x, y }) => document.elementFromPoint(x, y)?.closest("label")?.querySelector("input")?.dataset.testid ?? null,
        { x: cx, y },
      );
      expect(owner).toBe("provider-model-enabled");
    }
  } finally {
    await context.close();
  }
});

test("default-gateway checkbox label is a 44px-high coarse band", async ({ browser }) => {
  const context = await browser.newContext({
    viewport: { width: 390, height: 844 },
    hasTouch: true,
    isMobile: true,
  });
  const page = await context.newPage();
  try {
    await openCatalog(page);
    const toggle = page.getByTestId("provider-default-gateway").locator("xpath=ancestor::label[1]");
    const rect = await toggle.boundingBox();
    expect(rect!.height).toBeGreaterThanOrEqual(TOUCH - 0.5);
  } finally {
    await context.close();
  }
});
