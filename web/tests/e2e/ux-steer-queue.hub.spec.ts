import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * c-steer 插队发送: one click on an already-queued composer row interrupts the
 * running turn and sends THAT row now, ahead of the rest of the held queue.
 * Same in-process fake node as ux-steer.hub.spec.ts.
 *
 * Queue two rows with Enter while working (zero POSTs), click 插队发送 on the
 * second row: exactly one POST with mode=steer carrying that row's text, the
 * 已打断 chip, the row gone from the queue, and the surviving row flushing
 * afterwards as an ordinary new turn with no mode — order preserved.
 *
 * Synthetic fixture only; the interrupt-then-deliver node ordering itself is
 * covered by Rust unit tests in `remuda-node::runtime::pty_queue::tests`.
 */
test.describe.configure({ mode: "serial" });

const created: string[] = [];
let cap: { hostId: string; previous: number } | null = null;

type Posted = { operation?: string; payload?: { mode?: string; prompt?: string } };

async function raiseCap(page: Page, to: number) {
  const hosts = await page.evaluate(async () => {
    const list = await fetch("/v1/hosts", { credentials: "include" });
    return (await list.json()) as { items?: { id?: string; label?: string; maxInstances?: number }[] };
  });
  const host = (hosts.items ?? []).find((h) => h.label === "e2e-fake-node");
  if (!host?.id) return;
  cap = { hostId: host.id, previous: host.maxInstances ?? 8 };
  await page.evaluate(
    async ({ id, value }) => {
      await fetch(`/v1/hosts/${id}`, {
        method: "PATCH",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ maxInstances: value }),
      });
    },
    { id: host.id, value: to },
  );
}

test.beforeEach(async ({ page }) => {
  await login(page);
  if (!cap) await raiseCap(page, 24);
});

test.afterAll(async ({ browser }) => {
  if (!cap) return;
  const page = await browser.newPage();
  try {
    await login(page);
    await page.evaluate(
      async ({ id, value }) => {
        await fetch(`/v1/hosts/${id}`, {
          method: "PATCH",
          credentials: "include",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ maxInstances: value }),
        });
      },
      { id: cap.hostId, value: cap.previous },
    );
  } finally {
    await page.close();
  }
});

test.afterEach(async ({ page }) => {
  for (const id of created.splice(0)) {
    await page
      .evaluate(
        async (value) => fetch(`/v1/instances/${value}?force=1`, { method: "DELETE", credentials: "include" }),
        id,
      )
      .catch(() => undefined);
  }
});

async function createSession(page: Page, prompt: string): Promise<string> {
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  await expect(hostPicker).toContainText("e2e-fake-node", { timeout: 20_000 });
  const hostId = await hostPicker.locator("option").filter({ hasText: "e2e-fake-node" }).getAttribute("value");
  expect(hostId).toBeTruthy();
  await hostPicker.selectOption(hostId!);
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0, { timeout: 20_000 });
  await page.getByTestId("new-session-workspace").selectOption("wsp_e2e");
  await page.getByTestId("new-session-prompt").fill(prompt);
  const creating = page.waitForResponse((r) => r.request().method() === "POST" && r.url().includes("/v1/instances"));
  await page.getByTestId("new-session-start").click();
  await creating;
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
  const id = new URL(page.url()).pathname.split("/").pop() as string;
  created.push(id);
  return id;
}

async function answerPending(page: Page, instanceId: string) {
  // Wait for the launch approval to APPEAR first: an immediate poll can read
  // 0 pending while the approval is still being journaled, which used to make
  // this helper return with the launch still blocked (gate test timeout).
  const pending = () =>
    page.evaluate(
      async (id) => {
        const list = await fetch("/v1/interactions", { credentials: "include" });
        const body = (await list.json()) as {
          items?: {
            id: string;
            interactionId?: string;
            instanceId?: string;
            state?: string;
            request?: { inputDigest?: string; options?: { id: string }[] };
          }[];
        };
        return (body.items ?? []).filter((item) => item.instanceId === id && item.state === "pending");
      },
      instanceId,
    );
  const mine = await expect
    .poll(() => pending().then((items) => items.length), {
      timeout: 20_000,
      message: "launch approval appears",
    })
    .toBeGreaterThan(0)
    .then(() => pending());
  for (const item of mine) {
    const optionId = item.request?.options?.[0]?.id;
    if (!optionId) continue;
    await page.evaluate(
      ({ iid, optionId, digest }) =>
        fetch(`/v1/interactions/${iid}/answer`, {
          method: "POST",
          credentials: "include",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({
            answer: { kind: "approval", optionId, inputDigest: digest ?? "" },
          }),
        }),
      { iid: item.interactionId ?? item.id, optionId, digest: item.request?.inputDigest },
    );
  }
  await expect
    .poll(() => pending().then((items) => items.length), {
      timeout: 20_000,
      message: "approvals clear",
    })
    .toBe(0);
}

