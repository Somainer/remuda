import {
  devices,
  expect,
  request as apiRequest,
  test,
  type APIRequestContext,
  type Browser,
  type BrowserContext,
  type Page,
  type Route as PlaywrightRoute,
} from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { setMode, type AppearanceMode } from "./appearanceHelper";
import { login } from "./hub-auth";

/**
 * home-evidence (H5, home.md §11; ui-screenshots.md "Home state checklist").
 *
 * `HUB_E2E_HOME_EVIDENCE=1` makes the harness start ADDITIONAL Nodes beside
 * the shared fake Node; the primary `e2e-fake-node` and its g2 workspaces are
 * untouched. The showcase `forge` Node announces the neutral host, the
 * workspace roots /srv/remuda and /srv/atlas (labels 空间: remuda / atlas),
 * journals human assistant lines instead of `echo: <prompt>`, and the
 * `hshowcase-approval` / `hshowcase-plan` create sentinels raise cards whose
 * verbatim text a person would read, and `hshowcase-working` /
 * `hshowcase-idle` open cardless turns (a working session, an idle ledger
 * row) whose state needs no answer round trip. The `laptop` Node announces
 * once and drops, so ② has a real offline host with a 「离线 Nm」 age. This
 * spec seeds the rest (one owner-blocked task, the address-owner Main)
 * through the public routes and releases everything it owns, including its
 * project.
 *
 * Matrix: routes {/m at 390, /sessions at 1280, /sessions at 1440} ×
 * appearance {light, dark} × the §11 states — queue full, all handled, before
 * the first snapshot, Hub unreachable (banner), data stale, a host offline,
 * Main paused, no Main, soft keyboard up (390 only) and searching.
 *
 * Every state is ASSERTED, not only photographed: the spec is a test with
 * REMUDA_EVIDENCE set, and a default run writes nothing.
 *
 * Screenshots go to the ignored `web/scratch/evidence/home-evidence/` (D-060
 * Q6 is undecided, so no PNG enters git). They are written ONLY when
 * REMUDA_EVIDENCE=1; without it (or without the showcase Nodes) the whole
 * file skips.
 *
 * No assertion uses a millisecond threshold: every wait is for an eventual
 * state. The one fixed 500 ms pause is a bounded delete retry (ma-fence keeps
 * a closing chapter's delete at 409), never an expected-duration assertion.
 */

test.describe.configure({ mode: "serial" });

const evidence = process.env.REMUDA_EVIDENCE === "1";
const showcase = process.env.HUB_E2E_HOME_EVIDENCE === "1";

test.skip(
  !evidence,
  "set REMUDA_EVIDENCE=1 to capture the home evidence matrix; the default run writes no PNG",
);
test.skip(
  !showcase,
  "set HUB_E2E_HOME_EVIDENCE=1: the harness must start the showcase and laptop Nodes",
);

const here = path.dirname(fileURLToPath(import.meta.url));
/** Ignored output directory: never committed (home.md §11, D-060 Q6 open). */
const shotDir = path.join(here, "../../scratch/evidence/home-evidence");

type HomeRoute = {
  name: "390" | "1280" | "1440";
  width: number;
  height: number;
  path: string;
  phone: boolean;
};

const ROUTES: HomeRoute[] = [
  { name: "390", width: 390, height: 844, path: "/m", phone: true },
  { name: "1280", width: 1280, height: 900, path: "/sessions?scope=all", phone: false },
  { name: "1440", width: 1440, height: 900, path: "/sessions?scope=all", phone: false },
];

async function shot(page: Page, state: string, route: HomeRoute, mode: AppearanceMode, suffix = "") {
  if (!evidence) return;
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({
    path: path.join(shotDir, `home-evidence-${state}-${route.name}-${mode}${suffix}.png`),
    animations: "disabled",
  });
}

/**
 * The soft-keyboard state is the visual band only. The layout viewport still
 * spans the full 844 px (the shell is pinned to the band at offsetTop), so a
 * plain viewport shot would show the blank strip above the band: clip to the
 * band the operator actually sees.
 */
async function keyboardShot(page: Page, mode: AppearanceMode) {
  if (!evidence) return;
  const band = await page.evaluate(() => ({
    top: Math.round(window.visualViewport?.offsetTop ?? 0),
    width: Math.round(window.visualViewport?.width ?? window.innerWidth),
    height: Math.round(window.visualViewport?.height ?? window.innerHeight),
  }));
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({
    path: path.join(shotDir, `home-evidence-keyboard-up-390-${mode}.png`),
    animations: "disabled",
    clip: { x: 0, y: band.top, width: band.width, height: band.height },
  });
}

/** Capture one settled state on every route in both appearances. */async function captureState(
  browser: Browser,
  baseURL: string,
  state: string,
  settle: (page: Page, route: HomeRoute) => Promise<void>,
  routeFilter?: (route: HomeRoute) => boolean,
) {
  for (const route of ROUTES) {
    if (routeFilter && !routeFilter(route)) continue;
    const { context, page } = await openRoute(browser, baseURL, route);
    try {
      await page.goto(route.path);
      await settle(page, route);
      for (const mode of ["light", "dark"] as const) {
        await setMode(page, mode);
        await shot(page, state, route, mode);
      }
    } finally {
      await context.close();
    }
  }
}

