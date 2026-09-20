import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * c-mhome / D-049 §4.7: the /m phone home list at 390 px.
 *
 * The in-process fake node (crates/remuda-hub/examples/hub_e2e.rs) produces:
 *  - an "mhome-blocked" session: parked on a pending approval (body sentence
 *    "echo e2e", a transcript/interaction-only word), native status blocked,
 *    with one usage observation so its remaining-context ring reads 50%;
 *  - an "mhome-exit" session: reports a native session id then exits with a
 *    lastError (MHOME_EXIT_SENTINEL) that owns the row body, resumable.
 *
 * Both live in the default wsp_e2e workspace, so they share one
 * project + git branch group whose blocked count is 1.
 */

test.describe.configure({ mode: "serial" });

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
  const workspacePicker = page.getByTestId("new-session-workspace");
  await expect(workspacePicker.locator("option")).not.toHaveCount(0, { timeout: 20_000 });
  await workspacePicker.selectOption("wsp_e2e");
  await page.getByTestId("new-session-prompt").fill(prompt);
  await page.getByTestId("new-session-start").click();
  await page.waitForURL(/\/s\//, { timeout: 20_000 });
  const id = new URL(page.url()).pathname.split("/").pop()!;
  created.push(id);
  return id;
}

test.describe("390px phone home", () => {
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

  test("groups project+branch with the blocked count, pins blocked, shows the ring and no wire strings", async ({
    page,
  }) => {
    const blockedTitle = "MHOME_BLOCKED_TITLE mhome-blocked approval gate";
    const exitedTitle = "MHOME_EXIT_TITLE mhome-exit agent";
    const blockedId = await createSession(page, blockedTitle);
    const exitedId = await createSession(page, exitedTitle);

    await page.goto("/m");
    const home = page.getByTestId("home-list");
    await expect(home).toBeVisible();

    // One project group: name + live git branch (changes proxy) + blocked
    // count equal to buildSpaces() blockedCount (exactly the one blocked row).
    const group = page.getByTestId("home-group").filter({ hasText: "remuda-e2e" });
    await expect(group).toBeVisible();
    await expect(group).toContainText("feat/workbench-g2", { timeout: 15_000 });
    await expect(group.locator("header")).toContainText("1 待处理");
    expect(await page.getByTestId("home-group").count()).toBe(1);

    // Blocked row is pinned to the top of the group even though the exited
    // session was created later.
    const rows = group.getByTestId("home-row");
    await expect(rows).toHaveCount(2);
    await expect(rows.first()).toHaveAttribute("data-status", "blocked");
    await expect(rows.first()).toHaveAttribute("data-blocked", "1");
    expect(await rows.nth(1).getAttribute("data-status")).toBe("exited");

    // Row = dot + title + one next-step sentence verbatim + context ring.
    const blockedRow = rows.filter({ hasText: blockedTitle });
    await expect(blockedRow.getByTestId("home-row-body")).toHaveText("echo e2e");
    await expect(blockedRow.getByTestId("context-ring")).toBeVisible({ timeout: 15_000 });
    await expect(blockedRow.getByTestId("context-ring")).toHaveAttribute("data-pct", "50");
    await expect(blockedRow.getByTestId("context-ring-pct")).toHaveText("50%");

    // Error text owns the exited row's body slot, with the full text on title.
    const exitedRow = rows.filter({ hasText: exitedTitle });
    const exitedBody = exitedRow.getByTestId("home-row-body");
    await expect(exitedBody).toHaveAttribute("data-error", "1");
    await expect(exitedBody).toHaveText(/MHOME_EXIT_SENTINEL/);
    expect(await exitedBody.getAttribute("title")).toMatch(/MHOME_EXIT_SENTINEL \(429\)/);
    // No usage observation ever landed on that session: null contextPct
    // means neither ring nor number renders (ui-spec §3.3).
    await expect(exitedRow.getByTestId("context-ring")).toHaveCount(0);

    // Acceptance 1 (D-038 text-regex rule): no wire triplet, short id, driver
    // or model anywhere in the rendered home, not just hidden behind testids.
    const visible = await home.evaluate((el) => el.innerText);
    expect(visible).not.toMatch(/ins_[a-z0-9]/i);
    expect(visible).not.toMatch(/waiting-interaction|connected|disconnected|reconciling/);
    expect(visible).not.toMatch(/shell-pty|claude-pty|claude-print|generic-pty/);
    expect(visible).not.toMatch(/e2e\/auto|driver|model/i);
    // The instance ids must not leak either.
    expect(visible).not.toContain(blockedId);
    expect(visible).not.toContain(exitedId);

    await shot(page, "mobile-ui-3-home-390.png");
  });

  test("search matches title/project/host only; transcript-only and error-body words return nothing", async ({
    page,
  }) => {
    await createSession(page, "MHOME_FIND_TITLE mhome-blocked approval gate");
    await createSession(page, "MHOME_GONE_TITLE mhome-exit agent");
    await page.goto("/m");
    await expect(page.getByTestId("home-group")).toBeVisible();

    const search = page.getByTestId("home-search");

    // Title word matches.
    await search.fill("MHOME_FIND_TITLE");
    await expect(page.getByTestId("home-row")).toHaveCount(1, { timeout: 10_000 });
    await expect(page.getByTestId("home-row")).toContainText("MHOME_FIND_TITLE");

    // A word that exists only in the interaction/approval body sentence
    // ("echo e2e" is the blocked row's next step) matches nothing.
    await search.fill("echo");
    await expect(page.getByTestId("home-row")).toHaveCount(0);
    await expect(page.getByTestId("home-empty")).toBeVisible();

    // A word that exists only in the exited row's error body matches nothing.
    await search.fill("MHOME_EXIT_SENTINEL");
    await expect(page.getByTestId("home-row")).toHaveCount(0);

    // Project and host names still match.
    await search.fill("remuda-e2e");
    await expect(page.getByTestId("home-row")).toHaveCount(2, { timeout: 10_000 });
  });

  test("clock/list ordering switches and is remembered per device", async ({ page }) => {
    await createSession(page, "MHOME_ORDER_A mhome-blocked approval gate");
    await createSession(page, "MHOME_ORDER_Z mhome-exit agent");
    await page.goto("/m");
    await expect(page.getByTestId("home-row")).toHaveCount(2);

    await page.getByTestId("home-order-list").click();
    await expect(page.getByTestId("home-order-list")).toHaveAttribute("aria-pressed", "true");
    expect(await page.evaluate(() => localStorage.getItem("remuda.mobile.home.order.v1"))).toBe("list");

    // Reload: the choice survives (per-device localStorage, not URL state).
    await page.reload();
    await expect(page.getByTestId("home-list")).toBeVisible();
    await expect(page.getByTestId("home-order-list")).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByTestId("home-order-clock")).toHaveAttribute("aria-pressed", "false");

    await page.getByTestId("home-order-clock").click();
    expect(await page.evaluate(() => localStorage.getItem("remuda.mobile.home.order.v1"))).toBe("clock");
  });

  test("exited rows resume through hubStore.resume and navigate to the new live instance", async ({
    page,
  }) => {
    const exitedId = await createSession(page, "MHOME_RESUME_TITLE mhome-exit agent");
    await page.goto("/m");
    const exitedRow = page.getByTestId("home-row").filter({ hasText: "MHOME_RESUME_TITLE" });
    await expect(exitedRow).toHaveAttribute("data-status", "exited");

    await exitedRow.getByTestId("home-resume").click();
    // D-026: resume creates a new instance inheriting the native session; the
    // row leaves for the new id and never stays on the exited transcript.
    await page.waitForURL(/\/s\//, { timeout: 20_000 });
    const newId = new URL(page.url()).pathname.split("/").pop()!;
    expect(newId).not.toBe(exitedId);
    created.push(newId);
    await expect(page.getByTestId("session-page")).toBeVisible();
  });
});
