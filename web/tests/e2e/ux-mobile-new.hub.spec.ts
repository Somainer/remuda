import { type PathLike } from "node:fs";
import { mkdir, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { expect, test, type Page, type Response } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * c-mobilenew: a phone must always be able to create a session, and bulk
 * screen reads must never starve control RPCs.
 *
 * The spec builds every precondition itself against the stock fake Hub/Node —
 * no env knob, no config edit. The fake Node supports one runtime switch:
 * while the gate file below exists, it parks tty.screen (bulk-read half of the
 * Hub's per-link budget) and worktree.list (control half) for EVERY instance,
 * in the frame queue without replying; removing the file releases them.
 * Absent by default, the gate changes nothing for the rest of the suite.
 */
const gateFile: PathLike = path.join(os.tmpdir(), "remuda-e2e-rpc-gate");

const evidence = process.env.REMUDA_EVIDENCE === "1";
const evidenceDir = path.join(path.dirname(new URL(import.meta.url).pathname), "../../../docs/design/evidence");
const NAME_PREFIX = "e2e-mobile-new";

async function shot(page: Page, name: string) {
  if (!evidence) return;
  await mkdir(evidenceDir, { recursive: true });
  await page.screenshot({ path: path.join(evidenceDir, name), animations: "disabled" });
}

async function fakeHost(page: Page) {
  const hosts = (await (await page.request.get("/v1/hosts")).json()) as {
    items: { hostId: string; label: string }[];
  };
  const host = hosts.items.find((item) => item.label === "e2e-fake-node");
  expect(host).toBeTruthy();
  const snapshot = (await (
    await page.request.get(`/v1/hosts/${host!.hostId}/workspaces`)
  ).json()) as { workspaces: { workspaceId: string; root: string }[] };
  const workspace = snapshot.workspaces.find((item) => item.workspaceId === "wsp_e2e");
  expect(workspace).toBeTruthy();
  return { hostId: host!.hostId, workspaceId: workspace!.workspaceId, root: workspace!.root };
}

/** Create a claude-pty instance and stop it, leaving a real `exited` row. */
async function createStoppedInstance(
  page: Page,
  hostId: string,
  workspaceId: string,
  root: string,
  name: string,
): Promise<string> {
  const headers = { Origin: new URL(page.url()).origin };
  const create = await page.request.post("/v1/instances", {
    headers,
    data: {
      hostId,
      workspaceId,
      cwd: root,
      kind: "claude",
      driver: "claude-pty",
      name,
    },
  });
  expect(create.ok(), await create.text()).toBe(true);
  const id = ((await create.json()) as { instance: { instanceId: string } }).instance.instanceId;
  // The fake Node projects the close to a real exited lifecycle journal event.
  const close = await page.request.post(`/v1/instances/${id}/commands`, {
    headers,
    data: { operation: "instance.close", payload: {} },
  });
  expect(close.ok(), await close.text()).toBe(true);
  await expect
    .poll(
      async () => {
        const res = await page.request.get(`/v1/instances/${id}`);
        expect(res.ok()).toBeTruthy();
        return ((await res.json()) as { lifecycle: string }).lifecycle;
      },
      { timeout: 20_000 },
    )
    .toBe("exited");
  return id;
}

function newFromBottomBar(page: Page) {
  return page.locator("nav[aria-label='手机底栏'] button[aria-label='新建']").click();
}

function sessionsFromBottomBar(page: Page) {
  return page.locator("nav[aria-label='手机底栏'] a").filter({ hasText: "会话" }).click();
}

async function createSessionViaSheet(page: Page, prompt: string) {
  await expect(page.getByTestId("new-session-sheet")).toBeVisible();
  await page.getByTestId("new-session-prompt").fill(prompt);
  await expect(page.getByTestId("new-session-start")).toBeEnabled();
  const posting = page.waitForResponse(
    (response) =>
      response.request().method() === "POST" && new URL(response.url()).pathname === "/v1/instances",
  );
  await page.getByTestId("new-session-start").click();
  const response = await posting;
  expect(response.ok(), `create HTTP ${response.status()}`).toBe(true);
  const body = (await response.json()) as { instance?: { instanceId?: string } };
  const instanceId = body.instance?.instanceId;
  expect(instanceId).toBeTruthy();
  await expect(page).toHaveURL(new RegExp(`/s/${instanceId}`), { timeout: 20_000 });
  await expect(page.getByTestId("session-page")).toBeVisible();
  return instanceId!;
}

/** Per-id /screen response log, scoped to the ids this run created. */
function collectScreenResponses(page: Page, watchedIds: Set<string>) {
  const fiveHundreds: string[] = [];
  const watched = new Map<number, number>();
  const onResponse = (response: Response) => {
    const url = new URL(response.url());
    const match = url.pathname.match(/^\/v1\/instances\/([^/]+)\/screen$/);
    if (!match) return;
    if (watchedIds.has(match[1])) {
      watched.set(response.status(), (watched.get(response.status()) ?? 0) + 1);
    }
    if (response.status() === 500) fiveHundreds.push(`${response.status()} ${url.pathname}`);
  };
  page.on("response", onResponse);
  return { watched, fiveHundreds };
}

test.describe("mobile new session (390px)", () => {
  const created: string[] = [];

  test.beforeEach(async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
  });

  test.afterEach(async ({ page }) => {
    for (const id of created.splice(0)) {
      await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    }
    await rm(gateFile, { force: true }).catch(() => undefined);
  });

  test("exited rows cost no /screen 500 and two creates open their sessions", async ({ page }) => {
    await login(page);
    const { hostId, workspaceId, root } = await fakeHost(page);

    // Build the repro preconditions ourselves: 14 real exited rows on the Node.
    const exitedIds = new Set<string>();
    for (let n = 0; n < 14; n += 1) {
      exitedIds.add(
        await createStoppedInstance(page, hostId, workspaceId, root, `${NAME_PREFIX}-exited-${n}`),
      );
    }
    const collected = collectScreenResponses(page, exitedIds);

    await page.goto("/sessions");
    await expect(page.getByTestId("session-list")).toBeVisible();
    await page.getByTestId("space-chip").filter({ hasText: "remuda-e2e" }).first().click();
    // Mobile auto-opens the space's first tab; pick one of our own rows
    // explicitly instead of assuming which one.
    await expect(page).toHaveURL(/\/s\/ins_/);
    expect(exitedIds.has(new URL(page.url()).pathname.split("/s/")[1])).toBe(true);
    // Exited session pages never pull /screen for anything.
    await page.waitForTimeout(3_000);
    await sessionsFromBottomBar(page);
    await expect(page).toHaveURL(/\/sessions$/);
    // This space also holds other suites' rows; assert at least all 14 of
    // ours are rendered, scoped by the ids this test created. evaluateAll
    // serialises args as JSON, so pass an array (no Set across the boundary),
    // and poll via expect.poll: a cached one-shot promise would not requery.
    await expect
      .poll(
        () =>
          page
            .locator('[data-testid="board-card"][data-lifecycle="exited"]')
            .evaluateAll(
              (cards, ids) =>
                cards.filter((card) =>
                  ids.includes((card as HTMLElement).dataset.instanceId ?? ""),
                ).length,
              [...exitedIds],
            ),
        { timeout: 10_000 },
      )
      .toBeGreaterThanOrEqual(exitedIds.size);
    await shot(page, "mobile-new-session-1-list-390.png");

    // Several 2.5 s list poll cycles: zero /screen reads for the exited rows.
    await page.waitForTimeout(6_000);
    const screenCalls = [...collected.watched.values()].reduce((sum, n) => sum + n, 0);
    expect(screenCalls, "no bulk screen reads for exited rows").toBe(0);
    expect(collected.fiveHundreds).toEqual([]);

    await newFromBottomBar(page);
    const firstId = await createSessionViaSheet(page, "mobile new session repro one");
    created.push(firstId);
    await shot(page, "mobile-new-session-1-session-390.png");

    // The dimmed list stays mounted behind this sheet; it must not fan out.
    await newFromBottomBar(page);
    const secondId = await createSessionViaSheet(page, "mobile new session repro two");
    created.push(secondId);

    expect(collected.fiveHundreds).toEqual([]);
  });

  test("NODE_BUSY on a saturated control half rejects the real create with 503 inline", async ({ page }) => {
    test.setTimeout(120_000);
    await login(page);
    const { hostId, workspaceId, root } = await fakeHost(page);
    const exitedIds = new Set<string>();
    for (let n = 0; n < 14; n += 1) {
      exitedIds.add(
        await createStoppedInstance(page, hostId, workspaceId, root, `${NAME_PREFIX}-busy-${n}`),
      );
    }

    // Fill the link: 16 parked bulk reads (screen) plus 16 parked control
    // calls (worktree.list). The gate parks frames in the fake Node without
    // replying, so all 32 Hub pending slots stay occupied.
    await mkdir(path.dirname(gateFile.toString()), { recursive: true });
    await writeFile(gateFile, "block");
    const parked: Promise<unknown>[] = [];
    const exited = [...exitedIds];
    for (let n = 0; n < 20; n += 1) {
      parked.push(page.request.get(`/v1/instances/${exited[n % exited.length]}/screen?lines=80`));
      parked.push(page.request.get(`/v1/worktrees?hostId=${hostId}`));
    }
    // Let 32 calls occupy slots; the excess 8 settle immediately as 503.
    await page.waitForTimeout(1_000);

    await page.goto("/sessions/new");
    await expect(page.getByTestId("new-session-sheet")).toBeVisible();
    await page.getByTestId("new-session-prompt").fill("mobile new session busy");
    const posting = page.waitForResponse(
      (response) =>
        response.request().method() === "POST" && new URL(response.url()).pathname === "/v1/instances",
    );
    await page.getByTestId("new-session-start").click();
    const response = await posting;
    expect(response.status()).toBe(503);
    const body = (await response.json()) as { code?: string; retryAfterMs?: number };
    expect(body.code).toBe("NODE_BUSY");
    expect(body.retryAfterMs).toBeGreaterThan(0);

    const errorBox = page.getByTestId("new-session-error");
    await expect(errorBox).toBeVisible();
    await expect(errorBox).toContainText("NODE_BUSY");
    await expect(page).toHaveURL(/\/sessions\/new/);
    await expect(page.getByTestId("new-session-prompt")).toHaveValue("mobile new session busy");
    await expect(page.getByTestId("new-session-start")).toBeEnabled();
    await shot(page, "mobile-new-session-1-node-busy-390.png");

    // Release parked calls so the node link drains.
    await rm(gateFile, { force: true });
    await Promise.allSettled(parked);
  });

  test("saturated screen reads still let instance.create through", async ({ page }) => {
    test.setTimeout(120_000);
    await login(page);
    const { hostId, workspaceId, root } = await fakeHost(page);
    const exitedIds = [] as string[];
    for (let n = 0; n < 14; n += 1) {
      exitedIds.push(
        await createStoppedInstance(page, hostId, workspaceId, root, `${NAME_PREFIX}-sat-${n}`),
      );
    }
    const watched = new Set(exitedIds);
    const collected = collectScreenResponses(page, watched);

    await mkdir(path.dirname(gateFile.toString()), { recursive: true });
    await writeFile(gateFile, "block");
    // 40 reads over the 14 rows: 16 park (bulk sub-budget), the rest get 503
    // NODE_BUSY; none may be 500.
    const reads = Array.from({ length: 40 }, (_, n) =>
      page.request.get(`/v1/instances/${exitedIds[n % exitedIds.length]}/screen?lines=80`),
    );
    await page.waitForTimeout(1_000);

    await page.goto("/sessions");
    await expect(page.getByTestId("session-list")).toBeVisible();
    await newFromBottomBar(page);
    // Control half is reserved: despite 16 parked reads the create succeeds.
    const instanceId = await createSessionViaSheet(page, "mobile new session under saturation");
    created.push(instanceId);

    await rm(gateFile, { force: true });
    const settled = await Promise.allSettled(reads);
    const statuses = settled.map((entry) =>
      entry.status === "fulfilled" ? entry.value.status() : -1,
    );
    expect(statuses.filter((status) => status === 503).length, "reads get NODE_BUSY/503").toBeGreaterThan(0);
    expect(statuses.filter((status) => status === 500)).toHaveLength(0);
    expect(collected.fiveHundreds).toEqual([]);
  });
});