async function openRoute(
  browser: Browser,
  baseURL: string,
  route: HomeRoute,
  { showPrimary = false }: { showPrimary?: boolean } = {},
) {
  const context: BrowserContext = route.phone
    ? await browser.newContext({ ...devices["iPhone 13"], viewport: { width: route.width, height: route.height }, baseURL })
    : await browser.newContext({ baseURL, viewport: { width: route.width, height: route.height } });
  const page = await context.newPage();
  if (!showPrimary) {
    // The shared `e2e-fake-node` is the harness's primary fixture, not part
    // of the showcase. Hide it from the APP's host reads so every shot carries
    // only the neutral `forge`/`laptop` hosts (home.md §11). The spec's own
    // API calls use `page.request`, which page routes do not intercept, so
    // seeding and the isolation test still see the primary.
    await page.route(/\/v1\/hosts(\?.*)?$/, async (intercepted) => {
      if (intercepted.request().method() !== "GET") return intercepted.fallback();
      const response = await intercepted.fetch();
      const body = (await response.json()) as { items?: { label?: string }[] };
      body.items = (body.items ?? []).filter((row) => row.label !== "e2e-fake-node");
      await intercepted.fulfill({ response, json: body });
    });
  }
  await login(page, `evidence-${route.name}`);
  lastStorage = await context.storageState();
  return { context, page };
}

/** The last signed-in cookie jar, so afterAll can release a timed-out seed. */
let lastStorage: Awaited<ReturnType<BrowserContext["storageState"]>> | null = null;

const createdInstances: string[] = [];
const createdTasks: string[] = [];
const createdProjects: string[] = [];
/** Address-owner seats (they need the pause-then-delete routine). */
const mainSeats: string[] = [];

async function apiJson<T>(page: Page, method: string, pathName: string, body?: unknown): Promise<T> {
  const response = await page.request.fetch(pathName, {
    method,
    data: body,
    headers: body ? { "content-type": "application/json" } : undefined,
  });
  const text = await response.text();
  expect(response.ok(), `${method} ${pathName} → ${response.status()}: ${text}`).toBe(true);
  return (text ? JSON.parse(text) : null) as T;
}

type HostRow = {
  id?: string;
  label?: string;
  state?: string;
  online?: boolean;
  lastSeenAt?: string | null;
  workspaces?: { workspaceId?: string; root?: string }[];
};

async function hubHosts(page: Page): Promise<HostRow[]> {
  const body = await apiJson<{ items?: HostRow[] }>(page, "GET", "/v1/hosts");
  return body.items ?? [];
}

/** The showcase host: the `forge` Node the harness starts beside the primary. */
async function showcaseHost(page: Page): Promise<string> {
  let id = "";
  await expect
    .poll(
      async () => {
        const host = (await hubHosts(page)).find((row) => row.label === "forge");
        id = host?.id ?? "";
        return id;
      },
      { timeout: 20_000 },
    )
    .not.toBe("");
  return id;
}

async function createSession(page: Page, prompt: string, title: string): Promise<string> {
  const hostId = await showcaseHost(page);
  const launched = await apiJson<{ instance?: { instanceId?: string; id?: string } }>(page, "POST", "/v1/instances", {
    hostId,
    workspaceId: "wsp_e2e",
    kind: "claude",
    driver: "claude-print",
    title,
    prompt,
  });
  const instanceId = launched.instance?.instanceId ?? launched.instance?.id;
  expect(instanceId, `created ${title}`).toBeTruthy();
  createdInstances.push(instanceId!);
  return instanceId!;
}

type PendingRow = {
  id?: string;
  interactionId?: string;
  instanceId?: string;
  state?: string;
  kind?: string;
  request?: {
    inputDigest?: string;
    kind?: string;
    fields?: { id?: string; options?: { id?: string }[] }[];
    planRevision?: string;
    planDigest?: string;
  };
};

async function pendingFor(page: Page, instanceId: string): Promise<PendingRow | undefined> {
  const list = await apiJson<{ items?: PendingRow[] }>(page, "GET", `/v1/interactions?instanceId=${instanceId}`);
  return (list.items ?? []).find((row) => row.instanceId === instanceId && row.state === "pending");
}

/** Wait until the fake Node's card for `instanceId` is served as pending. */
async function waitPending(page: Page, instanceId: string): Promise<string> {
  let found = "";
  await expect
    .poll(
      async () => {
        const row = await pendingFor(page, instanceId);
        found = row?.interactionId ?? row?.id ?? "";
        return found;
      },
      { timeout: 20_000 },
    )
    .not.toBe("");
  return found;
}

async function pendingCount(page: Page): Promise<number> {
  const list = await apiJson<{ items?: PendingRow[] }>(page, "GET", "/v1/interactions");
  return (list.items ?? []).filter((row) => row.state === "pending").length;
}

