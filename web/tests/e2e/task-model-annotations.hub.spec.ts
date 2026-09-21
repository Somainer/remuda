import { expect, test, type Locator, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * Plan task-model task 9 (t-annotations), D-050 §7 / design §8 / ui-spec
 * §1.5,§2.9: structured annotations are device-local composer drafts riding
 * the next send — zero wire field, zero table.
 *
 * Coverage:
 *  - the composer dock offers ＋ 加批注; a card draft and a ① text anchor
 *    (selected in the transcript) raise the 「本次发送带 N 条批注」badge;
 *  - on send the drafts are serialised as a structured prompt PREFIX — the
 *    in-process fake harness echoes the prompt verbatim (`echo: …`), so the
 *    assertion proves the prefixed text reached the model with no new wire
 *    field;
 *  - drafts clear after the send (badge disappears, localStorage key gone).
 *
 * No gated fake-node trigger is required: this is a normal launch on the
 * default e2e workspace, so the spec runs in a default full-suite gate run.
 * Evidence PNGs are written only under REMUDA_EVIDENCE=1.
 */

test.describe.configure({ mode: "serial" });
test.skip(process.env.HUB_E2E_EXTERNAL === "1", "Needs the in-process fake Node");

const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence
  ? path.join(here, "../../../docs/design/evidence")
  : path.join(here, "../../test-results/evidence");

async function shot(page: Page, name: string) {
  if (!evidence) return;
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

async function patchMaxInstances(page: Page, value: number): Promise<void> {
  await page.evaluate(async (next) => {
    const list = await fetch("/v1/hosts", { credentials: "include" });
    const body = (await list.json()) as {
      items?: { hostId?: string; maxInstances?: number }[];
    };
    const id = body.items?.find((host) => host.hostId)?.hostId;
    if (!id) return;
    await fetch(`/v1/hosts/${id}`, {
      method: "PATCH",
      credentials: "include",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ maxInstances: next }),
    });
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
  const page = await browser.newPage();
  await login(page);
  await patchMaxInstances(page, 24);
  await page.close();
});

test.afterAll(async ({ browser }) => {
  const page = await browser.newPage();
  await login(page);
  await patchMaxInstances(page, 8);
  await forceDeleteAllInstances(page).catch(() => undefined);
  await page.close();
});

test.afterEach(async ({ page }) => {
  await forceDeleteAllInstances(page).catch(() => undefined);
});

/**
 * Select a message's PROSE (the section's second element child; the first is
 * the You/assistant role header), so the captured ① quote never carries the
 * role label.
 */
async function selectProse(section: Locator) {
  await section.evaluate((el) => {
    const prose = el.children[1] ?? el;
    const range = document.createRange();
    range.selectNodeContents(prose);
    const selection = window.getSelection();
    selection?.removeAllRanges();
    selection?.addRange(range);
  });
}

/** Launch on the fake node and answer the create-approval trap. */
async function createSession(page: Page, prompt: string): Promise<string> {
  await forceDeleteAllInstances(page);
  await page.getByTitle("新建", { exact: true }).click();
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
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0);
  await page.getByTestId("new-session-prompt").fill(prompt);
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
  const instanceId = new URL(page.url()).pathname.split("/").pop() as string;
  await expect(page.getByTestId("session-page")).toBeVisible();

  // The fake harness can raise a second approval right after the create one;
  // keep answering until none are pending server-side AND the card unmounts
  // (mirrors m-chrome.clearApprovals).
  const deadline = Date.now() + 20_000;
  for (;;) {
    const pendingCount = await page.evaluate(async (id) => {
      const list = await fetch("/v1/interactions", { credentials: "include" });
      const body = (await list.json()) as {
        items?: {
          id: string;
          instanceId?: string;
          state?: string;
          interactionId?: string;
          request?: { kind?: string; inputDigest?: string; options?: { id: string }[] };
        }[];
      };
      const mine = (body.items ?? []).filter(
        (item) => item.instanceId === id && item.state === "pending",
      );
      for (const item of mine) {
        const optionId = item.request?.options?.[0]?.id;
        if (!optionId) continue;
        await fetch(`/v1/interactions/${item.interactionId ?? item.id}/answer`, {
          method: "POST",
          credentials: "include",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({
            answer: {
              kind: "approval",
              optionId,
              inputDigest: item.request?.inputDigest ?? "",
            },
          }),
        });
      }
      return mine.length;
    }, instanceId);
    const cardCount = await page.getByTestId("approval-card").count();
    if (pendingCount === 0 && cardCount === 0) break;
    expect(Date.now() < deadline, "approvals clear within 20s").toBe(true);
    await page.waitForTimeout(300);
  }
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });
  return instanceId;
}

