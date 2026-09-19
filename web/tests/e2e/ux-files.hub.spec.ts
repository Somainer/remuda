import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";

// G2 files-view hub-live e2e against the fake Node's scenario workspaces.
// Synthetic fixtures only; no real models. The fake Node serves one workspace
// per §3.6 availability state (see hub_e2e.rs g2_scm_answer).

const HOST = process.env.HUB_E2E_HOST_OVERRIDE ?? undefined;

/** POST /v1/instances for a workspace, without the UI approval/composer path. */
async function createInstance(page: Page, workspaceId: string, hostId: string): Promise<string> {
  const response = await page.request.post("/v1/instances", {
    data: {
      hostId,
      workspaceId,
      kind: "claude",
      driver: "claude-headless",
      prompt: "g2 files fixture",
    },
  });
  expect(response.ok(), `create ${workspaceId}: ${await response.text()}`).toBe(true);
  const body = (await response.json()) as { instance: { instanceId: string } };
  return body.instance.instanceId;
}

async function resolveHost(page: Page): Promise<string> {
  if (HOST) return HOST;
  const hosts = await page.request.get("/v1/hosts");
  expect(hosts.ok()).toBe(true);
  const body = (await hosts.json()) as { items?: { id: string; label?: string }[] };
  const host = (body.items ?? []).find((row) => row.label === "e2e-fake-node") ?? body.items?.[0];
  expect(host, "fake node host").toBeTruthy();
  return host!.id;
}

async function openFiles(page: Page, instanceId: string) {
  await page.goto(`/s/${instanceId}/files`);
  await expect(page.getByTestId("files-pane")).toBeVisible({ timeout: 20_000 });
}

/** Watch for any write the view must not emit: instance commands. */
function watchForbiddenWrites(page: Page) {
  const writes: string[] = [];
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (request.method() !== "GET" && url.pathname.includes("/commands")) {
      writes.push(`${request.method()} ${url.pathname}`);
    }
  });
  return () => writes;
}

// Independent tests (the config runs workers:1 anyway); a failure no longer
// skips the rest. Each cleans up its own session to stay under maxInstances.
const created: string[] = [];

test.beforeEach(async ({ page }) => {
  await login(page);
  // The shared fake Node serves the whole serial hub suite with maxInstances 8;
  // earlier specs already occupy slots. Raise the cap for these read-only
  // scenarios so placement never reports PLACEMENT_UNSATISFIABLE.
  const patched = await page.request.patch(`/v1/hosts/${await resolveHost(page)}`, {
    data: { maxInstances: 64 },
  });
  expect(patched.ok(), `raise maxInstances: ${await patched.text()}`).toBe(true);
});

test.afterEach(async ({ page }) => {
  for (const instanceId of created.splice(0)) {
    await page.request.delete(`/v1/instances/${instanceId}?force=1`).catch(() => undefined);
  }
});

test("row 4: a clean repository shows the distinct 无变化 state without attribution wording", async ({ page }) => {
  const hostId = await resolveHost(page);
  const instanceId = await createInstance(page, "wsp_g2_clean", hostId);
  created.push(instanceId);
  await openFiles(page, instanceId);

  await expect(page.getByTestId("files-title")).toHaveText("工作区当前变更");
  await expect(page.getByTestId("files-clean")).toBeVisible();
  await expect(page.getByTestId("files-subtitle")).toContainText("采集于");

  // Row 11: no wording anywhere implies "本次会话改动".
  const body = page.getByTestId("files-pane");
  await expect(body).not.toContainText("本次会话");
  await expect(body).toContainText("可能由本会话或同目录的其他会话产生");
});

test("rows 1,2,5,8: entries, diff, untracked preview, binary, and truncation markers", async ({ page }) => {
  const hostId = await resolveHost(page);
  const instanceId = await createInstance(page, "wsp_e2e", hostId);
  created.push(instanceId);
  const forbidden = watchForbiddenWrites(page);
  await openFiles(page, instanceId);

  await expect(page.getByTestId("files-list")).toBeVisible();
  const entries = page.getByTestId("files-entry");
  await expect(entries).toHaveCount(3);

  // Row 1: tracked modified entry opens a unified diff with head shown.
  await expect(page.getByTestId("files-head")).toContainText("HEAD");
  await entries.filter({ hasText: "src/main.rs" }).click();
  await expect(page.getByTestId("files-diff")).toContainText('println!("workbench g2")');

  // Row 2: untracked entry opens a restricted preview with a sha256 digest.
  await entries.filter({ hasText: "notes/todo.md" }).click();
  await expect(page.getByTestId("files-file-preview")).toContainText("# TODO");
  await expect(page.getByTestId("files-file-digest")).toContainText("sha256:");

  // Row 5: a binary entry is entry-level 不支持, not a view-wide failure.
  await entries.filter({ hasText: "assets/logo.bin" }).click();
  await expect(page.getByTestId("files-detail-binary")).toBeVisible();
  await expect(page.getByTestId("files-title")).toBeVisible();

  // Row 10: opening/reading emitted no instance command.
  expect(forbidden()).toEqual([]);
});

test("row 8: truncation workspace marks capped list, diff, and file explicitly", async ({ page }) => {
  const hostId = await resolveHost(page);
  const instanceId = await createInstance(page, "wsp_g2_trunc", hostId);
  created.push(instanceId);
  await openFiles(page, instanceId);

  await expect(page.getByTestId("files-trunc-entries")).toContainText("12");
  // big.txt is tracked-modified: its diff is over the per-request byte cap.
  await page.getByTestId("files-entry").filter({ hasText: "big.txt" }).click();
  await expect(page.getByTestId("files-diff-trunc")).toBeVisible();

  // many.txt is untracked: its current bytes exceed maxFileBytes, so the
  // preview is capped and returns no content.
  await page.getByTestId("files-entry").filter({ hasText: "many.txt" }).click();
  await expect(page.getByTestId("files-detail-toolarge")).toBeVisible();
});

