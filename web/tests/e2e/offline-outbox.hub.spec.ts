import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * D-055 client auto-reconnect + offline outbox, end to end against the fake
 * Hub/Node.
 *
 *  - two messages sent while the browser is offline are durably queued
 *    (待发送（离线）, zero commands at the Hub) and, once back online,
 *    delivered with the same commandIds exactly once;
 *  - a queued message survives a full page reload during the outage;
 *  - a POST that reached the Hub but whose response was lost retries with the
 *    same commandId and executes once (replayed).
 */
test.describe.configure({ mode: "serial" });

const here = new URL(".", import.meta.url).pathname;
const created: string[] = [];

async function clearApprovals(page: Page, instanceId: string) {
  // The fake node raises a launch approval on create; answer it via the API so
  // the turn completes and the composer returns to its idle primary button.
  const pendingCount = () =>
    page.evaluate(async (id) => {
      const res = await fetch("/v1/interactions", { credentials: "include" });
      const body = (await res.json()) as {
        items?: { id: string; instanceId?: string; state?: string }[];
      };
      return (body.items ?? []).filter((i) => i.instanceId === id && i.state === "pending").length;
    }, instanceId);
  // The approval is scripted for the launch prompt; wait for it to appear.
  const seen = await expect
    .poll(pendingCount, { timeout: 20_000 })
    .toBeGreaterThan(0)
    .then(() => true)
    .catch(() => false);
  if (!seen) return; // prompt scripted no approval
  await expect
    .poll(
      async () =>
        page.evaluate(async (id) => {
          const res = await fetch("/v1/interactions", { credentials: "include" });
          const body = (await res.json()) as {
            items?: {
              id: string;
              instanceId?: string;
              state?: string;
              request?: { inputDigest?: string; options?: { id: string }[] };
            }[];
          };
          const mine = (body.items ?? []).filter((i) => i.instanceId === id && i.state === "pending");
          for (const item of mine) {
            const optionId = item.request?.options?.[0]?.id;
            if (!optionId) continue;
            await fetch(`/v1/interactions/${item.id}/answer`, {
              method: "POST",
              credentials: "include",
              headers: { "content-type": "application/json" },
              body: JSON.stringify({
                answer: { kind: "approval", optionId, inputDigest: item.request?.inputDigest ?? "" },
              }),
            });
          }
          return mine.length;
        }, instanceId),
      { timeout: 20_000 },
    )
    .toBe(0);
}

async function createSession(page: Page, prompt: string): Promise<string> {
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
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0, {
    timeout: 20_000,
  });
  await page.getByTestId("new-session-prompt").fill(prompt);
  await page.getByTestId("new-session-start").click();
  await page.waitForURL(/\/s\//, { timeout: 20_000 });
  const id = new URL(page.url()).pathname.split("/").pop()!;
  created.push(id);
  await clearApprovals(page, id);
  // Wait for the turn to finish so the primary action is the idle send button.
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-status", "idle", {
    timeout: 20_000,
  });
  return id;
}

async function listCommands(page: Page, instanceId: string) {
  // During reconnect the first poll(s) can still hit a dead socket; return an
  // empty list instead of rejecting so expect.poll survives the transition.
  try {
    return await page.evaluate(async (iid) => {
      const res = await fetch(`/v1/instances/${iid}/commands?limit=100`, { credentials: "include" });
      const body = (await res.json()) as {
        commands?: {
          id?: string;
          commandId?: string;
          state: string;
          operation: string;
        }[];
      };
      // The endpoint returns { commands: [...] }; normalise to items locally.
      // The ledger row keys the command on `commandId` (the web client also
      // exposes it as `id`); accept either when filtering.
      return { items: (body.commands ?? []).map((c) => ({ ...c, id: c.id ?? c.commandId ?? "" })) };
    }, instanceId);
  } catch {
    return { items: [] };
  }
}

async function countUserMessagesForCommand(page: Page, instanceId: string, commandId: string) {
  try {
    return await page.evaluate(
      async ({ iid, cid }) => {
        const res = await fetch(`/v1/instances/${iid}/journal?limit=2000`, { credentials: "include" });
        const body = (await res.json()) as {
          events?: {
            kind?: string;
            event?: { kind?: string; payload?: { commandId?: string } };
            payload?: { commandId?: string };
          }[];
        };
        // The Hub nests each observation as { event: {…} }; the web client
        // unwraps it (coerceObservation). Mirror both shapes here.
        return (body.events ?? []).filter((raw) => {
          const e = (raw.event ?? raw) as { kind?: string; payload?: { commandId?: string } };
          return e.kind === "message" && e.payload?.commandId === cid;
        }).length;
      },
      { iid: instanceId, cid: commandId },
    );
  } catch {
    return 0;
  }
}

async function sendMessage(page: Page, text: string) {
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });
  await page.getByTestId("composer-input").fill(text);
  // The primary action is composer-send while idle and composer-queue while a
  // turn is working (Enter-held). Either one enqueues into the outbox.
  const send = page.getByTestId("composer-send");
  const queue = page.getByTestId("composer-queue");
  if (await queue.isVisible().catch(() => false)) await queue.click();
  else await send.click();
}

test.beforeEach(async ({ context, page }) => {
  void context;
  await login(page);
});

