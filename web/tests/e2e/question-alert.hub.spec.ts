import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * c-question-alert r2 (OWNER-REQUESTED): when an agent asks a question while
 * the owner is NOT viewing that session, the app raises a standing alert
 * (notify-stack, with the deadline countdown) and a `(n)` document.title
 * badge; both clear on answer/expiry; suppress for the session being viewed.
 *
 * r2 coverage:
 *  - compact: a FRESH /m document (PhoneShell — no Shell mount) starts the
 *    watcher; the question is created by ANOTHER client while the page stays
 *    on /m;
 *  - the standing alert carries the live countdown text;
 *  - /m/inbox compact question rows render their own countdown
 *    (m-inbox-deadline) and deep-link to the form;
 *  - answering/expiry dismisses the standing notification and badge even with
 *    no further poll (deadline clock);
 *  - desktop keeps the original flow.
 *
 * Driven by the in-process fake Node's `ask-question-deadline` sentinel
 * (a hook question with a fixed ~60-second deadline). page.clock advances
 * time (no wall-clock sleeps).
 */
test.describe.configure({ mode: "serial" });

const created: string[] = [];

test.describe("question alerts", () => {
  // Suite scope: under HUB_E2E_EXTERNAL there is no in-process fake Node with
  // the sentinel, and the skip must precede the login in beforeEach.
  test.skip(
    process.env.HUB_E2E_EXTERNAL === "1",
    "needs the in-process fake Node sentinel",
  );

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

  /** Create straight through the REST API, as ANOTHER client would while the
   * watched page stays put. */
  async function restCreateQuestion(page: Page): Promise<string> {
    const hosts = (await (await page.request.get("/v1/hosts")).json()) as {
      items: { hostId: string; label: string }[];
    };
    const host = hosts.items.find((item) => item.label === "e2e-fake-node");
    expect(host).toBeTruthy();
    const response = await page.request.post("/v1/instances", {
      headers: { Origin: new URL(page.url()).origin },
      data: {
        hostId: host!.hostId,
        kind: "claude",
        driver: "claude-sdk",
        permissionMode: "manual",
        prompt: "ask-question-deadline please",
      },
    });
    expect(response.ok(), await response.text()).toBe(true);
    const instanceId = ((await response.json()) as {
      instance: { instanceId: string };
    }).instance.instanceId;
    created.push(instanceId);
    return instanceId;
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
            (item) =>
              item.instanceId === iid && item.kind === "question" && item.state === "pending",
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

  async function expectStandingAlert(page: Page) {
    const toast = page
      .getByTestId("blocking-error")
      .filter({ hasText: "有问题需要你回答" })
      .first();
    await expect(toast).toBeVisible({ timeout: 15_000 });
    await expect(page.getByTestId("blocking-action-open")).toBeVisible();
    // r2-5: the alert carries the live deadline countdown.
    await expect(toast.getByText(/还剩 \d+ (分钟|秒)，超时将自动拒绝/)).toBeVisible();
    await expect(page).toHaveTitle(/^\(\d+\) Remuda$/);
    return toast;
  }

  test("compact 390: fresh /m entry watches; alert with countdown, /m/inbox row deadline, expiry clears", async ({
    page,
  }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await page.emulateMedia({ hasTouch: true });
    // Install the controllable clock before any app timers start; the fake
    // node stamps a ~60 s deadline at roughly this real instant.
    await page.clock.install();

    // A FRESH phone entry: /m renders PhoneShell (Shell never mounts on this
    // route tree), and the watcher must still be running via the shared
    // authenticated root. The page stays on /m — the question comes from
    // another client.
    await page.goto("/m");
    await page.clock.runFor(500);
    await expect(page.getByTestId("home-list")).toBeVisible({ timeout: 15_000 });

    const instanceId = await restCreateQuestion(page);
    await waitForQuestion(page, instanceId);
    await expectStandingAlert(page);

    // SPA-navigate to the compact inbox without reloading (a reload would
    // re-baseline the watcher and hide the alert).
    await page.evaluate(() => {
      window.history.pushState({}, "", "/m/inbox");
      window.dispatchEvent(new PopStateEvent("popstate"));
    });
    await expect(page).toHaveURL(/\/m\/inbox/);
    await page.clock.runFor(2500);

    // r2-6: the compact question row deep-links to the form AND carries its
    // own deadline countdown (the form component itself is not rendered here).
    const row = page.locator(`[data-interaction-id]`).filter({ hasText: "去回答" }).first();
    await expect(row).toBeVisible();
    const inboxDeadline = page.getByTestId("m-inbox-deadline").first();
    await expect(inboxDeadline).toBeVisible();
    await expect(inboxDeadline).toContainText(/还剩 \d+ (分钟|秒)，超时将自动拒绝/);

    // Open the session: viewing suppresses the alert; the form countdown is
    // present in the session view.
    await row.getByTestId("m-inbox-answer").click();
    await page.waitForURL(/\/s\//);
    const form = page.getByTestId("question-form").first();
    await expect(form).toBeVisible({ timeout: 15_000 });
    const deadlineEl = page.getByTestId("interaction-deadline").first();
    await expect(deadlineEl).toBeVisible();
    await expect(deadlineEl).toContainText(/还剩 \d+ (分钟|秒)，超时将自动拒绝/);

    // Fast-forward past the ~60 s deadline with no further Hub update: the
    // card expires, the standing alert is dismissed, and the title badge
    // clears (r2-2/r2-4).
    await page.clock.fastForward(90_000);
    await expect(deadlineEl).toHaveCount(0);
    await expect(
      page.getByTestId("blocking-error").filter({ hasText: "有问题需要你回答" }),
    ).toHaveCount(0);
    await expect(page).toHaveTitle("Remuda");
  });

  test("desktop 1440: toast + title badge, countdown ticks, viewing suppresses, expiry clears", async ({
    page,
  }) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    await page.clock.install();

    // The watcher baselines on first mount (no questions yet), so a question
    // raised AFTER this is a genuine post-hydration arrival that alerts.
    await page.goto("/sessions");
    await page.clock.runFor(500);

    const instanceId = await pickHostAndCreate(page, "ask-question-deadline please");

    // Leave the session via SPA navigation (a full page.goto reloads the app
    // and re-baselines the watcher, hiding an already-present question).
    await page.evaluate(() => {
      window.history.pushState({}, "", "/sessions");
      window.dispatchEvent(new PopStateEvent("popstate"));
    });
    await expect(page).toHaveURL(/\/sessions/);
    await waitForQuestion(page, instanceId);
    await expectStandingAlert(page);

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

    // Fast-forward past the deadline (no wall sleep): the countdown
    // disappears as the card expires and the standing alert/badge clear.
    await page.clock.fastForward(90_000);
    await expect(deadlineEl).toHaveCount(0);
    await expect(
      page.getByTestId("blocking-error").filter({ hasText: "有问题需要你回答" }),
    ).toHaveCount(0);
    await expect(page).toHaveTitle("Remuda");
  });
});
