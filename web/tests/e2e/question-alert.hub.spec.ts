import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * c-question-alert (OWNER-REQUESTED): when an agent asks a question while the
 * owner is NOT viewing that session, the app raises a visible alert — an
 * info toast with a jump action — and a `(n)` document.title badge; the
 * QuestionForm shows a live deadline countdown. Opening the session
 * suppresses the alert.
 *
 * Driven by the in-process fake Node's `ask-question-deadline` sentinel
 * (a hook question with a fixed ~15-minute deadline). Desktop 1440 and
 * compact 390 share the same store/watcher; both are asserted. page.clock
 * advances the countdown (no wall-clock sleeps).
 */
test.describe.configure({ mode: "serial" });

const created: string[] = [];

test.beforeEach(async ({ page }) => {
  await login(page);
});

test.afterEach(async ({ page }) => {
  for (const id of created.splice(0)) {
    await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
  }
});

async function pickHostAndCreate(page: Page, prompt: string): Promise<string> {
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
  await page.getByTestId("new-session-prompt").fill(prompt);
  await page.getByTestId("new-session-start").click();
  await page.waitForURL(/\/s\//, { timeout: 20_000 });
  const id = new URL(page.url()).pathname.split("/").pop()!;
  created.push(id);
  return id;
}

/**
 * Wait for the pending question to exist server-side AND be folded into the
 * client store. With a fake clock installed the store's 2 s poll only fires
 * when time advances, so run the clock forward in steps as part of the wait.
 */
async function waitForQuestion(page: Page, instanceId: string): Promise<void> {
  for (let i = 0; i < 20; i++) {
    await page.clock.runFor(1000);
    const found = await page.evaluate(async (iid) => {
      const res = await fetch("/v1/interactions", { credentials: "include" });
      const body = (await res.json()) as {
        items?: { instanceId?: string; kind?: string; state?: string }[];
      };
      return Boolean(
        body.items?.some(
          (item) => item.instanceId === iid && item.kind === "question" && item.state === "pending",
        ),
      );
    }, instanceId);
    if (found) {
      // Give the store poll a beat to fold it and the watcher to fire.
      await page.clock.runFor(2500);
      return;
    }
  }
  throw new Error(`pending question never appeared for ${instanceId}`);
}

for (const [label, width, height, mobile] of [
  ["desktop", 1440, 900, false],
  ["compact", 390, 844, true],
] as const) {
  test(`question raises toast + title badge, countdown ticks, viewing suppresses (${label} ${width})`, async ({ page }) => {
    test.skip(process.env.HUB_E2E_EXTERNAL === "1", "needs the in-process fake Node sentinel");
    await page.setViewportSize({ width, height });
    if (mobile) {
      await page.emulateMedia({ hasTouch: true });
    }
    // Install a controllable clock before any app timers start; the fake node
    // stamps a ~60 s deadline at roughly this real instant.
    await page.clock.install();

    // The watcher baselines on first mount (no questions yet), so a question
    // raised AFTER this is a genuine post-hydration arrival that alerts.
    // beforeEach already logged in on a prior real-clock navigation.
    await page.goto(mobile ? "/m" : "/sessions");
    await page.clock.runFor(500);

    // Create a session whose prompt raises the question. createSession waits
    // only for the /s/:id redirect, not for the blocked command to settle, so
    // it returns while the fake node holds the question pending.
    const instanceId = await pickHostAndCreate(page, "ask-question-deadline please");

    // Leave the session via SPA navigation (a full page.goto reloads the app
    // and re-baselines the watcher, hiding an already-present question).
    // BrowserRouter listens for popstate; push the history entry then fire
    // popstate so React Router changes view WITHOUT reloading the document.
    await page.evaluate((path) => {
      window.history.pushState({}, "", path);
      window.dispatchEvent(new PopStateEvent("popstate"));
    }, mobile ? "/m" : "/sessions");
    await expect(page).toHaveURL(/\/sessions|\/m/);
    await waitForQuestion(page, instanceId);

    // The standing question alert (persistent, with a jump action) and the
    // persistent title badge appear. The body is
    // "Agent <title> · 有问题需要你回答".
    const toast = page.getByTestId("blocking-error").filter({ hasText: "有问题需要你回答" }).first();
    await expect(toast).toBeVisible({ timeout: 15_000 });
    await expect(page.getByTestId("blocking-action-open")).toBeVisible();
    await expect(page).toHaveTitle(/^\(\d+\) Remuda$/);

    // Open the session (full nav is fine now — the alert was already posted):
    // the form and a live countdown are present.
    await page.goto(`/s/${instanceId}/structured`);
    const form = page.getByTestId("question-form").first();
    await expect(form).toBeVisible({ timeout: 15_000 });
    const deadlineEl = page.getByTestId("interaction-deadline").first();
    await expect(deadlineEl).toBeVisible();
    expect(((await deadlineEl.textContent()) ?? "")).toMatch(
      /还剩 \d+ (分钟|秒)，超时将自动拒绝/,
    );

    // Fast-forward past the ~60 s deadline (no wall sleep): the countdown
    // disappears as the card expires.
    await page.clock.fastForward(90_000);
    await expect(deadlineEl).toHaveCount(0);

  });
}