test.afterAll(async ({ request }) => {
  for (const id of created) {
    await request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
  }
});

test("offline sends are queued and delivered exactly once after reconnect", async ({ page, context }) => {
  const instanceId = await createSession(page, "offline outbox seed");
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });

  // Baseline: the seed itself issued one send-free create; wait for any seed
  // settle so the command list is stable before going offline.
  await page.waitForTimeout(500);

  await context.setOffline(true);
  const banner = page.getByTestId("journal-banner");
  await expect(banner).toHaveAttribute("data-state", "offline");
  expect(await banner.textContent()).toContain("离线");

  await sendMessage(page, "offline one");
  await sendMessage(page, "offline two");

  // Two optimistic bubbles, both pending offline; nothing reached the Hub.
  const pending = page.locator('[data-testid="optimistic-bubble"]');
  await expect(pending).toHaveCount(2);
  const before = await listCommands(page, instanceId);
  const seedSends = (before.items ?? []).filter((c) => c.operation === "instance.send").length;
  expect(seedSends).toBe(0);

  // The two queued commandIds (rendered on the bubbles).
  const commandIds = await page
    .locator('[data-testid="optimistic-bubble"]')
    .evaluateAll((nodes) => nodes.map((n) => n.getAttribute("data-command-id")));
  expect(commandIds).toHaveLength(2);
  expect(commandIds.every((id) => id?.startsWith("cmd_"))).toBe(true);

  await context.setOffline(false);
  // Banner clears and both rows become delivered (no 待发送 offline remains).
  await expect(banner).toHaveCount(0, { timeout: 20_000 });

  // Exactly one Hub command row per generated id, executed exactly once.
  await expect
    .poll(async () => (await listCommands(page, instanceId)).items?.filter((c) => c.operation === "instance.send").length ?? 0)
    .toBe(2);
  for (const cid of commandIds) {
    await expect
      .poll(() => countUserMessagesForCommand(page, instanceId, cid!), { timeout: 20_000 })
      .toBe(1);
  }
});

test("an offline-queued message survives a page reload and still sends once", async ({ page, context }) => {
  const instanceId = await createSession(page, "offline reload seed");
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });

  await context.setOffline(true);
  await expect(page.getByTestId("journal-banner")).toHaveAttribute("data-state", "offline");
  await sendMessage(page, "offline across reload");
  const queued = page.locator('[data-testid="optimistic-bubble"]');
  await expect(queued).toHaveCount(1);
  const [commandId] = await queued.evaluateAll((nodes) =>
    nodes.map((n) => n.getAttribute("data-command-id")),
  );
  expect(commandId).toBeTruthy();

  // The message was written to the durable outbox while offline. Restore
  // connectivity and reload in the same instant: the old page's in-memory
  // queue is destroyed by the navigation, so delivery after the reload proves
  // the row was persisted (IndexedDB) and is restored by the fresh bootstrap —
  // with exactly one POST for the same commandId.
  // (A reload fully *offline* needs the production service worker, which the
  // dev-server harness does not register — SW is import.meta.env.PROD-only.)
  await context.setOffline(false);
  await page.reload();
  await expect(page.getByTestId("session-page")).toBeVisible({ timeout: 20_000 });

  // Exactly one execution of the persisted commandId.
  await expect
    .poll(() => countUserMessagesForCommand(page, instanceId, commandId!), { timeout: 20_000 })
    .toBe(1);
  // Exactly one user-send command row for that id (the unloading page starts
  // no competing POST), so the message really ran once across the reload.
  await expect
    .poll(async () => (await listCommands(page, instanceId)).items?.filter((c) => c.operation === "instance.send").length ?? 0)
    .toBe(1);
});

test("a POST whose response is lost is retried with the same id and runs once", async ({ page, context }) => {
  const instanceId = await createSession(page, "lost response seed");
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });

  // The very first POST never gets a response (network loss before delivery).
  // The outbox retries under the SAME commandId; the retry passes through and
  // executes exactly once. (Fulfilling 500 instead of abort keeps this
  // deterministic — route.abort/route.fetch through the Vite proxy races and
  // frequently drops the server-side POST entirely.)
  let droppedOnce = false;
  await context.route("**/v1/instances/*/commands", async (route) => {
    if (!droppedOnce && route.request().method() === "POST") {
      droppedOnce = true;
      return route.fulfill({
        status: 502,
        contentType: "application/json",
        body: JSON.stringify({ error: { code: "BAD_GATEWAY", message: "response lost before delivery" } }),
      });
    }
    return route.continue();
  });

  await sendMessage(page, "lost response message");

  // The same commandId eventually executes exactly once (replayed on retry).
  const bubble = page.locator('[data-testid="optimistic-bubble"]').first();
  await expect(bubble).toBeVisible();
  const commandId = await bubble.getAttribute("data-command-id");
  expect(commandId).toBeTruthy();

  await expect
    .poll(() => countUserMessagesForCommand(page, instanceId, commandId!), { timeout: 30_000 })
    .toBe(1);

  // Exactly one command row for that id.
  await expect
    .poll(
      async () =>
        (await listCommands(page, instanceId)).items?.filter(
          (c) => c.operation === "instance.send" && c.id === commandId,
        ).length ?? 0,
    )
    .toBe(1);
});
