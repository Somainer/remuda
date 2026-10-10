import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * c-dirpicker: add a directory through the Node-backed folder browser and
 * remove a registered directory, against the composed Hub + gated fake Node
 * (HUB_E2E_DIR_PICKER=1 in crates/remuda-hub/examples/hub_e2e.rs).
 *
 * Coverage:
 * 1. add-via-browse: 「+ 添加目录」 opens a directories-only browser rooted at
 *    the Node allowlist; hidden folders are off by default; navigating and
 *    「使用此文件夹」 registers the folder as a workspace through the
 *    prepare/commit workspaces route.
 * 2. remove: confirmation first, unbind never deletes files; a live session
 *    makes the Hub refuse with the reason shown; after the session ends (its
 *    history kept) the same remove succeeds and the row disappears.
 *
 * Self-skips without the trigger, deletes the instances it created, uses only
 * relative URLs (no hardcoded ports), and screenshots only behind
 * REMUDA_EVIDENCE.
 */

test.describe.configure({ mode: "serial" });
test.skip(
  process.env.HUB_E2E_DIR_PICKER !== "1",
  "set HUB_E2E_DIR_PICKER=1 for the c-dirpicker harness",
);

/// The fixture root AS THE NODE REPORTS IT, read exactly ONCE per run from a
/// real `host.dirs.list` response (round 6 item 6; r9 item 4) and reused by
/// every call site. Never recompute it with a platform guess like
/// `/private/tmp`, and never refetch: browsing, the register fallback and
/// the cleanup DELETEs all use these same bytes.
let cachedFixtureRoot: string | null = null;

async function fixtureRoot(page: Page, host: string): Promise<string> {
  if (cachedFixtureRoot !== null) return cachedFixtureRoot;
  const listing = await apiJson<{ path: string }>(page, "GET", `/v1/hosts/${host}/dirs`);
  if (!listing.path) throw new Error("fixture host.dirs.list returned no path");
  cachedFixtureRoot = listing.path;
  return listing.path;
}

const createdInstances: string[] = [];

async function apiJson<T>(page: Page, method: string, path: string, body?: unknown): Promise<T> {
  const response = await page.request.fetch(path, {
    method,
    data: body,
    headers: body ? { "content-type": "application/json" } : undefined,
  });
  const text = await response.text();
  if (!response.ok()) {
    throw Object.assign(new Error(`${method} ${path} → ${response.status()}: ${text}`), {
      status: response.status(),
      body: text,
    });
  }
  return (text ? JSON.parse(text) : null) as T;
}

async function apiStatus(
  page: Page,
  method: string,
  path: string,
  body?: unknown,
): Promise<{ status: number; body: string }> {
  const response = await page.request.fetch(path, {
    method,
    data: body,
    headers: body ? { "content-type": "application/json" } : undefined,
  });
  return { status: response.status(), body: await response.text() };
}

type WorkspaceRow = { workspaceId: string; hostId: string; root: string };

async function fakeHost(page: Page): Promise<string> {
  const hosts = await apiJson<{ items?: { id?: string; label?: string }[] }>(
    page,
    "GET",
    "/v1/hosts",
  );
  const host = (hosts.items ?? []).find((item) => item.label === "e2e-fake-node");
  if (!host?.id) throw new Error("e2e-fake-node not found");
  return host.id;
}

async function listWorkspaces(page: Page, host: string): Promise<WorkspaceRow[]> {
  const view = await apiJson<{ workspaces?: WorkspaceRow[] }>(
    page,
    "GET",
    `/v1/hosts/${host}/workspaces`,
  );
  return view.workspaces ?? [];
}

async function registerByPath(page: Page, host: string, path: string): Promise<WorkspaceRow[]> {
  const view = await apiJson<{ workspaces: WorkspaceRow[] }>(
    page,
    "POST",
    `/v1/hosts/${host}/workspaces`,
    { path },
  );
  return view.workspaces;
}

async function evidenceShot(page: Page, name: string) {
  if (process.env.REMUDA_EVIDENCE === "1") {
    await test.info().attach(name, {
      body: await page.screenshot({ fullPage: true }),
      contentType: "image/png",
    });
  }
}

test.beforeEach(async ({ page }) => {
  await login(page);
});

test.afterAll(async ({ browser }) => {
  const context = await browser.newContext();
  const page = await context.newPage();
  try {
    await login(page);
    for (const id of createdInstances) {
      await page.request.fetch(`/v1/instances/${id}?force=1`, { method: "DELETE" }).catch(() => undefined);
    }
    const host = await fakeHost(page).catch(() => null);
    if (host) {
      // Read the fixture root ONCE from the Node and reuse those exact bytes
      // for every cleanup DELETE (round 6 item 6).
      const root = await fixtureRoot(page, host).catch(() => null);
      if (root) {
        for (const path of [`${root}/alpha`, `${root}/beta`]) {
          await page.request
            .fetch(`/v1/hosts/${host}/workspaces`, {
              method: "DELETE",
              headers: { "content-type": "application/json" },
              data: { path },
            })
            .catch(() => undefined);
        }
      }
    }
  } finally {
    await context.close();
  }
});