/** An owner-blocked task with no session: one 「任务待你」 card (§4.6). */
async function blockedTask(page: Page, projectName: string, title: string, reason: string): Promise<string> {
  const project = await apiJson<{ id: string }>(page, "POST", "/v1/projects", { name: projectName });
  createdProjects.push(project.id);
  const task = await apiJson<{ id: string }>(page, "POST", "/v1/tasks", {
    projectId: project.id,
    title,
    intent: title,
  });
  createdTasks.push(task.id);
  await apiJson(page, "PATCH", `/v1/tasks/${task.id}`, { state: "failed", reason });
  return task.id;
}

async function archiveTask(page: Page, taskId: string): Promise<void> {
  await page.request.post(`/v1/tasks/${taskId}/archive`, { data: {} }).catch(() => undefined);
}

/** Remove the seeded project so nothing outlives the spec (item 3). */
async function deleteProject(page: Page, projectId: string): Promise<void> {
  const origin = new URL(page.url()).origin;
  await page.request
    .delete(`/v1/projects/${projectId}`, { headers: { Origin: origin } })
    .catch(() => undefined);
  const at = createdProjects.indexOf(projectId);
  if (at >= 0) createdProjects.splice(at, 1);
}

/** Seat the address-owner Main the way a human device does (ma-main-ui H8a). */
async function seatMain(page: Page, prompt: string, title: string): Promise<string> {
  const hostId = await showcaseHost(page);
  const response = await page.request.post("/v1/instances", {
    data: {
      hostId,
      workspaceId: "wsp_e2e",
      kind: "claude",
      driver: "claude-print",
      // The desktop Main card's read-only model chip: a human family name,
      // never the fixture's wire default.
      model: "sonnet",
      prompt,
      title,
      role: "top-coordinator",
      grants: ["address-owner"],
    },
  });
  expect(response.ok(), `seat Main: ${response.status()} ${await response.text()}`).toBe(true);
  const body = (await response.json()) as { instance: { instanceId: string } };
  createdInstances.push(body.instance.instanceId);
  mainSeats.push(body.instance.instanceId);
  return body.instance.instanceId;
}

/**
 * Release an address-owner seat: close (pause for a continuity lineage), wait
 * for the pause, then force-delete the chapter. Newest first: deleting a
 * lineage's current chapter removes every chapter.
 */
async function releaseMain(page: Page, id: string): Promise<void> {
  const origin = new URL(page.url()).origin;
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
  await expect.poll(async () => (await page.request.get(`/v1/instances/${id}`)).status(), { timeout: 15_000 }).toBe(404);
  const seat = mainSeats.indexOf(id);
  if (seat >= 0) mainSeats.splice(seat, 1);
  const at = createdInstances.indexOf(id);
  if (at >= 0) createdInstances.splice(at, 1);
}

async function deleteInstance(page: Page, id: string): Promise<void> {
  await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
  await expect.poll(async () => (await page.request.get(`/v1/instances/${id}`)).status(), { timeout: 15_000 }).toBe(404);
  const at = createdInstances.indexOf(id);
  if (at >= 0) createdInstances.splice(at, 1);
}

/** Release the queue-full fixture (never the working session). */
async function releaseQueue(page: Page, state: Showcase) {
  for (const id of [state.approvalInstanceId, state.chipsInstanceId, state.planInstanceId]) {
    await deleteInstance(page, id);
  }
  // A parked question would keep the pause from settling: answer it first.
  await releaseMain(page, state.mainId).catch(() => undefined);
  await archiveTask(page, state.taskId);
  // The project is part of the seed; nothing it created may outlive the spec.
  for (const id of [...createdProjects]) await deleteProject(page, id);
  await expect.poll(() => pendingCount(page), { timeout: 30_000 }).toBe(0);
}

async function waitQueue(page: Page) {
  await expect(page.getByTestId("home-queue")).toBeVisible({ timeout: 20_000 });
}

function queueCard(page: Page, id: string) {
  return page.locator(`[data-testid="home-queue-card"][data-card-id="${id}"]`);
}

async function expandQueue(page: Page) {
  const more = page.getByTestId("home-queue-more");
  if ((await more.count()) && (await more.getAttribute("aria-expanded")) === "false") await more.click();
}

async function collapseQueue(page: Page) {
  const more = page.getByTestId("home-queue-more");
  if ((await more.count()) && (await more.getAttribute("aria-expanded")) === "true") await more.click();
}

/** Bring the queue into the viewport for a state shot (the phone page scrolls). */
async function showQueue(page: Page) {
  await page.getByTestId("home-queue").evaluate((el) => el.scrollIntoView({ block: "start" }));
}

/**
 * Raise the soft keyboard the headless way (m-realdevice.hub.spec.ts): the
 * visual viewport becomes the short band starting at offsetTop, and the
 * listeners the app subscribes fire.
 */
