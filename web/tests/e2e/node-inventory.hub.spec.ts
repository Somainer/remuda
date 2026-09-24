import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * A Node restart must settle the rows it lost, and the fleet view must show it.
 *
 * The 2026-09-18 demo: the ssh-stdio Node on one host restarted, and because
 * its hello carried no instance inventory the Hub logged "node epoch changed
 * but hello carried no instance inventory; leaving instance rows untouched".
 * Four instances whose processes died with the old Node stayed `running` /
 * `working`: they counted against `maxInstances` (placement was unsatisfiable
 * until the cap was raised by hand) and `remuda watch` kept calling them
 * working.
 *
 * This spec drives the fake node's own restart (`TTYNODE_RESTART`, see
 * `crates/remuda-hub/examples/hub_e2e.rs`): it reconnects under a new epoch
 * whose inventory no longer holds the session the operator was looking at —
 * the hello a real Node now sends after its sweep, and the diff that settles
 * the row. The assertions are the ones the demo failed: the row settles with
 * its reason, and its placement slot comes back to the fleet view.
 *
 * Fake Node only — the in-process harness in
 * crates/remuda-hub/examples/hub_e2e.rs.
 */

test.describe.configure({ mode: "serial" });

test.skip(process.env.HUB_E2E_EXTERNAL === "1", "Needs the in-process fake Node");

const created: string[] = [];

type HostRef = { hostId: string; label: string; maxInstances?: number };

/** The fake node advertises `maxInstances: 8`, shared by every serial spec. */
async function hostRef(page: Page): Promise<HostRef> {
  const hosts = (await (await page.request.get("/v1/hosts")).json()) as { items: HostRef[] };
  const host = hosts.items.find((item) => item.label === "e2e-fake-node");
  expect(host, "the fake node must be enrolled").toBeTruthy();
  return host!;
}

async function patchMaxInstances(page: Page, value: number): Promise<void> {
  const host = await hostRef(page);
  const response = await page.request.patch(`/v1/hosts/${host.hostId}`, {
    data: { maxInstances: value },
  });
  expect(response.ok(), await response.text()).toBe(true);
}

/** Delete every live instance so a later spec is not starved of slots. */
async function forceDeleteAllInstances(page: Page): Promise<void> {
  const list = (await (await page.request.get("/v1/instances")).json()) as {
    items?: { instanceId?: string; lifecycle?: string }[];
  };
  for (const instance of list.items ?? []) {
    if (!instance.instanceId) continue;
    if (["exited", "failed", "closed"].includes(instance.lifecycle ?? "")) continue;
    await page.request.delete(`/v1/instances/${instance.instanceId}?force=1`);
  }
}

/**
 * Create a `claude-pty` session on the fake node.
 *
 * That driver reaches `running` and therefore holds a real placement slot —
 * which is what the demo's cap exhaustion was made of. A `terminal` kind does
 * not: the harness never journals a lifecycle for it, so the row stays
 * `requested` and occupies nothing.
 */
async function createClaudePty(page: Page): Promise<string> {
  const host = await hostRef(page);
  const workspaces = (await (
    await page.request.get(`/v1/hosts/${host.hostId}/workspaces`)
  ).json()) as { workspaces: { workspaceId: string; root: string }[] };
  const workspace = workspaces.workspaces[0];
  expect(workspace).toBeTruthy();

  const create = await page.request.post("/v1/instances", {
    headers: { Origin: new URL(page.url()).origin },
    data: {
      hostId: host.hostId,
      workspaceId: workspace.workspaceId,
      cwd: workspace.root,
      kind: "claude",
      driver: "claude-pty",
      model: "e2e/auto",
      name: "e2e-node-inventory",
    },
  });
  expect(create.ok(), await create.text()).toBe(true);
  const instanceId = (await create.json()).instance.instanceId as string;
  created.push(instanceId);
  await expect
    .poll(async () => (await instanceRow(page, instanceId)).lifecycle, { timeout: 20_000 })
    .toBe("running");
  return instanceId;
}