test.describe("composer annotation drafts", () => {
  test.use({ viewport: { width: 1440, height: 900 } });

  test("badge counts card + anchor drafts and the send carries the structured prefix then clears", async ({
    page,
  }) => {
    await login(page);
    const suffix = Date.now().toString(36);
    const instanceId = await createSession(page, `ann echo seed ${suffix}`);

    // The fake harness echoes the seed prompt in an assistant message; wait
    // for the transcript prose that will be the ① anchor surface.
    const echo = page
      .getByTestId("message")
      .filter({ hasText: `echo: ann echo seed ${suffix}` })
      .first();
    await expect(echo).toBeVisible({ timeout: 20_000 });

    // No drafts yet: no badge, but the additive entry point exists.
    expect(page.getByTestId("annotation-badge")).toHaveCount(0);
    await expect(page.getByTestId("annotation-add")).toBeVisible();

    // Carrier 1 — a card-level annotation.
    await page.getByTestId("annotation-add").click();
    await expect(page.getByTestId("annotation-panel")).toBeVisible();
    await page
      .getByTestId("annotation-card-input")
      .fill(`card note ${suffix}: rerun the seed`);
    await page.getByTestId("annotation-card-save").click();
    await expect(page.getByTestId("annotation-badge-count")).toHaveText("1");
    await expect(page.getByTestId("annotation-badge")).toContainText("本次发送带 1 条批注");

    // Carrier 2 — an in-message ① anchor created by selecting transcript text.
    await selectProse(echo);
    const capture = page.getByTestId("annotation-capture");
    await expect(capture).toBeVisible({ timeout: 5_000 });
    await capture.click();
    const popover = page.getByTestId("annotation-capture-popover");
    await expect(popover).toBeVisible();
    await expect(popover.getByTestId("annotation-capture-quote")).toContainText("ann echo seed");
    await page
      .getByTestId("annotation-capture-input")
      .fill(`anchor note ${suffix}: this line is wrong`);
    await page.getByTestId("annotation-capture-save").click();
    await expect(page.getByTestId("annotation-badge-count")).toHaveText("2");

    // The 标记 tab lists the anchor with its ① mark and the quote.
    await page.getByTestId("annotation-tab-anchor").click();
    await expect(page.getByTestId("annotation-item").filter({ hasText: "①" })).toBeVisible();
    await expect(page.locator('[data-testid="annotation-item"][data-carrier="anchor"]')).toContainText(
      "ann echo seed",
    );

    // Send a prompt — the structured prefix rides it, no new wire field.
    const question = `act on the notes please ${suffix}`;
    await page.getByTestId("composer-input").fill(question);
    await page.getByTestId("composer-send").click();

    // The fake harness echoes the prompt verbatim; the echo proves the
    // structured prefix reached the model ahead of the prompt text.
    const sent = page
      .getByTestId("message")
      .filter({ hasText: "【批注 ×2】" })
      .first();
    await expect(sent).toBeVisible({ timeout: 20_000 });
    await expect(sent).toContainText(`card note ${suffix}: rerun the seed`);
    await expect(sent).toContainText(`anchor note ${suffix}: this line is wrong`);
    await expect(sent).toContainText("文本标记 · 会话记录");
    await expect(sent).toContainText(question);
    // Feedback wording only — no board protocol is injected for the agent.
    const text = (await sent.textContent()) ?? "";
    expect(text).not.toMatch(/set_task_state|boardColumn|board_column/);

    // Drafts clear after the send: badge gone, device-local key removed.
    await expect(page.getByTestId("annotation-badge")).toHaveCount(0);
    await expect
      .poll(() =>
        page.evaluate((id) => localStorage.getItem(`runtime.annotation.${id}`), instanceId),
      )
      .toBeNull();

    // A second send with no drafts is byte-free of any prefix block.
    await page.getByTestId("composer-input").fill(`plain follow up ${suffix}`);
    await page.getByTestId("composer-send").click();
    const plain = page
      .getByTestId("message")
      .filter({ hasText: `plain follow up ${suffix}` })
      .last();
    await expect(plain).toBeVisible({ timeout: 20_000 });
    await expect(plain).not.toContainText("【批注");
  });

  test("raw-events segment offers no annotation surface", async ({ page }) => {
    await login(page);
    const suffix = Date.now().toString(36);
    const instanceId = await createSession(page, `ann events check ${suffix}`);
    // The structured (transcript) segment has the dock…
    await expect(page.getByTestId("annotation-dock")).toBeVisible();
    // …a terminal-style segment (raw events) carries no annotation context:
    // the page-level attribute that lets selection raise an anchor is absent.
    await page.goto(`/s/${instanceId}/events`);
    await expect(page.getByTestId("session-page")).toHaveAttribute("data-view", "events");
    await expect(page.locator("[data-annotation-instance]")).toHaveCount(0);
  });

  test("evidence shots: badge + panel at 1440 and 390", async ({ page }) => {
    test.skip(!evidence, "set REMUDA_EVIDENCE=1 to capture committed screenshots");
    await login(page);
    const suffix = Date.now().toString(36);
    await createSession(page, `ann evidence seed ${suffix}`);
    const echo = page
      .getByTestId("message")
      .filter({ hasText: `echo: ann evidence seed ${suffix}` })
      .first();
    await expect(echo).toBeVisible({ timeout: 20_000 });

    // One card draft plus one ① transcript anchor, panel open at 1440.
    await page.getByTestId("annotation-add").click();
    await page.getByTestId("annotation-card-input").fill("这条实现需要先补回归测试再合入。");
    await page.getByTestId("annotation-card-save").click();
    await selectProse(echo);
    await page.getByTestId("annotation-capture").click();
    await expect(page.getByTestId("annotation-capture-popover")).toBeVisible();
    await page.getByTestId("annotation-capture-input").fill("这行结论和上面的回归断言对不上。");
    await page.getByTestId("annotation-capture-save").click();
    await expect(page.getByTestId("annotation-badge-count")).toHaveText("2");
    // Dismiss the capture popover; the save already opened the panel.
    await page.getByTestId("annotation-capture-done").click();
    await expect(page.getByTestId("annotation-capture-popover")).toHaveCount(0);
    await page.getByTestId("annotation-tab-anchor").click();
    await expect(page.getByTestId("annotation-panel")).toBeVisible();
    await shot(page, "task-model-9-composer-1440.png");

    // 390 phone: the same device-local drafts ride along; reopen the panel
    // from the badge (it was never toggled) — close first, then open.
    await page.setViewportSize({ width: 390, height: 844 });
    await expect(page.getByTestId("annotation-badge-count")).toHaveText("2");
    await shot(page, "task-model-9-composer-390.png");
  });
});
