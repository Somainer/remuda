import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { seedMode } from "./appearanceHelper";
import { login } from "./hub-auth";

/**
 * D-060 / cb-memory-ui: the PWA memory page, the about-me editor and the
 * 「已记住」 rows' entry points.
 *
 * Self-skips unless the in-process fake Node (`e2e-fake-node`) is enrolled:
 * every case needs one real instance row so the session page (and its ⋯ menu
 * entry point) can render. Memory entries themselves are written through the
 * owner REST routes, so the page is exercised against the real Hub.
 *
 * Runs in two projects, each in light and dark:
 *  - `playwright.hub.config.ts` (chromium): 1440 desktop;
 *  - `playwright.mrealdevice.config.ts` (WebKit, iPhone 13): 390, with the
 *    soft keyboard raised on the editor the same way `m-realdevice` does.
 *
 * Evidence PNGs are written only when REMUDA_EVIDENCE=1, under
 * `web/test-results/memory-ui-evidence` (or REMUDA_EVIDENCE_DIR).
 */

test.describe.configure({ mode: "serial" });

const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = process.env.REMUDA_EVIDENCE_DIR
  ? path.resolve(process.env.REMUDA_EVIDENCE_DIR)
  : path.join(here, "../../test-results/memory-ui-evidence");

const created: string[] = [];
const THEMES = ["light", "dark"] as const;

/** True on the WebKit iPhone project (390); false on the desktop project. */
function isPhone(): boolean {
  return test.info().project.name !== "chromium";
}

async function shot(page: Page, name: string, bandOnly = false) {
  if (!evidence) return;
  await mkdir(shotDir, { recursive: true });
  const clip = await page.evaluate((band) => {
    const width = Math.min(390, window.innerWidth);
    const x = Math.max(0, Math.round((window.innerWidth - width) / 2));
    return band
      ? { x, y: window.visualViewport?.offsetTop ?? 0, width, height: window.visualViewport?.height ?? window.innerHeight }
      : { x: 0, y: 0, width: window.innerWidth, height: window.innerHeight };
  }, bandOnly);
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled", clip });
}

/**
 * Raise the soft keyboard the headless way (m-realdevice's idiom): the visual
 * viewport becomes the short band starting at offsetTop and its listeners fire.
 */
async function raiseKeyboard(page: Page, keyboardHeight = 308) {
  await page.evaluate((kb) => {
    const vv = window.visualViewport;
    const height = Math.max(240, window.innerHeight - kb);
    const offsetTop = window.innerHeight - height;
    const override = (name: string, value: number) => {
      const desc: PropertyDescriptor = { configurable: true, get: () => value };
      try {
        Object.defineProperty(vv, name, desc);
      } catch {
        let proto: object | null = vv;
        while (proto) {
          try {
            Object.defineProperty(proto, name, desc);
            break;
          } catch {
            proto = Object.getPrototypeOf(proto);
          }
        }
      }
    };
    override("height", height);
    override("offsetTop", offsetTop);
    vv.dispatchEvent(new Event("resize"));
    vv.dispatchEvent(new Event("scroll"));
    window.dispatchEvent(new Event("resize"));
  }, keyboardHeight);
}

async function fakeHostId(page: Page): Promise<string | null> {
  const response = await page.request.get("/v1/hosts");
  if (!response.ok()) return null;
  const body = (await response.json()) as { items?: { id?: string; hostId?: string; label?: string }[] };
  const host = (body.items ?? []).find((row) => row.label === "e2e-fake-node");
  return host?.hostId ?? host?.id ?? null;
}

async function seedSession(page: Page): Promise<string | null> {
  const hostId = await fakeHostId(page);
  if (!hostId) return null;
  const response = await page.request.post("/v1/instances", {
    data: {
      hostId,
      workspaceId: "wsp_e2e",
      kind: "claude",
      driver: "claude-print",
      title: "记忆页会话",
      prompt: "memoryui-seed",
    },
  });
  expect(response.ok(), `create session: ${response.status()} ${await response.text()}`).toBe(true);
  const body = (await response.json()) as { instance?: { instanceId?: string; id?: string } };
  const id = body.instance?.instanceId ?? body.instance?.id;
  expect(id, "created instance id").toBeTruthy();
  created.push(id!);
  return id!;
}

type SeedEntry = { slug: string; type: string; title: string; description: string; body: string };

async function writeEntry(page: Page, lineageId: string, entry: SeedEntry) {
  const response = await page.request.post(`/v1/lineages/${lineageId}/memory`, { data: entry });
  expect(
    response.ok(),
    `write ${entry.slug}: ${response.status()} ${await response.text()}`,
  ).toBe(true);
  return (await response.json()) as { entry: { revision: number; body: string } };
}

async function saveEntry(page: Page, lineageId: string, slug: string, expectedRevision: number, body: string) {
  const response = await page.request.put(`/v1/lineages/${lineageId}/memory/entries/${slug}`, {
    data: { expectedRevision, body },
  });
  expect(response.ok(), `update ${slug}: ${response.status()} ${await response.text()}`).toBe(true);
  return (await response.json()) as { entry: { revision: number; body: string } };
}