test("adds a directory by browsing the host filesystem", async ({ page }) => {
  const host = await fakeHost(page);
  await page.goto("/sessions/new");

  // Wait for the data plane to see the fake host before driving the SPA
  // select, whose list also hides stale-offline rows during startup.
  await expect
    .poll(
      async () =>
        (
          await apiJson<{ items?: { label?: string; state?: string; online?: boolean }[] }>(
            page,
            "GET",
            "/v1/hosts",
          )
        ).items?.some((item) => item.label === "e2e-fake-node" && item.online),
      { timeout: 30_000 },
    )
    .toBe(true);

  // Choose the fake host explicitly (the list may contain other rows).
  const hostSelect = page.getByTestId("new-session-host");
  await expect
    .poll(
      async () =>
        (await hostSelect.locator("option").allInnerTexts()).some((text) =>
          text.includes("e2e-fake-node"),
        ),
      { timeout: 20_000 },
    )
    .toBe(true);
  const labels = await hostSelect.locator("option").allInnerTexts();
  const label = labels.find((text) => text.includes("e2e-fake-node"));
  expect(label, labels.join(" | ")).toBeTruthy();
  await hostSelect.selectOption({ label: label! });

  await page.getByTestId("workspace-add").click();
  const browser = page.getByTestId("dir-browser");
  await expect(browser).toBeVisible();
  await expect(browser.getByTestId("dir-browser-row")).toHaveCount(2);
  // Hidden dot-directories are off by default.
  await expect(browser.getByText(".hidden")).toHaveCount(0);

  // Filtering narrows the current folder client-side.
  await browser.getByTestId("dir-browser-filter").fill("bet");
  await expect(browser.getByTestId("dir-browser-row")).toHaveCount(1);
  await expect(browser.getByText("beta")).toBeVisible();
  await browser.getByTestId("dir-browser-filter").clear();

  // Showing hidden folders re-requests the folder from the Node.
  await browser.getByTestId("dir-browser-hidden").check();
  await expect(browser.getByText(".hidden")).toBeVisible();

  // Navigate into alpha and register exactly that folder.
  await browser.getByText("alpha").click();
  await expect(browser.getByText("nested")).toBeVisible();
  await evidenceShot(page, "dirpicker-browser");
  await browser.getByTestId("dir-browser-use").click();
  await expect(browser).toBeHidden();

  const root = await fixtureRoot(page, host);
  const workspaces = await listWorkspaces(page, host);
  // Match the EXACT root+name bytes the Node reported (r8 item 5: the root is
  // now a per-run directory, so a /alpha suffix match would be ambiguous).
  const alpha = workspaces.find((row) => row.root === `${root}/alpha`);
  expect(alpha, JSON.stringify(workspaces)).toBeTruthy();

  // The new workspace is selected in the picker.
  await expect(page.getByTestId("new-session-workspace")).toHaveValue(alpha!.workspaceId);
});

test("removes a directory after confirmation and refuses it while a session is live", async ({ page }) => {
  const host = await fakeHost(page);
  // The one fixture root, read from the Node's own listing (round 6 item 6).
  const root = await fixtureRoot(page, host);
  let workspaces = await listWorkspaces(page, host);
  let beta = workspaces.find((row) => row.root === `${root}/beta`);
  if (!beta) {
    // Register using the exact bytes the Node just reported; the returned row
    // stays authoritative for every later lookup.
    workspaces = await registerByPath(page, host, `${root}/beta`);
    beta = workspaces.find((row) => row.root === `${root}/beta`);
  }
  expect(beta).toBeTruthy();
  // Every later lookup (row filter, aria name, DELETE body) uses the root the
  // fixture actually reported — round 4 item 8.
  const betaRoot = beta!.root;

  // Start a live session in the directory.
  const created = await apiJson<{ instance: { instanceId?: string; id?: string } }>(
    page,
    "POST",
    "/v1/instances",
    {
      hostId: host,
      workspaceId: beta!.workspaceId,
      kind: "claude",
      driver: "claude-pty",
      prompt: "dirpicker busy directory",
    },
  );
  const instanceId = created.instance.instanceId ?? created.instance.id;
  expect(instanceId).toBeTruthy();
  createdInstances.push(instanceId!);
  await expect
    .poll(
      async () =>
        (await apiJson<{ lifecycle?: string }>(page, "GET", `/v1/instances/${instanceId}`))
          .lifecycle,
      { timeout: 20_000 },
    )
    .not.toBe("exited");

  page.on("dialog", (dialog) => void dialog.accept());
  await page.goto("/hosts");
  const hostWorkspaces = page.getByTestId("host-workspaces");
  const betaRow = hostWorkspaces
    .getByTestId("host-workspace")
    .filter({ hasText: betaRoot });
  await expect(betaRow).toBeVisible();

  // First removal: the Hub refuses with the reason, shown inline; files stay.
  await betaRow.getByRole("button", { name: `移除目录 ${betaRoot}` }).click();
  await expect(hostWorkspaces.getByRole("alert")).toContainText("live session", { timeout: 20_000 });
  const blocked = await apiStatus(
    page,
    "DELETE",
    `/v1/hosts/${host}/workspaces`,
    { path: betaRoot },
  );
  expect(blocked.status).toBe(409);
  expect(blocked.body).toContain("live session");
  await expect(betaRow).toBeVisible();

  // End the session; its history row remains.
  const close = await page.request.post(`/v1/instances/${instanceId}/commands`, {
    data: { operation: "instance.close", payload: {} },
  });
  expect(close.ok(), await close.text()).toBe(true);
  await expect
    .poll(
      async () =>
        (await apiJson<{ lifecycle?: string }>(page, "GET", `/v1/instances/${instanceId}`))
          .lifecycle,
      { timeout: 20_000 },
    )
    .toBe("exited");
  const history = await apiStatus(page, "GET", `/v1/instances/${instanceId}`);
  expect(history.status).toBe(200);

  // Second removal settles and the row disappears (files are never deleted).
  await betaRow.getByRole("button", { name: `移除目录 ${betaRoot}` }).click();
  await expect(betaRow).toBeHidden({ timeout: 20_000 });
  workspaces = await listWorkspaces(page, host);
  expect(workspaces.some((row) => row.root === betaRoot)).toBe(false);
});