async function raiseKeyboard(page: Page, keyboardHeight = 308) {
  await page.evaluate((height) => {
    const vv = window.visualViewport;
    const h = Math.max(240, window.innerHeight - height);
    const top = window.innerHeight - h;
    const override = (name: string, value: number) => {
      const desc = { configurable: true, get: () => value } as PropertyDescriptor;
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
    override("height", h);
    override("offsetTop", top);
    vv.dispatchEvent(new Event("resize"));
    vv.dispatchEvent(new Event("scroll"));
    window.dispatchEvent(new Event("resize"));
  }, keyboardHeight);
}

async function lowerKeyboard(page: Page) {
  await page.evaluate(() => {
    const vv = window.visualViewport;
    Object.defineProperty(vv, "height", { configurable: true, get: () => window.innerHeight });
    Object.defineProperty(vv, "offsetTop", { configurable: true, get: () => 0 });
    vv.dispatchEvent(new Event("resize"));
    window.dispatchEvent(new Event("resize"));
  });
}

/** The state the queue-full matrices run on: every card kind §4.2 names. */
/** The queue-full matrix: instance ids for the seed, card ids for lookup. */
type Showcase = {
  mainId: string;
  mainQuestionId: string;
  approvalInstanceId: string;
  approvalCardId: string;
  chipsInstanceId: string;
  chipsCardId: string;
  planInstanceId: string;
  planCardId: string;
  taskId: string;
};

let showcaseState: Showcase | null = null;
let workingSessionId = "";
/** The offline card's INTERACTION id (its data-card-id) and instance id. */
let offlineCardId = "";
let offlineInstanceId = "";

test.afterAll(async () => {
  if (!lastStorage) return;
  const baseURL = test.info().project.use.baseURL ?? "http://127.0.0.1:58889";
  const origin = new URL(baseURL).origin;
  const request = await apiRequest.newContext({ baseURL, storageState: lastStorage });
  try {
    for (const id of createdTasks.splice(0)) {
      await request.post(`/v1/tasks/${id}/archive`, { data: {} }).catch(() => undefined);
    }
    for (const id of [...createdInstances].reverse()) {
      await request
        .post(`/v1/instances/${id}/commands`, {
          headers: { Origin: origin },
          data: { operation: "instance.close", payload: {} },
        })
        .catch(() => undefined);
      await request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    }
    for (const id of createdProjects.splice(0)) {
      await request.delete(`/v1/projects/${id}`, { headers: { Origin: origin } }).catch(() => undefined);
    }
  } finally {
    await request.dispose();
  }
});

/** The supported pre-snapshot state: an offline reload (home-freshness). */
async function failListReads(page: Page) {
  const fail = (route: PlaywrightRoute) => route.abort("failed").catch(() => undefined);
  await page.route("**/v1/instances", fail);
  await page.route("**/v1/interactions", fail);
  // The task ledger would otherwise paint the 「已归档」 group and take the
  // ledger slot the skeletons occupy (item 4: row-independent).
  await page.route(/\/v1\/tasks(\?.*)?$/, fail);
}

/**
 * HUB_E2E_HOME_EVIDENCE=1 must add Nodes, never rewrite the shared one: an
 * existing spec's host label and its g2 workspaces are still there while the
 * env var is set (home-evidence r2 item 2).
 */
test.describe("showcase fixture isolation", () => {
  test("the shared e2e-fake-node and its g2 workspaces are untouched", async ({ browser }) => {
    test.setTimeout(60_000);
    const baseURL = test.info().project.use.baseURL!;
    const { context, page } = await openRoute(browser, baseURL, ROUTES[0], { showPrimary: true });
    try {
      const hosts = await hubHosts(page);
      const primary = hosts.find((row) => row.label === "e2e-fake-node");
      expect(primary?.id, "the shared primary Node still announces e2e-fake-node").toBeTruthy();
      const primaryWorkspaces = (primary?.workspaces ?? []).map((row) => row.workspaceId);
      expect(primaryWorkspaces).toEqual(
        expect.arrayContaining(["wsp_g2_clean", "wsp_g2_changes", "wsp_e2e"]),
      );
      const forge = hosts.find((row) => row.label === "forge");
      expect(forge?.id, "the showcase Node is a SECOND host").toBeTruthy();
      expect(forge?.id).not.toBe(primary?.id);
      expect((forge?.workspaces ?? []).map((row) => row.root)).toEqual(
        expect.arrayContaining(["/srv/remuda", "/srv/atlas"]),
      );
      const laptop = hosts.find((row) => row.label === "laptop");
      expect(laptop?.id, "the harness announces the offline laptop host").toBeTruthy();
      expect(laptop?.lastSeenAt, "laptop has a last-seen so ② can age it").toBeTruthy();
    } finally {
      await context.close();
    }
  });
});

/**
 * The supported pre-snapshot state: an offline reload with every list read
 * failing. It depends on NO rows: the task ledger read is failed too, so an
 * archived task or a cached session can never take the ledger slot.
 */
test.describe("before the first snapshot", () => {
  test("skeletons in ④ and the ledger, 「–」 in ①, never an empty state", async ({ browser }) => {
    test.setTimeout(120_000);
    const baseURL = test.info().project.use.baseURL!;
    await captureState(browser, baseURL, "before-first-snapshot", async (home, route) => {
      await failListReads(home);
      await home.goto(route.path);
      await expect(home.getByTestId("home-queue-skeleton")).toHaveCount(2, { timeout: 20_000 });
      await expect(home.getByTestId("glance-need-you")).toContainText("–", { timeout: 20_000 });
      if (route.phone) {
        await expect(home.getByTestId("home-skeleton").first()).toBeVisible({ timeout: 20_000 });
      } else {
        await expect(home.getByTestId("session-skeleton").first()).toBeVisible({ timeout: 20_000 });
      }
      await expect(home.getByText("暂无会话")).toHaveCount(0);
      await expect(home.getByText("还没有会话")).toHaveCount(0);
    });
  });
});

test.describe("all handled", () => {
  test("zero pending with a working session: the greeting, one-line ④ and 会话", async ({ browser }) => {
    test.setTimeout(240_000);
    const baseURL = test.info().project.use.baseURL!;
    const setup = await openRoute(browser, baseURL, ROUTES[0]);
    try {
      const page = setup.page;
      await page.goto("/m");
      await waitQueue(page);

      // One live session still working: a create raises an approval, answer
      // it, then a hold-working send leaves the turn open with no card.
      // The showcase Node's cardless turn: native working, no interaction.
      workingSessionId = await createSession(page, "hshowcase-working 继续拆分", "拆分发布脚本");
      await apiJson(page, "POST", `/v1/instances/${workingSessionId}/commands`, {
        operation: "instance.send",
        payload: { prompt: "hold-working 继续" },
      });
      await expect.poll(() => pendingCount(page), { timeout: 30_000 }).toBe(0);
    } finally {
      await setup.context.close();
    }

    await captureState(browser, baseURL, "all-handled", async (home, route) => {
      await waitQueue(home);
      await expect(home.getByTestId("glance-greeting")).toContainText("都处理完了", { timeout: 20_000 });
      await expect(home.getByTestId("glance-need-you")).toHaveCount(0);
      await expect(home.getByTestId("home-queue")).toHaveAttribute("data-empty", "1");
      await expect(home.getByTestId("home-queue-title")).toHaveText(/需要你 · 0/);
      await expect(home.getByTestId("home-queue-card")).toHaveCount(0);
      if (route.phone) {
        await expect(home.getByTestId("home-head").getByRole("heading", { level: 1 })).toHaveText("会话");
        await expect(home.locator('[data-testid="home-row"][data-rank="working"]')).toHaveCount(1);
      } else {
        await expect(
          home.locator('[data-testid="board-card"][data-rank="working"]').filter({ hasText: "拆分发布脚本" }),
        ).toBeVisible({ timeout: 20_000 });
      }
      await expect(home.getByTestId("glance-working")).toContainText(/[1-9]\d* 工作中/);
    });
  });
});

test.describe("Hub unreachable and stale data", () => {
  test("the ⑧ banner, then the stale verdict with 数据可能过期 and 更新于 T", async ({ browser }) => {
    test.setTimeout(240_000);
    const baseURL = test.info().project.use.baseURL!;
    const setup = await openRoute(browser, baseURL, ROUTES[0]);
    try {
      const page = setup.page;
      await page.goto("/m");
      await waitQueue(page);
      offlineInstanceId = await createSession(page, "hshowcase-approval", "核对发布清单");
      offlineCardId = await waitPending(page, offlineInstanceId);
    } finally {
      await setup.context.close();
    }

    for (const route of ROUTES) {
      const { context, page } = await openRoute(browser, baseURL, route);
      try {
        await page.goto(route.path);
        await waitQueue(page);
        await showQueue(page);
        await expect(queueCard(page, offlineCardId)).toBeVisible({ timeout: 20_000 });
        await context.setOffline(true);
        // §8 banner: 5 s of consecutive failures, then 重试.
        const banner = page.getByTestId("hub-banner");
        await expect(banner).toBeVisible({ timeout: 30_000 });
        await expect(banner).toContainText("连不上 Hub");
        await expect(banner).toContainText("上次更新");
        await expect(page.getByTestId("hub-banner-retry")).toHaveText("重试");
        for (const mode of ["light", "dark"] as const) {
          await setMode(page, mode);
          await shot(page, "hub-unreachable", route, mode);
        }
        // §8 stale: the cached queue greys its buttons and names the last
        // successful update. (The link is down, so the 5 s banner is up too:
        // §11's two states share one screen; each is asserted in its own
        // right — the banner by its copy and 重试, the stale verdict by the
        // greyed buttons, 「数据可能过期」 and 「更新于 T」.)
        const stale = queueCard(page, offlineCardId).getByTestId("home-card-stale");
        await expect(stale).toHaveText("数据可能过期", { timeout: 30_000 });
        await expect(queueCard(page, offlineCardId).getByRole("button", { name: "允许一次" })).toBeDisabled();
        await expect(page.getByTestId("glance-updated")).toContainText("更新于", { timeout: 30_000 });
        for (const mode of ["light", "dark"] as const) {
          await setMode(page, mode);
          await shot(page, "data-stale", route, mode);
        }
        await context.setOffline(false);
        await expect(banner).toHaveCount(0, { timeout: 30_000 });
      } finally {
        await context.setOffline(false);
        await context.close();
      }
    }

    const cleanupRun = await openRoute(browser, baseURL, ROUTES[0]);
    try {
      await cleanupRun.page.goto("/m");
      await deleteInstance(cleanupRun.page, offlineInstanceId);
      offlineCardId = "";
      offlineInstanceId = "";
    } finally {
      await cleanupRun.context.close();
    }
  });
});

test.describe("host offline", () => {
  test("② puts the offline host first, dashed and grey, 离线 Nm; no green", async ({ browser }) => {
    test.setTimeout(120_000);
    const baseURL = test.info().project.use.baseURL!;
    // No host is registered here: the harness's own `laptop` Node announced
    // once and dropped, so ② ages it without any SSH target (and therefore
    // without an SSH supervisor probing a real machine).
    await captureState(browser, baseURL, "host-offline", async (home) => {
      await waitQueue(home);
      await expect(home.getByTestId("host-summary")).toHaveCount(0);
      // home.md §2.1: the offline host sorts FIRST, dashed and grey, with how
      // long it has been away (ui-screenshots: 「离线 Nm」).
      const first = home.getByTestId("host-chip").first();
      await expect(first).toHaveAttribute("data-offline", "1", { timeout: 20_000 });
      await expect(first).toContainText("laptop");
      // The age becomes a minute once the dropped link is old enough; the
      // strip re-renders on its own clock, so this is an eventual state.
      await expect(first).toContainText(/离线\s+\d+[mhd]/, { timeout: 120_000 });
      const forge = home.locator('[data-testid="host-chip"]').filter({ hasText: "forge" });
      await expect(forge).toHaveAttribute("data-offline", "0");
      // No green anywhere in the strip (home.md §0.3): text, background and
      // every border side, not only the text colour.
      const greens = await home.getByTestId("host-strip").evaluate((el) => {
        const rgba = /^rgba?\(([\d.]+),\s*([\d.]+),\s*([\d.]+)(?:,\s*([\d.]+))?\)$/;
        const isGreen = (value: string) => {
          const match = value.match(rgba);
          if (!match) return false;
          const [red, green, blue] = [Number(match[1]), Number(match[2]), Number(match[3])];
          const alpha = match[4] === undefined ? 1 : Number(match[4]);
          return alpha > 0 && green > red + 24 && green > blue + 24;
        };
        return [...el.querySelectorAll("*")].flatMap((node) => {
          const style = getComputedStyle(node);
          return [
            style.color,
            style.backgroundColor,
            style.borderTopColor,
            style.borderRightColor,
            style.borderBottomColor,
            style.borderLeftColor,
          ].filter(isGreen);
        });
      });
      expect(greens).toEqual([]);
    });
  });
});

test.describe("soft keyboard up (390)", () => {
  test("focusing ⑨ hides PhoneNav, keeps the dock and the list on the band", async ({ browser }) => {
    test.setTimeout(180_000);
    const baseURL = test.info().project.use.baseURL!;
    const setup = await openRoute(browser, baseURL, ROUTES[0]);
    let mainId = "";
    try {
      const page = setup.page;
      await page.goto("/m");
      await expect(page.getByTestId("home-list")).toBeVisible({ timeout: 20_000 });
      // Cardless turn: a live Main with no launch approval to clear.
      mainId = await seatMain(page, "hshowcase-working 值班中", "值班 Main");
      await page.goto("/m");
      await expect(page.getByTestId("main-ask-dock")).toBeVisible({ timeout: 20_000 });
      const input = page.getByTestId("main-ask-dock").getByTestId("composer-input");
      await input.tap();
      await expect(input).toBeFocused();
      await raiseKeyboard(page);
      await expect(page.locator("html")).toHaveAttribute("data-keyboard", "1");
      await expect(page.locator("html")).toHaveAttribute("data-keyboard-focus", "composer");
      await expect(page.getByTestId("phone-nav")).toBeHidden();
      await expect(page.getByTestId("main-ask-dock")).toBeVisible();
      await expect(page.getByTestId("home-list")).toBeVisible();
      await expect(page.locator('[data-testid="home-row"]').first()).toBeVisible();
      // ui-screenshots asks for the 快捷提问 chips above the input while the
      // keyboard is up. They belong to `home-main-extras` (home.md §6.3) and
      // are not in this build: assert them the moment that slice lands,
      // rather than pretending a surface the UI does not have.
      const quickAsk = page.getByTestId("home-quick-ask");
      if ((await quickAsk.count()) > 0) {
        await expect(quickAsk).toBeVisible();
      } else {
        test.info().annotations.push({
          type: "uncovered",
          description: "home-main-extras: quick-ask chips above the focused ⑨",
        });
      }
      for (const mode of ["light", "dark"] as const) {
        await setMode(page, mode);
        await keyboardShot(page, mode);
      }
      await lowerKeyboard(page);
      await expect(page.locator("html")).not.toHaveAttribute("data-keyboard");
    } finally {
      await setup.context.close();
      if (mainId) {
        const cleanup = await openRoute(browser, baseURL, ROUTES[0]);
        try {
          await cleanup.page.goto("/m");
          await releaseMain(cleanup.page, mainId).catch(() => undefined);
        } finally {
          await cleanup.context.close();
        }
      }
    }
  });
});

test.describe("Main paused", () => {
  test("③ reads 已暂停 and ⑨ becomes 恢复", async ({ browser }) => {
    test.setTimeout(180_000);
    const baseURL = test.info().project.use.baseURL!;
    const setup = await openRoute(browser, baseURL, ROUTES[0]);
    const page = setup.page;
    try {
      await page.goto("/m");
      await expect(page.getByTestId("home-list")).toBeVisible({ timeout: 20_000 });
      await seatMain(page, "hshowcase-paused mhome-exit", "值班 Main");
      await expect
        .poll(async () => (await page.request.get("/v1/instances")).ok(), { timeout: 20_000 })
        .toBe(true);
    } finally {
      await setup.context.close();
    }

    await captureState(browser, baseURL, "main-paused", async (home) => {
      const card = home.getByTestId("main-card");
      await expect(card).toBeVisible({ timeout: 20_000 });
      await expect(card.getByTestId("main-card-status")).toHaveText(/^已暂停/, { timeout: 20_000 });
      const resume = home.getByTestId("main-card").getByTestId("main-ask-resume");
      const dockResume = home.getByTestId("main-ask-dock").getByTestId("main-ask-resume");
      await expect(resume.or(dockResume).first()).toContainText("恢复");
    });
  });
});

test.describe("no Main", () => {
  test("③ and ⑨ are absent, and the phone band is PhoneNav only", async ({ browser }) => {
    test.setTimeout(180_000);
    const baseURL = test.info().project.use.baseURL!;
    const cleanup = await openRoute(browser, baseURL, ROUTES[0]);
    try {
      await cleanup.page.goto("/m");
      await expect(cleanup.page.getByTestId("home-list")).toBeVisible({ timeout: 20_000 });
      for (const id of [...mainSeats]) {
        await releaseMain(cleanup.page, id).catch(() => undefined);
      }
    } finally {
      await cleanup.context.close();
    }

    await captureState(browser, baseURL, "no-main", async (home, route) => {
      await expect(home.getByTestId("home-list").or(home.getByTestId("session-list")).first()).toBeVisible({
        timeout: 20_000,
      });
      await expect(home.getByTestId("main-card")).toHaveCount(0);
      await expect(home.getByTestId("main-ask-dock")).toHaveCount(0);
      if (route.phone) await expect(home.getByTestId("phone-nav")).toBeVisible();
    });
  });
});

test.describe("queue full", () => {
  test("five pending cards of every §4.2 kind, the 3/4 fold and its expansion", async ({ browser }) => {
    test.setTimeout(240_000);
    const baseURL = test.info().project.use.baseURL!;
    const seed = await openRoute(browser, baseURL, ROUTES[0]);
    const page = seed.page;
    try {
      await page.goto("/m");
      await waitQueue(page);
      // Main asks a two-field question; the workers raise the other kinds.
      const mainId = await seatMain(page, "hshowcase-main ask-question", "整理上线清单");
      const approvalInstanceId = await createSession(page, "hshowcase-approval", "审查支付迁移");
      const planInstanceId = await createSession(page, "hshowcase-plan", "拆分发布流程");
      const chipsInstanceId = await createSession(page, "hqueue-chips", "旧迁移的处理意见");
      const taskId = await blockedTask(page, "发布流程", "确认验收范围", "等你确认验收范围");
      // An interaction card's data-card-id is the INTERACTION id (a task
      // card's is the task id), so the lookups keep those, not the instances.
      const mainQuestionId = await waitPending(page, mainId);
      const approvalCardId = await waitPending(page, approvalInstanceId);
      const planCardId = await waitPending(page, planInstanceId);
      const chipsCardId = await waitPending(page, chipsInstanceId);
      showcaseState = {
        mainId,
        mainQuestionId,
        approvalInstanceId,
        approvalCardId,
        chipsInstanceId,
        chipsCardId,
        planInstanceId,
        planCardId,
        taskId,
      };
    } finally {
      await seed.context.close();
    }

    const state = showcaseState!;
    expect(state).toBeTruthy();
    for (const route of ROUTES) {
      const { context, page: home } = await openRoute(browser, baseURL, route);
      try {
        await home.goto(route.path);
        await waitQueue(home);
        // §5.1: ① and the queue header read the same N.
        await expect
          .poll(async () => {
            const header = (await home.getByTestId("home-queue-title").innerText()).match(/需要你 · (\d+)/)?.[1];
            const glance = (await home.getByTestId("glance-need-you").innerText()).match(/(\d+) 需要你/)?.[1];
            return header !== undefined && header === glance && Number(header) >= 5;
          })
          .toBe(true);
        await showQueue(home);

        // The fold: 3 cards at 390, 4 on desktop, with the rest named.
        const fold = route.phone ? 3 : 4;
        await expect(home.getByTestId("home-queue-card")).toHaveCount(fold);
        await expect(home.getByTestId("home-queue-more")).toContainText(`还有 ${5 - fold} 条`);
        await expect(home.getByTestId("home-queue-inbox")).toHaveText("去收件箱 ›");
        for (const mode of ["light", "dark"] as const) {
          await setMode(home, mode);
          await shot(home, "queue-full-folded", route, mode);
        }

        // Expanded in place: every card, asserted by kind and affordance.
        await expandQueue(home);
        await expect(home.getByTestId("home-queue-card")).toHaveCount(5);
        const approval = queueCard(home, state.approvalCardId);
        await expect(approval).toHaveAttribute("data-home-kind", "approval");
        await expect(approval.getByRole("button")).toHaveText(["拒绝", "允许一次"]);
        await expect(approval).toContainText("cargo test --workspace --locked");
        const plan = queueCard(home, state.planCardId);
        await expect(plan).toHaveAttribute("data-home-kind", "plan");
        await expect(plan.getByTestId("home-card-review")).toHaveText("审阅计划");
        await expect(plan.getByRole("button")).toHaveCount(0);
        const chips = queueCard(home, state.chipsCardId);
        await expect(chips).toHaveAttribute("data-home-kind", "simple-question");
        await expect(chips.getByTestId("home-card-chip")).toHaveText(["删除", "保留", "改名"]);
        await expect(chips.getByTestId("home-card-other")).toHaveText("其他…");
        // The Main's question is one of the five, not a sixth count.
        const mainQuestion = queueCard(home, state.mainQuestionId);
        await expect(mainQuestion).toHaveAttribute("data-home-kind", "question");
        await expect(mainQuestion.getByTestId("home-card-answer")).toHaveText("去回答");
        const task = queueCard(home, state.taskId);
        await expect(task).toHaveAttribute("data-card-type", "task");
        await expect(task).toContainText("任务待你");
        await expect(task.getByTestId("home-card-task")).toHaveText("打开任务");
        await showQueue(home);
        for (const mode of ["light", "dark"] as const) {
          await setMode(home, mode);
          await shot(home, "queue-full-expanded", route, mode);
        }
        await collapseQueue(home);
      } finally {
        await context.close();
      }
    }
  });
});

test.describe("searching", () => {
  test("typing in the ledger search hides ④ and expands every fold", async ({ browser }) => {
    test.setTimeout(180_000);
    const baseURL = test.info().project.use.baseURL!;
    // An IDLE row that the busy group would fold away; searching must show it
    // (ui-screenshots: 「folds expanded」) while ④ is hidden.
    const setup = await openRoute(browser, baseURL, ROUTES[0]);
    try {
      const page = setup.page;
      await page.goto("/m");
      await createSession(page, "hshowcase-idle 支付文档复核", "支付文档复核");
    } finally {
      await setup.context.close();
    }

    await captureState(browser, baseURL, "searching", async (home, route) => {
      await waitQueue(home);
      if (route.phone) {
        await home.getByTestId("home-search").fill("支付");
        await expect(home.getByTestId("home-queue")).toHaveCount(0);
        await expect(home.getByTestId("home-row").filter({ hasText: "审查支付迁移" })).toBeVisible({
          timeout: 20_000,
        });
        // Folded rows are visible and neither fold toggle is offered.
        await expect(home.getByTestId("home-row").filter({ hasText: "支付文档复核" })).toBeVisible({
          timeout: 20_000,
        });
        await expect(home.getByTestId("home-idle-fold")).toHaveCount(0);
        await expect(home.getByTestId("home-quiet-fold")).toHaveCount(0);
      } else {
        await expect(home.getByTestId("session-search")).toBeVisible({ timeout: 20_000 });
        await home.getByTestId("session-search").fill("支付");
        await expect(home.getByTestId("home-queue")).toHaveCount(0);
        await expect(home.locator('[data-testid="board-card"]').filter({ hasText: "审查支付迁移" })).toBeVisible({
          timeout: 20_000,
        });
        await expect(home.locator('[data-testid="board-card"]').filter({ hasText: "支付文档复核" })).toBeVisible({
          timeout: 20_000,
        });
      }
    });

    // This is the last state: release the queue fixture. The working session
    // and its greeting stay for a clean teardown.
    const cleanupRun = await openRoute(browser, baseURL, ROUTES[0]);
    try {
      await cleanupRun.page.goto("/m");
      await waitQueue(cleanupRun.page);
      await releaseQueue(cleanupRun.page, showcaseState!);
    } finally {
      await cleanupRun.context.close();
    }
  });
});