/** Wire-level POST log for one page. */
function watchCommands(page: Page): Posted[] {
  const commands: Posted[] = [];
  page.on("request", (request) => {
    if (request.method() !== "POST") return;
    if (!new URL(request.url()).pathname.endsWith("/commands")) return;
    const body = request.postDataJSON() as Posted | null;
    if (body) commands.push(body);
  });
  return commands;
}

test("插队发送 on a queued row jumps it ahead; the surviving row flushes after", async ({ page }) => {
  const commands = watchCommands(page);
  const instanceId = await createSession(page, "steer-queue ordering session");
  await answerPending(page, instanceId);
  // Answering the create approval ends the fake turn (idle). Send the
  // hold-working sentinel to start a turn the agent stays busy in.
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-status", "idle", {
    timeout: 15_000,
  });
  await page.getByTestId("composer-input").fill("hold-working run a long tool");
  await page.getByTestId("composer-send").click();
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-status", "working", {
    timeout: 15_000,
  });
  // Baseline: the hold-working setup POST is real; held rows must add none.
  const baselineSends = commands.filter((c) => c.operation === "instance.send").length;
  const sendsSince = () => commands.filter((c) => c.operation === "instance.send").length - baselineSends;
  const input = page.getByTestId("composer-input");

  // Queue TWO rows with Enter while working; nothing is POSTed.
  await input.fill("queued alpha");
  await expect(page.getByTestId("composer-queue")).toBeEnabled({ timeout: 10_000 });
  await input.press("Enter");
  await expect(page.locator('[data-testid="optimistic-bubble"][data-held="turn"]')).toHaveCount(1);
  await input.fill("queued beta");
  await input.press("Enter");
  await expect(page.locator('[data-testid="optimistic-bubble"][data-held="turn"]')).toHaveCount(2);
  expect(sendsSince()).toBe(0);

  // 插队发送 on the SECOND queued row (no confirm dialog): it interrupts and
  // jumps ahead. Use the composer chip-row button for the beta row.
  await page
    .getByTestId("composer-queued-chip")
    .filter({ hasText: "queued beta" })
    .getByTestId("composer-queued-steer")
    .click();

  // Exactly one steer POST lands, carrying beta's text with mode=steer; the
  // interrupted-turn badge shows.
  await expect
    .poll(() => commands.filter((c) => c.operation === "instance.send" && c.payload?.mode === "steer").length)
    .toBe(1);
  await expect(page.getByTestId("composer-interrupted-chip")).toBeVisible();
  const steer = commands.find((c) => c.operation === "instance.send" && c.payload?.mode === "steer");
  expect(steer?.payload?.prompt).toBe("queued beta");
  // Beta leaves the queue immediately.
  await expect(
    page.locator('[data-testid="optimistic-bubble"][data-held="turn"]', { hasText: "queued beta" }),
  ).toHaveCount(0);

  // The idle evidence then flushes the surviving alpha row AFTER the steer, as
  // an ordinary new turn with no mode — order preserved.
  await expect.poll(() => sendsSince()).toBeGreaterThanOrEqual(2);
  const sends = commands.filter((c) => c.operation === "instance.send");
  expect(sends[0]?.payload).toMatchObject({ prompt: "hold-working run a long tool" });
  expect(sends[1]?.payload).toMatchObject({ prompt: "queued beta", mode: "steer" });
  expect(sends.at(-1)?.payload?.prompt).toBe("queued alpha");
  expect(sends.at(-1)?.payload?.mode).toBeFalsy();
  // Nothing remains stuck 排队.
  await expect(page.locator('[data-testid="optimistic-bubble"][data-held]')).toHaveCount(0);
});