async function readEntry(page: Page, lineageId: string, slug: string) {
  const response = await page.request.get(`/v1/lineages/${lineageId}/memory/entries/${slug}?includeArchived=true`);
  if (!response.ok()) return null;
  return (await response.json()) as { revision: number; body: string; archivedAt?: string | null };
}

async function expectNoHorizontalOverflow(page: Page) {
  const overflow = await page.evaluate(() => ({
    scroll: document.documentElement.scrollWidth,
    client: document.documentElement.clientWidth,
  }));
  expect(
    overflow.scroll,
    `page overflows horizontally: ${overflow.scroll} > ${overflow.client}`,
  ).toBeLessThanOrEqual(overflow.client + 1);
}

test.beforeEach(async ({ page }) => {
  await login(page, "e2e-memory-ui");
});

test.afterEach(async ({ page }) => {
  for (const id of created.splice(0)) {
    await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
  }
});

for (const theme of THEMES) {
  const label = (phone: boolean) => (phone ? "390-webkit" : "1440");

  test(`${theme}: the memory page groups by type and searches CJK by substring`, async ({ page }) => {
    const phone = isPhone();
    await seedMode(page, theme);
    if (!phone) await page.setViewportSize({ width: 1440, height: 900 });
    const id = await seedSession(page);
    test.skip(!id, "needs the in-process fake Node (e2e-fake-node)");
    const lineage = id!;
    await writeEntry(page, lineage, {
      slug: "pref",
      type: "user",
      title: "称呼",
      description: "怎么称呼我",
      body: "叫我老王。边界条件也用中文写清楚。",
    });
    await writeEntry(page, lineage, {
      slug: "why-terse",
      type: "feedback",
      title: "回答要短",
      description: "Why 与 How to apply",
      body: "Why: 每次解释太长。\nHow to apply: 先给结论再展开。",
    });
    await writeEntry(page, lineage, {
      slug: "repo",
      type: "project",
      title: "仓库",
      description: "主要工作目录",
      body: "当前项目在 remuda 仓库。",
    });
    await writeEntry(page, lineage, {
      slug: "docs",
      type: "reference",
      title: "文档位置",
      description: "设计文档",
      body: "设计文档在 docs/design。",
    });

    await page.goto(`/s/${lineage}/memory`);
    await expect(page.getByTestId("memory-page")).toBeVisible({ timeout: 20_000 });
    for (const kind of ["user", "feedback", "project", "reference"]) {
      await expect(page.getByTestId(`memory-group-${kind}`)).toBeVisible();
    }
    await expectNoHorizontalOverflow(page);
    await shot(page, `memory-ui-${label(phone)}-${theme}-page.png`, phone);

    // CJK: unicode61 sees the run as one token, so this only finds it through
    // the Hub's LIKE fallback.
    await page.getByTestId("memory-search").fill("边界");
    await expect(page.getByTestId("memory-row")).toHaveCount(1, { timeout: 20_000 });
    await expect(page.getByTestId("memory-row").first()).toContainText("称呼");
    await page.getByTestId("memory-search").fill("");
    await expect(page.getByTestId("memory-row")).toHaveCount(4, { timeout: 20_000 });

    // The ⋯ menu is the entry point; it opens the same route.
    await page.goto(`/s/${lineage}`);
    await expect(page.getByTestId("session-page")).toBeVisible({ timeout: 20_000 });
    await page.getByTestId("session-more-open").click();
    await page.getByTestId("memory-open").click();
    await expect(page).toHaveURL(new RegExp(`/s/${lineage}/memory`));
    await expect(page.getByTestId("memory-page")).toBeVisible();

    // Edit in place; on the phone raise the keyboard and prove the editor
    // stays inside the visual band (it must never sit under the keyboard).
    await page.getByTestId("memory-row").filter({ hasText: "称呼" }).first().click();
    await expect(page.getByTestId("memory-editor")).toBeVisible();
    await page.getByTestId("memory-editor-body").fill("叫我老王。边界条件也用中文写清楚。更新一版。");
    if (phone) {
      await page.getByTestId("memory-editor-body").focus();
      await raiseKeyboard(page);
      const geometry = await page.getByTestId("memory-editor").evaluate((el) => {
        const rect = el.getBoundingClientRect();
        const vv = window.visualViewport!;
        return {
          top: rect.top,
          bottom: rect.bottom,
          bandTop: vv.offsetTop,
          bandBottom: vv.offsetTop + vv.height,
        };
      });
      expect(geometry.top).toBeGreaterThanOrEqual(geometry.bandTop - 1);
      expect(geometry.bottom).toBeLessThanOrEqual(geometry.bandBottom + 1);
    }
    await shot(page, `memory-ui-${label(phone)}-${theme}-editor.png`, phone);
    await page.getByTestId("memory-editor-save").click();
    await expect(page.getByTestId("memory-editor")).toBeHidden({ timeout: 20_000 });
    const saved = await readEntry(page, lineage, "pref");
    expect(saved?.body).toContain("更新一版");
  });

  test(`${theme}: a stale edit shows both bodies and 保留我的 overwrites with the fresh revision`, async ({
    page,
  }) => {
    const phone = isPhone();
    await seedMode(page, theme);
    if (!phone) await page.setViewportSize({ width: 1440, height: 900 });
    const id = await seedSession(page);
    test.skip(!id, "needs the in-process fake Node (e2e-fake-node)");
    const lineage = id!;
    await writeEntry(page, lineage, {
      slug: "conflict",
      type: "project",
      title: "冲突用例",
      description: "revision CAS",
      body: "服务器第一版",
    });

    await page.goto(`/s/${lineage}/memory`);
    await page.getByTestId("memory-row").filter({ hasText: "冲突用例" }).first().click();
    await expect(page.getByTestId("memory-editor")).toBeVisible();
    await page.getByTestId("memory-editor-body").fill("我的第二版");

    // Someone else (another owner tab, or a seat through a current chapter)
    // moves the entry while the editor is open.
    const theirs = await saveEntry(page, lineage, "conflict", 1, "服务器第二版");
    expect(theirs.entry.revision).toBe(2);

    await page.getByTestId("memory-editor-save").click();
    await expect(page.getByTestId("memory-conflict")).toBeVisible({ timeout: 20_000 });
    await expect(page.getByTestId("memory-conflict-server")).toHaveText("服务器第二版");
    await expect(page.getByTestId("memory-conflict-mine")).toHaveText("我的第二版");
    await shot(page, `memory-ui-${label(phone)}-${theme}-conflict.png`, phone);

    await page.getByTestId("memory-conflict-mine-keep").click();
    await expect(page.getByTestId("memory-conflict")).toBeHidden({ timeout: 20_000 });
    const mine = await readEntry(page, lineage, "conflict");
    expect(mine?.body).toBe("我的第二版");
    expect(mine?.revision).toBe(3);

    // The page shows the winning revision.
    await expect(page.getByTestId("memory-row").filter({ hasText: "冲突用例" }).first()).toContainText("rev 3");
  });

  test(`${theme}: about-me saves in place, delete confirms, and an archived entry restores`, async ({ page }) => {
    const phone = isPhone();
    await seedMode(page, theme);
    if (!phone) await page.setViewportSize({ width: 1440, height: 900 });
    const id = await seedSession(page);
    test.skip(!id, "needs the in-process fake Node (e2e-fake-node)");
    const lineage = id!;
    await writeEntry(page, lineage, {
      slug: "temporary",
      type: "reference",
      title: "临时记忆",
      description: "会被删除",
      body: "删除我。",
    });

    await page.goto(`/s/${lineage}/memory`);
    await expect(page.getByTestId("memory-about")).toBeVisible({ timeout: 20_000 });
    await page.getByTestId("memory-about-body").fill("我是这个 Hub 的所有者，偏好简短回答。");
    await page.getByTestId("memory-about-save").click();
    await expect
      .poll(async () => (await readEntry(page, lineage, "about-me"))?.body, { timeout: 20_000 })
      .toContain("偏好简短回答");
    await shot(page, `memory-ui-${label(phone)}-${theme}-about.png`, phone);

    // Delete asks first, then removes with expectedRevision.
    await page.getByTestId("memory-row").filter({ hasText: "临时记忆" }).first().click();
    await page.getByTestId("memory-editor-delete").click();
    await expect(page.getByTestId("memory-delete-confirm")).toBeVisible();
    await page.getByTestId("memory-delete-confirm-yes").click();
    await expect
      .poll(async () => await readEntry(page, lineage, "temporary"), { timeout: 20_000 })
      .toBeNull();
    await expect(page.getByTestId("memory-row").filter({ hasText: "临时记忆" })).toHaveCount(0);

    // Retiring the lineage archives its memory; the owner can still reach the
    // archived entry from the route and restore it.
    await writeEntry(page, lineage, {
      slug: "keep-me",
      type: "project",
      title: "归档后恢复",
      description: "archived",
      body: "先归档，再恢复。",
    });
    const retired = await page.request.delete(`/v1/instances/${lineage}?force=1`);
    expect(retired.ok(), `retire: ${retired.status()} ${await retired.text()}`).toBe(true);
    await page.goto(`/s/${lineage}/memory`);
    const archivedRow = page.getByTestId("memory-row").filter({ hasText: "归档后恢复" });
    await expect(archivedRow).toBeVisible({ timeout: 20_000 });
    await expect(archivedRow).toHaveAttribute("data-archived", "1");
    await shot(page, `memory-ui-${label(phone)}-${theme}-archived.png`, phone);
    await archivedRow.getByTestId("memory-restore").click();
    await expect
      .poll(async () => (await readEntry(page, lineage, "keep-me"))?.archivedAt ?? null, { timeout: 20_000 })
      .toBeNull();
  });
}
