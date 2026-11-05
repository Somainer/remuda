import { devices, expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * cb-seat-profile (D-063, main-agent.md §3.4.2): Main's seat profile panel
 * against the in-process fake node.
 *
 * Fake-node behaviour used:
 *  - a launch whose spec names `skills` journals a `launch.capabilities`
 *    event listing each as unavailable `missing` (the fake host has none);
 *  - the `mhome-exit` create sentinel reports a native session, then ends
 *    the process, so 恢复 continues the lineage in chapter 2.
 *
 * It seats the Hub's one address-owner Main, so it runs only when asked:
 * HUB_E2E_MAIN_UI=1 (the gate main-ui.hub.spec.ts uses). Evidence
 * screenshots stay behind REMUDA_EVIDENCE and land in scratch/.
 */
test.describe.configure({ mode: "serial" });

test.skip(
  process.env.HUB_E2E_MAIN_UI !== "1",
  "set HUB_E2E_MAIN_UI=1: seats the Hub's one address-owner Main on the fake node",
);

const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = path.join(here, "../../../scratch/evidence/seat-profile");

const created: string[] = [];

async function shot(page: Page, name: string) {
  if (!evidence) return;
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

async function fakeHost(page: Page): Promise<string> {
  const response = await page.request.get("/v1/hosts");
  expect(response.ok()).toBe(true);
  const body = (await response.json()) as { items?: { id?: string; label?: string }[] };
  const host = (body.items ?? []).find((row) => row.label === "e2e-fake-node");
  expect(host?.id, "fake node host").toBeTruthy();
  return host!.id!;
}

async function seatMain(page: Page, prompt: string): Promise<string> {
  const hostId = await fakeHost(page);
  const response = await page.request.post("/v1/instances", {
    data: {
      hostId,
      workspaceId: "wsp_e2e",
      kind: "claude",
      driver: "claude-print",
      prompt,
      title: "Main e2e",
      role: "top-coordinator",
      grants: ["address-owner"],
      skills: ["/srv/skills/review-kit"],
    },
  });
  expect(response.ok(), `seat Main: ${response.status()} ${await response.text()}`).toBe(true);
  const body = (await response.json()) as { instance: { instanceId: string } };
  created.push(body.instance.instanceId);
  return body.instance.instanceId;
}

async function putProfile(page: Page, lineage: string, body: Record<string, unknown>) {
  const origin = new URL(page.url()).origin;
  return page.request.put(`/v1/lineages/${lineage}/profile`, { headers: { Origin: origin }, data: body });
}

/** Pause (close) and delete every seated chapter, newest first. */
async function cleanup(page: Page) {
  const origin = new URL(page.url()).origin;
  for (const id of created.splice(0).reverse()) {
    await page.request
      .post(`/v1/instances/${id}/commands`, {
        headers: { Origin: origin },
        data: { operation: "instance.close", payload: {} },
      })
      .catch(() => undefined);
    const pausedOrGone = async () => {
      const record = await page.request.get(`/v1/instances/${id}`);
      if (record.status() === 404) return true;
      const body = (await record.json()) as { lineageId?: string | null; lifecycle?: string };
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

test.describe("desktop seat profile", () => {
  test.afterEach(async ({ page }) => {
    await cleanup(page);
  });

  test("1440: edit with the revision CAS, 「待下一章节生效」, a 409 reload, and the next chapter clears it", async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    await login(page);
    const firstId = await seatMain(page, "seat-profile mhome-exit chapter one");
    await page.goto(`/s/${firstId}`);
    const panel = page.getByTestId("seat-profile");
    await expect(panel).toBeVisible({ timeout: 20_000 });
    await expect(panel).toContainText("未设置 · 沿用创建时配置");
    // The last launch snapshot's unavailable item.
    await expect(panel.getByTestId("seat-profile-unavailable-count")).toHaveText("上次启动 1 项不可用", {
      timeout: 20_000,
    });
    await expect(panel.getByTestId("seat-profile-unavailable")).toContainText("技能 review-kit：不存在");
    await expect(panel.getByTestId("seat-profile-pending")).toHaveCount(0);

    // The owner's first edit: effort and two model candidates.
    await panel.getByTestId("seat-profile-edit").click();
    await panel.getByTestId("seat-profile-effort").selectOption("high");
    await panel.getByTestId("seat-profile-models-input").fill("e2e/model-a\ne2e/model-b");
    await panel.getByTestId("seat-profile-save").click();
    await expect(panel.getByTestId("seat-profile-revision")).toHaveText("修订 1");
    await expect(panel.getByTestId("seat-profile-pending")).toHaveText("待下一章节生效");
    await shot(page, "seat-profile-1440-pending.png");

    // Another device saves first: this one's save gets the 409 and the
    // reload text, and reloading re-reads the current revision.
    await panel.getByTestId("seat-profile-edit").click();
    await panel.getByTestId("seat-profile-models-input").fill("e2e/model-b");
    const other = await putProfile(page, firstId, { expectedRevision: 1, role: "elsewhere" });
    expect(other.ok(), await other.text()).toBe(true);
    await panel.getByTestId("seat-profile-save").click();
    await expect(panel.getByTestId("seat-profile-conflict")).toContainText("当前修订 2");
    await expect(panel.getByTestId("seat-profile-conflict")).toContainText("请重新载入后再保存");
    await shot(page, "seat-profile-1440-conflict.png");
    await panel.getByTestId("seat-profile-reload").click();
    await expect(panel.getByTestId("seat-profile-conflict")).toHaveCount(0);
    await expect(panel.getByTestId("seat-profile-revision")).toHaveText("修订 2");

    // The next chapter applies revision 2: the badge clears there.
    const bar = page.getByTestId("main-session-bar");
    await expect(bar.getByTestId("main-resume")).toBeVisible({ timeout: 20_000 });
    await bar.getByTestId("main-resume").click();
    await expect(bar.getByTestId("main-status-chapter")).toHaveText("第 2 章", { timeout: 30_000 });
    // Chapter 2 from the lineage record (the bar's own navigation is
    // main-ui.hub.spec.ts's subject), then its page.
    const chapter2 = async () => {
      const record = await page.request.get(`/v1/lineages/${firstId}`);
      const body = (await record.json()) as { chapters?: { instanceId: string; generation: number }[] };
      return body.chapters?.find((chapter) => chapter.generation === 2)?.instanceId ?? "";
    };
    await expect.poll(chapter2, { timeout: 30_000 }).not.toBe("");
    const secondId = await chapter2();
    expect(secondId).not.toBe(firstId);
    created.push(secondId);
    await page.goto(`/s/${secondId}`);
    await expect(page.getByTestId("seat-profile").getByTestId("seat-profile-revision")).toHaveText("修订 2", {
      timeout: 20_000,
    });
    await expect(page.getByTestId("seat-profile").getByTestId("seat-profile-pending")).toHaveCount(0, {
      timeout: 20_000,
    });
    const view = await page.request.get(`/v1/lineages/${firstId}/profile`);
    const body = (await view.json()) as { pending: boolean; liveChapter: { appliedRevision: number | null } };
    expect(body.pending).toBe(false);
    expect(body.liveChapter.appliedRevision).toBe(2);
    // The successor launched with the profile's first candidate.
    const row = await page.request.get(`/v1/instances/${secondId}`);
    expect(((await row.json()) as { model?: string }).model).toBe("e2e/model-b");
  });
});

test.describe("phone seat profile", () => {
  test("iPhone 13: the panel and 「待下一章节生效」 fit the session page", async ({ browser }) => {
    const context = await browser.newContext({ ...devices["iPhone 13"] });
    const page = await context.newPage();
    try {
      await login(page);
      const mainId = await seatMain(page, "seat-profile phone");
      await page.goto(`/s/${mainId}`);
      const panel = page.getByTestId("seat-profile");
      await expect(panel).toBeVisible({ timeout: 20_000 });
      const saved = await putProfile(page, mainId, { expectedRevision: 0, effort: { name: "high" } });
      expect(saved.ok(), await saved.text()).toBe(true);
      await page.reload();
      await expect(panel.getByTestId("seat-profile-pending")).toHaveText("待下一章节生效", { timeout: 20_000 });
      const box = await panel.boundingBox();
      const viewport = page.viewportSize();
      expect(box && viewport && box.x >= 0 && box.x + box.width <= viewport.width + 1).toBe(true);
      await shot(page, "seat-profile-390-pending.png");
    } finally {
      await cleanup(page);
      await context.close();
    }
  });
});
