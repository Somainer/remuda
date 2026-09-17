import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * c-steer: 插队 / 排队 / AskUserQuestion-pending composer behaviour against the
 * in-process fake node (`crates/remuda-hub/examples/hub_e2e.rs`):
 *
 * 1. while a turn is working, Enter holds TWO messages client-side (pending
 *    transcript rows tagged `排队中 · 第 n 条 · 回车后送出`, per-row cancel,
 *    zero POSTs); one is cancelled; the 插队 button (and ⌘/Ctrl+Enter) POSTs
 *    `mode:steer`, the turn is interrupted (`已打断`), and when the idle
 *    evidence lands the surviving held row is posted AFTER the steer — order
 *    preserved, no message stuck in 排队;
 * 2. a pending question/approval is `blocked`, NOT working: the composer stays
 *    enabled, offers no 插队 (Esc must not reach a dialog), Enter holds with
 *    `待回答后送出`, and answering the interaction flushes the message;
 * 3. ⌘/Ctrl+Enter is the 插队 keyboard gesture.
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
  await page.evaluate(async (id) => {
    for (;;) {
      const list = await fetch("/v1/interactions", { credentials: "include" });
      const body = (await list.json()) as {
        items?: {
          id: string;
          instanceId?: string;
          state?: string;
          request?: { inputDigest?: string; options?: { id: string }[] };
        }[];
      };
      const mine = (body.items ?? []).filter((item) => item.instanceId === id && item.state === "pending");
      if (mine.length === 0) return;
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
      await new Promise((resolve) => setTimeout(resolve, 300));
    }
  }, instanceId);
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

test("queue two, cancel one, then 插队 jumps ahead of the remaining row", async ({ page }) => {
  const commands = watchCommands(page);
  page.on("framenavigated", (f) => console.log("NAV:", f.url()));
  page.on("pageerror", (e) => console.log("PAGEERROR:", e.message, (e as Error).stack ?? ""));
  page.on("console", (m) => { if (m.type() === "error") console.log("CONSOLE-ERR:", m.text()); });
  const instanceId = await createSession(page, "steer ordering session");
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
  // Wait for the POST flight to settle: while `sending` the dock is disabled
  // and an Enter pressed too early would be swallowed by canSubmit().
  await expect(page.getByTestId("composer-interrupt")).toBeEnabled({ timeout: 10_000 });

  const input = page.getByTestId("composer-input");
  // Enter while working holds locally; nothing is POSTed.
  await input.fill("queued alpha");
  await input.press("Enter");
  await expect(page.locator('[data-testid="optimistic-bubble"][data-held="turn"]')).toHaveCount(1);
  await expect(page.getByTestId("held-queue-tag").first()).toContainText("第 1 条");

  await input.fill("queued beta");
  await input.press("Enter");
  await expect(page.locator('[data-testid="optimistic-bubble"][data-held="turn"]')).toHaveCount(2);
  await expect(page.getByTestId("held-queue-tag").nth(1)).toContainText("第 2 条");
  expect(commands.filter((c) => c.operation === "instance.send")).toHaveLength(0);

  // Per-message cancel: alpha is dropped; beta keeps its place (now ordinal 1).
  await page
    .locator('[data-testid="optimistic-bubble"][data-held="turn"]', { hasText: "queued alpha" })
    .getByTestId("held-queue-cancel")
    .click();
  await expect(page.locator('[data-testid="optimistic-bubble"][data-held="turn"]')).toHaveCount(1);
  await expect(page.getByTestId("held-queue-tag")).toContainText("第 1 条");
  await expect(
    page.locator('[data-testid="optimistic-bubble"][data-held="turn"]', { hasText: "queued beta" }),
  ).toBeVisible();
  expect(commands.filter((c) => c.operation === "instance.send")).toHaveLength(0);

  // 插队 the third message: visible button + confirm.
  await input.fill("jump first");
  page.once("dialog", (dialog) => {
    expect(dialog.message()).toContain("插队");
    void dialog.accept();
  });
  await page.getByTestId("composer-steer").click();

  // Steer POST lands first, carrying mode=steer; the interrupted-turn badge
  // shows; the idle evidence then flushes the surviving held row AFTER it.
  await expect
    .poll(() => commands.some((c) => c.operation === "instance.send" && c.payload?.mode === "steer"))
    .toBeTruthy();
  await expect(page.getByTestId("composer-interrupted-chip")).toBeVisible();
  await expect
    .poll(() => commands.filter((c) => c.operation === "instance.send").length)
    .toBeGreaterThanOrEqual(3);
  const sends = commands.filter((c) => c.operation === "instance.send");
  expect(sends[0]?.payload).toMatchObject({ prompt: "hold-working run a long tool" });
  expect(sends[1]?.payload).toMatchObject({ prompt: "jump first", mode: "steer" });
  expect(sends.at(-1)?.payload?.prompt).toBe("queued beta");
  expect(sends.at(-1)?.payload?.mode).toBeFalsy();
  // Delivered rows lose the held tag; nothing remains stuck 排队.
  await expect(page.locator('[data-testid="optimistic-bubble"][data-held]')).toHaveCount(0);
});

test("a pending question blocks, never disables the composer, and the held row flushes after the answer", async ({
  page,
}) => {
  const commands = watchCommands(page);
  const instanceId = await createSession(page, "blocked-question please wait for me");
  // Do NOT answer the approval: the instance is parked on a human interaction.
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-status", "blocked", {
    timeout: 15_000,
  });

  const input = page.getByTestId("composer-input");
  await expect(input).toBeEnabled();
  // 插队 must not be offered at a dialog (Esc would hit the question).
  await expect(page.getByTestId("composer-steer")).toHaveCount(0);

  // Enter holds with「待回答后送出」; no command is posted.
  await input.fill("after you answer");
  await input.press("Enter");
  const held = page.locator('[data-testid="optimistic-bubble"][data-held="answer"]');
  await expect(held).toHaveCount(1);
  await expect(page.getByTestId("held-queue-tag")).toContainText("待回答后送出");
  expect(commands).toHaveLength(0);

  // Answering the pending interaction resolves the block and flushes the row.
  await answerPending(page, instanceId);
  await expect
    .poll(() => commands.some((c) => c.operation === "instance.send"))
    .toBeTruthy();
  const send = commands.find((c) => c.operation === "instance.send");
  expect(send?.payload?.prompt).toBe("after you answer");
  expect(send?.payload?.mode).toBeFalsy();
  await expect(page.locator('[data-testid="optimistic-bubble"][data-held]')).toHaveCount(0);
});

test("Cmd/Ctrl+Enter is the 插队 gesture", async ({ page }) => {
  const commands = watchCommands(page);
  const instanceId = await createSession(page, "steer keyboard session");
  await answerPending(page, instanceId);
  await page.getByTestId("composer-input").fill("hold-working long tool again");
  await page.getByTestId("composer-send").click();
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-status", "working", {
    timeout: 15_000,
  });
  await expect(page.getByTestId("composer-interrupt")).toBeEnabled({ timeout: 10_000 });

  const input = page.getByTestId("composer-input");
  await input.fill("keyboard jump");
  page.once("dialog", (dialog) => {
    expect(dialog.message()).toContain("插队");
    void dialog.accept();
  });
  await input.press("Control+Enter");
  await expect
    .poll(() => commands.some((c) => c.operation === "instance.send" && c.payload?.mode === "steer"))
    .toBeTruthy();
  const send = commands.find((c) => c.payload?.prompt === "keyboard jump");
  expect(send?.payload?.mode).toBe("steer");
});
