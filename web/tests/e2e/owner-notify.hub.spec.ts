import { devices, expect, test, type Browser, type BrowserContext, type Locator, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * cb-owner-notify (D-060 §11.5, home.md §4.1 kind 4, §4.2, §8, §9 落点).
 *
 * Gated here, against notices this spec writes through the real route:
 *  - ④ 「告诉你」: an unacknowledged notice is a card after 任务待你 and before
 *    回合失败, with its title, body, 知道了 (and 打开对话 when a chapter wrote
 *    it); it is NOT counted — the header 「需要你 · N」 stays the same N as
 *    ① before and after it arrives;
 *  - a repeat with the same dedupe key returns the original notice;
 *  - §8: offline greys 知道了 with 数据可能过期, and it comes back live;
 *  - 知道了 acknowledges on the Hub: the card leaves and the Hub no longer
 *    lists it unacknowledged;
 *  - push landing: /sessions?focus=<noticeId> rings the card on /sessions
 *    (1280, the 2-column grid) and on /m (390).
 *
 * With HUB_E2E_MAIN_UI=1 (it seats the Hub's one address-owner Main, like
 * main-ui.hub.spec.ts) it also drives Main's own notice: the card speaks as
 * Main in ④, 打开对话 opens Main's transcript where the same notice is a
 * labelled card under Main's reply (never a user bubble), and 知道了 there
 * marks it 已确认 and removes it from ④.
 *
 * Every notice this spec writes is acknowledged at the end. No millisecond
 * thresholds; evidence screenshots only behind REMUDA_EVIDENCE=1.
 */

test.describe.configure({ mode: "serial" });

const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = path.join(here, "../../test-results/evidence");

async function shot(page: Page, name: string) {
  if (!evidence) return;
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

type Notice = { noticeId: string; title: string; ackedAt: string | null; chapterInstanceId: string | null };

const written: string[] = [];
const seated: string[] = [];

async function notify(
  page: Page,
  body: { title: string; body: string; dedupeKey?: string },
  instanceId?: string,
): Promise<{ status: number; notice: Notice; created: boolean }> {
  const origin = new URL(page.url()).origin;
  const response = await page.request.post("/v1/owner/notify", {
    headers: { Origin: origin, ...(instanceId ? { "x-remuda-instance-id": instanceId } : {}) },
    data: body,
  });
  const text = await response.text();
  expect(response.ok(), `notify → ${response.status()}: ${text}`).toBe(true);
  const parsed = JSON.parse(text) as { notice: Notice; created: boolean };
  if (!written.includes(parsed.notice.noticeId)) written.push(parsed.notice.noticeId);
  return { status: response.status(), ...parsed };
}

async function unacked(page: Page): Promise<string[]> {
  const response = await page.request.get("/v1/owner/notices?unacked=1&limit=200");
  expect(response.ok()).toBe(true);
  const body = (await response.json()) as { items: Notice[] };
  return body.items.map((row) => row.noticeId);
}

async function ackAll(page: Page) {
  const origin = new URL(page.url()).origin;
  for (const id of written.splice(0)) {
    await page.request
      .post(`/v1/owner/notices/${id}/ack`, { headers: { Origin: origin } })
      .catch(() => undefined);
  }
}

function card(page: Page, id: string): Locator {
  return page.locator(`[data-testid="home-queue-card"][data-card-id="${id}"]`);
}

async function showCard(page: Page, id: string) {
  const more = page.locator('[data-testid="home-queue-more"][aria-expanded="false"]');
  await expect(card(page, id).or(more).first()).toBeVisible({ timeout: 20_000 });
  if (await more.count()) await more.click();
  await expect(card(page, id)).toBeVisible({ timeout: 20_000 });
}

/** The ④ header N, and ① 「⚠N 需要你」 (hidden when it is 0). */
async function counts(page: Page): Promise<{ header: string | undefined; glance: string }> {
  const header = (await page.getByTestId("home-queue-title").innerText()).match(/需要你 · (\S+)/)?.[1];
  const segment = page.getByTestId("glance-need-you");
  const glance = (await segment.count()) ? (await segment.innerText()).match(/(\d+) 需要你/)?.[1] ?? "?" : "0";
  return { header, glance };
}

async function phoneContext(browser: Browser, baseURL: string, name: string): Promise<BrowserContext> {
  const context = await browser.newContext({ ...devices["iPhone 13"], baseURL });
  await login(await context.newPage(), name);
  return context;
}

test.describe("owner notify: 告诉你", () => {
  test.describe.configure({ timeout: 180_000 });

  test("390: a notice is an uncounted 告诉你 card; stale greys 知道了; 知道了 removes it", async ({ browser }) => {
    const baseURL = test.info().project.use.baseURL!;
    const context = await phoneContext(browser, baseURL, "e2e-notify-phone");
    const page = await context.newPage();
    try {
      await page.goto("/m");
      await expect(page.getByTestId("home-queue")).toBeVisible({ timeout: 20_000 });
      await expect.poll(async () => (await counts(page)).header, { timeout: 20_000 }).not.toBe("–");
      const before = await counts(page);

      const suffix = Date.now().toString(36);
      const first = await notify(page, {
        title: `夜间 gate 红了 ${suffix}`,
        body: "第 2 条车道失败：wire_golden。\n我已暂停合入，等你看日志。",
        dedupeKey: `e2e:notify:${suffix}`,
      });
      expect(first.status).toBe(201);
      expect(first.created).toBe(true);
      // The same dedupe key: the original notice, nothing new.
      const again = await notify(page, {
        title: "another title",
        body: "another body",
        dedupeKey: `e2e:notify:${suffix}`,
      });
      expect(again.status).toBe(200);
      expect(again.created).toBe(false);
      expect(again.notice.noticeId).toBe(first.notice.noticeId);

      const id = first.notice.noticeId;
      await showCard(page, id);
      const notice = card(page, id);
      await expect(notice).toHaveAttribute("data-card-type", "notice");
      await expect(notice).toContainText("告诉你");
      await expect(notice.getByTestId("home-card-title")).toHaveText(`夜间 gate 红了 ${suffix}`);
      await expect(notice.getByTestId("home-card-body")).toContainText("第 2 条车道失败");
      // The owner's own note has no conversation to open.
      await expect(notice.getByTestId("home-card-open")).toHaveCount(0);

      // §4.1: kind 4 sits after every 任务待你 and before every 回合失败.
      const types = await page
        .getByTestId("home-queue-card")
        .evaluateAll((nodes) => nodes.map((node) => node.getAttribute("data-card-type")));
      const at = types.indexOf("notice");
      expect(types.slice(at).includes("task")).toBe(false);
      expect(types.slice(0, at).includes("failed")).toBe(false);

      // §5.1: not counted — N is what it was, and ① and ④ agree.
      await expect.poll(async () => counts(page)).toEqual(before);
      expect(before.header).toBe(before.glance);
      await shot(page, "owner-notify-390.png");

      // §8: offline greys 知道了 with 数据可能过期.
      const ack = notice.getByTestId("home-card-ack");
      await expect(ack).toBeEnabled();
      await context.setOffline(true);
      await expect(ack).toBeDisabled({ timeout: 30_000 });
      await expect(notice.getByTestId("home-card-stale")).toHaveText("数据可能过期");
      await context.setOffline(false);
      await expect(ack).toBeEnabled({ timeout: 30_000 });

      // 知道了: acknowledged on the Hub; the card leaves.
      await ack.click();
      await expect(card(page, id)).toHaveCount(0, { timeout: 20_000 });
      expect(await unacked(page)).not.toContain(id);
    } finally {
      await context.setOffline(false);
      await ackAll(page);
      await context.close();
    }
  });

  test("push landing: /sessions?focus=<noticeId> rings the card on /sessions and on /m", async ({ browser }) => {
    const baseURL = test.info().project.use.baseURL!;
    const desktop = await browser.newContext({ baseURL, viewport: { width: 1280, height: 900 } });
    const desk = await desktop.newPage();
    const phone = await phoneContext(browser, baseURL, "e2e-notify-land-phone");
    const page = await phone.newPage();
    try {
      await login(desk, "e2e-notify-land-desktop");
      const { notice } = await notify(desk, { title: "证书今天到期", body: "记得续签 hub 的 TLS 证书。" });
      // The push payload's landing (owner_notify::landing_url).
      await desk.goto(`/sessions?focus=${notice.noticeId}`);
      await expect(desk.getByTestId("home-queue")).toHaveAttribute("data-placement", "desktop", { timeout: 20_000 });
      const deskCard = card(desk, notice.noticeId);
      await expect(deskCard).toHaveAttribute("data-focus", "true", { timeout: 20_000 });
      await shot(desk, "owner-notify-1280-focus.png");

      await page.goto(`/sessions?focus=${notice.noticeId}`);
      await expect(page).toHaveURL(`/m?focus=${notice.noticeId}`);
      await expect(card(page, notice.noticeId)).toHaveAttribute("data-focus", "true", { timeout: 20_000 });
    } finally {
      await ackAll(desk);
      await desktop.close();
      await phone.close();
    }
  });
});

test.describe("owner notify: Main's voice and the Owner thread", () => {
  test.skip(
    process.env.HUB_E2E_MAIN_UI !== "1",
    "set HUB_E2E_MAIN_UI=1: seats the Hub's one address-owner Main on the fake node",
  );
  test.describe.configure({ timeout: 180_000 });

  async function fakeHost(page: Page): Promise<string> {
    const response = await page.request.get("/v1/hosts");
    expect(response.ok()).toBe(true);
    const body = (await response.json()) as { items?: { id?: string; label?: string }[] };
    const host = (body.items ?? []).find((row) => row.label === "e2e-fake-node");
    if (!host?.id) test.skip(true, "needs the in-process fake Node");
    return host!.id!;
  }

  async function seatMain(page: Page): Promise<string> {
    const hostId = await fakeHost(page);
    const response = await page.request.post("/v1/instances", {
      headers: { Origin: new URL(page.url()).origin },
      data: {
        hostId,
        workspaceId: "wsp_e2e",
        kind: "claude",
        driver: "claude-print",
        prompt: "owner-notify seat",
        title: "Main e2e",
        role: "top-coordinator",
        grants: ["address-owner"],
      },
    });
    expect(response.ok(), `seat Main: ${response.status()} ${await response.text()}`).toBe(true);
    const body = (await response.json()) as { instance: { instanceId: string } };
    seated.push(body.instance.instanceId);
    return body.instance.instanceId;
  }

  /** Close (pause) and delete the seat, as main-ui.hub.spec.ts does. */
  async function releaseMain(page: Page) {
    const origin = new URL(page.url()).origin;
    for (const id of seated.splice(0).reverse()) {
      await page.request
        .post(`/v1/instances/${id}/commands`, {
          headers: { Origin: origin },
          data: { operation: "instance.close", payload: {} },
        })
        .catch(() => undefined);
      const pausedOrGone = async () => {
        const record = await page.request.get(`/v1/instances/${id}`);
        if (record.status() === 404) return true;
        const body = (await record.json()) as { lineageId?: string | null };
        if (!body.lineageId) return true;
        const lineage = await page.request.get(`/v1/lineages/${body.lineageId}`);
        return !lineage.ok() || ((await lineage.json()) as { state?: string }).state === "paused";
      };
      await expect.poll(pausedOrGone, { timeout: 20_000 }).toBe(true);
      for (let attempt = 0; attempt < 5; attempt += 1) {
        const removed = await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
        if (!removed || removed.ok() || removed.status() === 404) break;
        await page.waitForTimeout(500);
      }
    }
  }

  test("390: Main's notice speaks as Main in ④ and in its transcript; 知道了 there clears ④", async ({ browser }) => {
    const baseURL = test.info().project.use.baseURL!;
    const context = await phoneContext(browser, baseURL, "e2e-notify-main");
    const page = await context.newPage();
    try {
      await page.goto("/m");
      const mainId = await seatMain(page);
      // Main has replied once (the fake Node journals `echo: <prompt>`).
      await page.goto(`/s/${mainId}`);
      await expect(page.locator('[data-testid="transcript-row"][data-role="assistant"]').first()).toBeVisible({
        timeout: 20_000,
      });
      const { notice } = await notify(
        page,
        { title: "三个 worker 都绿了", body: "SE-41、SE-42、SE-43 都过了 gate，等你说合不合。" },
        mainId,
      );
      expect(notice.chapterInstanceId).toBe(mainId);

      await page.goto("/m");
      await showCard(page, notice.noticeId);
      const homeCard = card(page, notice.noticeId);
      await expect(homeCard).toContainText("告诉你 · Main");
      await shot(page, "owner-notify-390-main.png");
      await homeCard.getByTestId("home-card-open").click();
      await expect(page).toHaveURL(new RegExp(`/s/${mainId}`));

      // The Owner thread: Main's labelled card under Main's reply.
      const inThread = page.locator(`[data-testid="transcript-notice"][data-notice-id="${notice.noticeId}"]`);
      await expect(inThread).toBeVisible({ timeout: 20_000 });
      await expect(inThread).toContainText("Main 告诉你");
      await expect(inThread).toContainText("SE-41、SE-42、SE-43");
      const row = inThread.locator('xpath=ancestor::*[@data-testid="transcript-row"][1]');
      await expect(row).toHaveAttribute("data-role", "assistant");
      await shot(page, "owner-notify-390-thread.png");

      await inThread.getByTestId("transcript-notice-ack").click();
      await expect(inThread.getByTestId("transcript-notice-acked")).toHaveText("已确认", { timeout: 20_000 });
      expect(await unacked(page)).not.toContain(notice.noticeId);
      await page.goto("/m");
      await expect(page.getByTestId("home-queue")).toBeVisible({ timeout: 20_000 });
      await expect(card(page, notice.noticeId)).toHaveCount(0, { timeout: 20_000 });
    } finally {
      await ackAll(page);
      await releaseMain(page);
      await context.close();
    }
  });
});