/** Read one instance row through the Hub API. */
async function instanceRow(page: Page, instanceId: string) {
  const response = await page.request.get(`/v1/instances/${instanceId}`);
  expect(response.ok(), await response.text()).toBe(true);
  return (await response.json()) as { lifecycle?: string; lastError?: string };
}

/** The host's fleet view: capacity and free slots. */
async function hostCap(page: Page) {
  const host = await hostRef(page);
  const response = await page.request.get(`/v1/hosts/${host.hostId}/hostcap`);
  expect(response.ok(), await response.text()).toBe(true);
  return (await response.json()) as { running?: number; freeSlots?: number };
}

/** Type a line into the attached terminal and submit it. */
async function typeIntoTerminal(page: Page, line: string): Promise<void> {
  await page
    .locator(".xterm-helper-textarea")
    .first()
    .evaluate((el) => (el as HTMLTextAreaElement).focus());
  await page.keyboard.type(line);
  await page.keyboard.press("Enter");
}

/** Attach the session's tty projection and wait for it to report live. */
async function attachTerminal(page: Page, instanceId: string): Promise<void> {
  await page.goto(`/s/${instanceId}/tty`);
  await expect(page.locator("[data-tty-lab='1']")).toHaveAttribute("data-tty-status", "live", {
    timeout: 30_000,
  });
}

test.beforeAll(async ({ browser }) => {
  const page = await browser.newPage();
  await login(page, "e2e-node-inventory");
  await patchMaxInstances(page, 16);
  await page.close();
});

test.afterAll(async ({ browser }) => {
  const page = await browser.newPage();
  await login(page, "e2e-node-inventory");
  await forceDeleteAllInstances(page);
  await patchMaxInstances(page, 8);
  await page.close();
});

test.afterEach(async ({ page }) => {
  await forceDeleteAllInstances(page).catch(() => {});
  created.length = 0;
});

test("a Node restart settles the instance it lost and frees its slot", async ({ page }) => {
  test.slow();
  await login(page, "e2e-node-inventory");

  const kept = await createClaudePty(page);
  const lost = await createClaudePty(page);

  // Both rows are running and both hold a placement slot.
  const before = await hostCap(page);
  expect(before.running).toBeGreaterThanOrEqual(2);
  const freeBefore = before.freeSlots ?? 0;

  // Restart the fake Node from the lost session's terminal. Its reconnect
  // announces a new epoch whose inventory no longer holds this session — the
  // process serving it is gone, which is what the Hub must believe.
  await attachTerminal(page, lost);
  await typeIntoTerminal(page, "TTYNODE_RESTART");

  // The row settles with the settled vocabulary, not a bare "gone": the web
  // renders this exact string as 「Node 重启，会话已中断」 plus Resume.
  await expect
    .poll(async () => (await instanceRow(page, lost)).lifecycle, { timeout: 30_000 })
    .toBe("exited");
  expect((await instanceRow(page, lost)).lastError).toBe("node-epoch-changed");

  // The slot came back — the demo's sessions were unplaceable until the cap
  // was raised by hand.
  await expect
    .poll(async () => (await hostCap(page)).freeSlots ?? 0, { timeout: 20_000 })
    .toBeGreaterThan(freeBefore);

  // The restart announced the surviving session, so only the lost one
  // settled: a restart is not a licence to wipe another session's row.
  expect((await instanceRow(page, kept)).lifecycle).toBe("running");

  // And the fleet view says so: the settled session reads exited through the
  // header label the demo showed as 运行中, with the restart banner the web
  // renders from `lastError: node-epoch-changed`.
  await page.goto(`/s/${lost}`);
  await expect(page.getByTestId("session-status-label")).toHaveText("已退出", { timeout: 20_000 });
  await expect(page.getByTestId("node-restart-banner")).toContainText("Node 重启，会话已中断", {
    timeout: 20_000,
  });
});