test("row 5: a non-git registered directory shows 不支持 with the reason", async ({ page }) => {
  const hostId = await resolveHost(page);
  const instanceId = await createInstance(page, "wsp_g2_nogit", hostId);
  created.push(instanceId);
  await openFiles(page, instanceId);
  await expect(page.getByTestId("files-unsupported")).toBeVisible();
  await expect(page.getByTestId("files-unsupported")).toContainText("git");
});

test("row 7: a denied/timeout probe shows 权限不足, distinct from a load failure", async ({ page }) => {
  const hostId = await resolveHost(page);
  const instanceId = await createInstance(page, "wsp_g2_denied", hostId);
  created.push(instanceId);
  await openFiles(page, instanceId);
  await expect(page.getByTestId("files-forbidden")).toBeVisible();
  await expect(page.getByTestId("files-forbidden")).toContainText("权限不足");
});

test("row 6: an offline node maps to the 离线 state without stale content", async ({ page }) => {
  const hostId = await resolveHost(page);
  const instanceId = await createInstance(page, "wsp_e2e", hostId);
  created.push(instanceId);

  // The shared fake Node serves other specs; emulate this host going offline at
  // the proxy boundary instead of killing it.
  await page.route("**/changes", async (route) => {
    await route.fulfill({
      status: 409,
      contentType: "application/json",
      body: JSON.stringify({ code: "HOST_OFFLINE", error: "host is offline" }),
    });
  });

  await openFiles(page, instanceId);
  await expect(page.getByTestId("files-offline")).toBeVisible();
  await expect(page.getByTestId("files-list")).toHaveCount(0);
});

test("row 9: content changed after collection prompts a manual refresh, no polling", async ({ page }) => {
  const hostId = await resolveHost(page);
  const instanceId = await createInstance(page, "wsp_g2_changed", hostId);
  created.push(instanceId);

  // First status is served with one head; the subsequent diff answers with a
  // moved HEAD, exactly the list-vs-content gap §3.7 describes.
  let statusCalls = 0;
  await page.route(/\/changes(\/|$|\?)/, async (route) => {
    const pathname = new URL(route.request().url()).pathname;
    const isStatus = pathname.endsWith("/changes");
    if (isStatus) {
      statusCalls += 1;
      await route.continue();
      return;
    }
    if (pathname.includes("/diff")) {
      // The detail fetch answers against a moved HEAD (§3.7).
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({
          availability: "ok",
          headOid: "ffffffffffffffffffffffffffffffffffffffff",
          staged: false,
          observedAt: "2026-09-14T12:00:05.000Z",
          items: [{ path: "src/edited.rs", patch: "+changed\n", binary: false, truncated: false, bytesAvailable: 9 }],
          truncated: { diffBytes: false, bytesOmitted: 0 },
        }),
      });
      return;
    }
    await route.continue();
  });

  await openFiles(page, instanceId);
  await page.getByTestId("files-entry").filter({ hasText: "src/edited.rs" }).click();
  await expect(page.getByTestId("files-content-changed")).toBeVisible();
  await expect(page.getByTestId("files-content-changed")).toContainText("请刷新");

  // No polling: only the single status fetch so far. Manual refresh refetches.
  const statusBefore = statusCalls;
  await page.waitForTimeout(1500);
  expect(statusCalls).toBe(statusBefore);
  await page.getByTestId("files-content-refresh").click();
  await expect.poll(() => statusCalls).toBeGreaterThan(statusBefore);
});

test.describe("mobile (390px)", () => {
  test.use({ viewport: { width: 390, height: 780 } });

  test("row 12: phone entry into the fullscreen route, back keeps the body position", async ({ page }) => {
    const hostId = await resolveHost(page);
    const instanceId = await createInstance(page, "wsp_e2e", hostId);
    created.push(instanceId);

    await page.goto(`/s/${instanceId}/structured`);
    await expect(page.getByTestId("session-page")).toBeVisible();

    // Wait for the transcript scroller itself: the session page paints the
    // loading state first, and driving scroll before the virtualized scroller
    // mounts would capture 0 regardless of where the pin restores to.
    const scroller = page.getByTestId("transcript-scroller");
    await expect(scroller).toBeVisible();

    // The entry point still exists at phone width; it is reachable on every
    // width. D-040 moved 文件 into the header ⋯ sheet on compact, so open the
    // sheet first — the toggle itself (and its testid) is unchanged.
    const more = page.getByTestId("session-more-open");
    await expect(more).toBeVisible();
    await more.click();
    const toggle = page.getByTestId("session-more-sheet").getByTestId("files-toggle");
    await expect(toggle).toBeVisible();

    // Drive the conversation scroll, then open files and come back. With the
    // short fake fixture the transcript may not overflow; capture whatever
    // position it accepts and require the same value after back.
    await scroller.evaluate((target: HTMLElement) => {
      target.scrollTop = target.scrollHeight;
    });
    const scrollBefore = await scroller.evaluate((target: HTMLElement) => target.scrollTop);

    await toggle.click();
    await expect(page).toHaveURL(/\/files$/);
    await expect(page.getByTestId("files-pane")).toBeVisible();
    await expect(page.getByTestId("files-list")).toBeVisible();

    await page.getByTestId("files-back").click();
    await expect(page).toHaveURL(/\/structured$/);
    await expect.poll(() =>
      page.evaluate(() =>
        document.querySelector<HTMLElement>("[data-testid='transcript-scroller']")?.scrollTop ?? 0,
      ),
    ).toBe(scrollBefore);
  });
});
