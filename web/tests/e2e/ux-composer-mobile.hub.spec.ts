import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

const evidenceDir = process.env.REMUDA_EVIDENCE === "1"
  ? path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence")
  : path.resolve("test-results/evidence");

async function shot(page: Page, name: string) {
  await mkdir(evidenceDir, { recursive: true });
  await page.screenshot({ path: path.join(evidenceDir, name), animations: "disabled" });
}

/**
 * c-composer / D-042 (ui-spec.md §2.2 compact composer 边界, workbench-ux
 * plan task 4): at a 390 px touch viewport the composer collapses to one
 * input + one options trigger that STILL names the permissionMode word and
 * the effort tier (danger modes stay visible while collapsed), while
 * attachments / harness / permission picker / effort slider ride in the
 * options sheet. The send/queue/interrupt three-state buttons, the queue
 * chip and the 「尚未验证」 note never enter the sheet. The two old
 * window.confirm flows (插队 / Esc 打断) are Sheet dialogs — no native
 * browser dialog is allowed to appear.
 *
 * The fake Node keeps the turn busy for the sentinel prompt
 * "hold-working run a long tool" (same fixture as ux-steer.hub.spec.ts).
 */

test.describe.configure({ mode: "serial" });

test.use({
  viewport: { width: 390, height: 844 },
  hasTouch: true,
  isMobile: true,
});

const created: string[] = [];
// The fake node advertises maxInstances: 8; leaked slots from earlier serial
// specs can otherwise 422 a create far from the spec that leaked (same
// mitigation as ux-steer.hub.spec.ts).
let cap: { hostId: string; previous: number } | null = null;

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

async function createSession(page: Page, prompt: string): Promise<string> {
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  await expect(hostPicker).toContainText("e2e-fake-node", { timeout: 20_000 });
  const hostId = await hostPicker.locator("option").filter({ hasText: "e2e-fake-node" }).getAttribute("value");
  expect(hostId).toBeTruthy();
  await hostPicker.selectOption(hostId!);
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0, {
    timeout: 20_000,
  });
  await page.getByTestId("new-session-prompt").fill(prompt);
  const creating = page.waitForResponse(
    (response) => response.request().method() === "POST" && new URL(response.url()).pathname === "/v1/instances",
  );
  await page.getByTestId("new-session-start").click();
  const res = await creating;
  expect(res.ok(), `instance create failed: ${res.status()}`).toBe(true);
  const instanceId = (await res.json()).instance.instanceId as string;
  created.push(instanceId);
  await expect(page).toHaveURL(new RegExp(`/s/${instanceId}`), { timeout: 20_000 });
  return instanceId;
}

/** Answer the create-time approval so the composer becomes usable. */
async function clearApprovals(page: Page, instanceId: string) {
  // Wait until the requested row is durable, then answer, then confirm the
  // pending list drained: the pipelined Node→Hub uplink can lag create, and
  // answering a not-yet-persisted request is resurrected by its late event.
  await page.evaluate(async (id) => {
    const listPending = async () => {
      const body = await (await fetch("/v1/interactions", { credentials: "include" })).json();
      return (body.items ?? []).filter(
        (item: { instanceId?: string; state?: string }) =>
          item.instanceId === id && item.state === "pending",
      );
    };
    const deadline = Date.now() + 10_000;
    let mine = await listPending();
    while (mine.length === 0 && Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 100));
      mine = await listPending();
    }
    for (const item of mine) {
      const optionId = item.request?.options?.[0]?.id;
      if (!optionId) continue;
      await fetch(`/v1/interactions/${item.interactionId ?? item.id}/answer`, {
        method: "POST",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          answer: { kind: "approval", optionId, inputDigest: item.request?.inputDigest ?? "" },
        }),
      });
    }
    let remaining = await listPending();
    while (remaining.length > 0 && Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 100));
      remaining = await listPending();
    }
  }, instanceId);
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 30_000 });
}

/** Wire-level POST log for one page. */
function watchCommands(page: Page) {
  const commands: Array<{ operation?: string; payload?: { mode?: string } }> = [];
  page.on("request", (request) => {
    if (request.method() !== "POST") return;
    if (!new URL(request.url()).pathname.endsWith("/commands")) return;
    const body = request.postDataJSON() as (typeof commands)[number] | null;
    if (body) commands.push(body);
  });
  return commands;
}

/** Any native confirm/alert is a D-042 violation; record it to fail on. */
function failOnNativeDialog(page: Page) {
  const seen: string[] = [];
  page.on("dialog", (dialog) => {
    seen.push(dialog.message());
    void dialog.dismiss().catch(() => {});
  });
  return () => {
    expect(seen, `no native browser dialog may appear; got: ${seen.join(" | ")}`).toEqual([]);
  };
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
    await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
  }
});

test("collapsed bar names permission + effort; options are in the sheet; three-state stays outside", async ({
  page,
}) => {
  const instanceId = await createSession(page, "composer mobile collapsed");
  await clearApprovals(page, instanceId);
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-status", "idle", { timeout: 15_000 });

  const bar = page.getByTestId("composer-bar");
  await expect(bar).toHaveAttribute("data-collapsed", "1");
  // The trigger always names the current permission word (manual = 询问).
  const trigger = page.getByTestId("model-effort-chip");
  await expect(trigger).toHaveAttribute("data-options-trigger", "1");
  await expect(trigger).toHaveAttribute("data-permission", "manual");
  await expect(trigger).toHaveAttribute("data-permission-danger", "0");
  await expect(page.getByTestId("composer-trigger-permission")).toHaveText("询问");
  await expect(page.locator("[data-testid='model-effort-chip-label']")).toBeVisible();
  // Plain phone placeholder — no desktop shortcut hints.
  await expect(page.getByTestId("composer-input")).toHaveAttribute("placeholder", "输入提示词…");
  await shot(page, "ux2026-composer-1-collapsed-390.png");

  // Option-only controls are not on the collapsed bar — including context
  // usage, which belongs in the sheet per dispatch plan §C default 4.
  await expect(page.getByTestId("attach-file")).toHaveCount(0);
  await expect(page.getByTestId("harness-chip")).toHaveCount(0);
  await expect(page.getByTestId("context-chip")).toHaveCount(0);
  await expect(page.getByTestId("permission-menu")).toHaveCount(0);
  // The primary three-state control stays outside.
  await expect(page.getByTestId("composer-send")).toBeVisible();

  // The collapsed trigger has a ≥44px touch zone (D-038 / ui-spec §3.4).
  // The visible chip is 32px tall; the centred ::after zone extends 6px
  // above and below it. Probe 20px off centre (inside the 22px half-zone,
  // outside the 16px half-chip) — a point that misses without the hot zone.
  // Box measurement and elementFromPoint run in one evaluate so the dock
  // cannot re-layout between them.
  const edgeHits = await trigger.evaluate((el) => {
    const rect = el.getBoundingClientRect();
    const cx = rect.left + rect.width / 2;
    const cy = rect.top + rect.height / 2;
    return [cy - 20, cy + 20].map((y) =>
      document.elementFromPoint(cx, y)?.closest("[data-options-trigger='1']")?.getAttribute("data-testid") ?? null,
    );
  });
  expect(edgeHits).toEqual(["model-effort-chip", "model-effort-chip"]);

  // Open the options sheet.
  await trigger.click();
  const sheet = page.getByTestId("composer-options-sheet");
  await expect(sheet).toBeVisible();
  await expect(sheet).toHaveAttribute("data-variant", "sheet");
  await expect(sheet.getByTestId("attach-file")).toBeVisible();
  await expect(sheet.getByTestId("attach-camera")).toBeVisible();
  await expect(sheet.getByTestId("attach-paste")).toBeVisible();
  await expect(sheet.getByTestId("harness-chip")).toBeVisible();
  await expect(sheet.getByTestId("context-chip")).toBeVisible();
  await expect(sheet.getByTestId("permission-option-manual")).toBeVisible();
  await expect(sheet.getByTestId("effort-slider")).toBeVisible();
  await shot(page, "ux2026-composer-1-sheet-390.png");

  // D-039: sheet tap targets (context chip + permission rows) reach 44px via
  // centred ::after zones even though the visuals are 30-31px. Probe 20px off
  // centre (inside the 22px half-zone, outside the ~15.5px half-control);
  // rect + elementFromPoint in one evaluate so the sheet cannot re-layout.
  const sheetTargets = await page.evaluate(() => {
    const probe = (sel: string) => {
      const el = document.querySelector(sel);
      if (!el) return [null, null];
      const r = el.getBoundingClientRect();
      // Sample 20px off the control centre. For full-width sheet rows whose
      // ::after zone extends left of the row, re-anchor x onto the zone:
      // pick a point inside the row's visible text column so the probe lands
      // on the row itself (and its ::after layer) rather than a neighbour.
      const x = Math.max(r.left + 8, r.left + r.width / 2);
      return [r.top + r.height / 2 - 20, r.top + r.height / 2 + 20].map((y) => {
        const t = document.elementFromPoint(x, y);
        return t?.closest("[data-testid]")?.getAttribute("data-testid") ?? null;
      });
    };
    const row = (name: string) => probe(`[data-testid='permission-option-${name}']`);
    return {
      context: probe("[data-testid='context-chip']"),
      permissionManual: row("manual"),
    };
  });
  expect(sheetTargets.context).toEqual(["context-chip", "context-chip"]);
  expect(sheetTargets.permissionManual).toEqual([
    "permission-option-manual",
    "permission-option-manual",
  ]);

  // Changing a permission and the effort slider updates the collapsed summary
  // after the sheet closes.
  await sheet.getByTestId("permission-option-acceptEdits").click();
  await expect(sheet).toHaveCount(0);
  await expect(page.getByTestId("composer-trigger-permission")).toHaveText("可改文件");
  await expect(trigger).toHaveAttribute("data-permission", "acceptEdits");

  await trigger.click();
  await expect(sheet).toBeVisible();
  // Move the effort slider one stop with the keyboard (focus, not a centre
  // click, which would itself snap the stop); the collapsed summary then
  // names the new tier (or its in-flight tag).
  const slider = sheet.getByTestId("effort-slider");
  await expect(slider).toHaveAttribute("data-index", "2");
  const startIndex = Number(await slider.getAttribute("data-index"));
  await slider.focus();
  await page.keyboard.press("ArrowRight");
  await expect(slider).toHaveAttribute("data-index", String(startIndex + 1));
  const nextTier = await slider.getAttribute("data-name");
  await page.getByTestId("composer-options-close").click();
  await expect(sheet).toHaveCount(0);
  // Poll for the FINAL tier: between the request landing and the transcript
  // read-back the chip can briefly render `?`, so a one-shot regex misses.
  await expect
    .poll(async () => page.getByTestId("model-effort-chip-label").textContent(), {
      timeout: 10_000,
    })
    .toMatch(new RegExp(nextTier));
  // Focus returns to the collapsed trigger after closing the sheet.
  await expect(trigger).toBeFocused();

  // Reopen for the D-028a assertions on the working turn below.
  await trigger.click();
  await expect(sheet).toBeVisible();
  // D-028a: three-state controls are never options.
  expect(await sheet.getByTestId("composer-send").count()).toBe(0);
  expect(await sheet.getByTestId("composer-steer").count()).toBe(0);
  expect(await sheet.getByTestId("composer-interrupt").count()).toBe(0);
  expect(await sheet.getByTestId("composer-queue-status").count()).toBe(0);
  expect(await sheet.getByTestId("composer-cap-note").count()).toBe(0);

  await page.getByTestId("composer-options-close").click();
  await expect(sheet).toHaveCount(0);

  // While a turn runs, the three-state buttons and the queue chip stay on
  // the collapsed bar (and still do not move into the sheet).
  await page.getByTestId("composer-input").fill("hold-working run a long tool");
  await page.getByTestId("composer-send").click();
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-status", "working", {
    timeout: 15_000,
  });
  await expect(page.getByTestId("composer-steer")).toBeVisible();
  await expect(page.getByTestId("composer-interrupt")).toBeVisible();
  await expect(page.getByTestId("composer-queue")).toBeVisible();
  // Mobile Enter is a newline; the visible queue button holds the message.
  await page.getByTestId("composer-input").fill("queued from phone");
  await page.getByTestId("composer-queue").click();
  await expect(page.getByTestId("composer-queued-row")).toBeVisible();
  await expect(page.getByTestId("composer-queue-status")).toContainText("1");
  await trigger.click();
  await expect(sheet).toBeVisible();
  expect(await sheet.getByTestId("composer-queued-row").count()).toBe(0);
  expect(await sheet.getByTestId("composer-queue-status").count()).toBe(0);
  // The steer/interrupt buttons remain operable behind the sheet's bar row.
  await expect(page.getByTestId("composer-steer")).toBeVisible();
});

test("插队 and Esc 打断 go through the Sheet confirm; the fake harness receives both", async ({ page }) => {
  const commands = watchCommands(page);
  const assertNoNativeDialog = failOnNativeDialog(page);
  const instanceId = await createSession(page, "composer mobile steer interrupt");
  await clearApprovals(page, instanceId);
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-status", "idle", { timeout: 15_000 });

  // Start a turn the fake harness keeps busy.
  await page.getByTestId("composer-input").fill("hold-working run a long tool");
  await page.getByTestId("composer-send").click();
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-status", "working", {
    timeout: 15_000,
  });

  // 插队: cancel first (draft kept, nothing posted), then confirm.
  await page.getByTestId("composer-input").fill("jump first");
  await page.getByTestId("composer-steer").click();
  const confirm = page.getByTestId("composer-confirm");
  await expect(confirm).toBeVisible();
  await shot(page, "ux2026-composer-1-steer-confirm-390.png");
  await expect(confirm).toHaveAttribute("data-variant", "sheet");
  await expect(page.getByTestId("composer-confirm-title")).toHaveText("插队发送");
  await page.getByTestId("composer-confirm-cancel").click();
  await expect(confirm).toHaveCount(0);
  expect(commands.some((c) => c.operation === "instance.send" && c.payload?.mode === "steer")).toBe(false);
  await expect(page.getByTestId("composer-input")).toHaveValue("jump first");

  await page.getByTestId("composer-steer").click();
  await expect(confirm).toBeVisible();
  await page.getByTestId("composer-confirm-ok").click();
  await expect
    .poll(() => commands.some((c) => c.operation === "instance.send" && c.payload?.mode === "steer"))
    .toBeTruthy();
  // The 已打断 receipt is asserted from the command log (the chip itself
  // self-clears on the 4s timer / when the turn-end flush runs, so a
  // point-in-time DOM check races the idle transition).
  const steerCmd = commands.find((c) => c.operation === "instance.send" && c.payload?.mode === "steer");
  expect(steerCmd).toBeTruthy();
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-status", "idle", {
    timeout: 20_000,
  });

  // Esc 打断: Esc inside the Sheet cancels (focus trap), then confirm ok
  // posts instance.cancel through the same command path.
  await page.getByTestId("composer-input").fill("hold-working run a long tool");
  await page.getByTestId("composer-send").click();
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-status", "working", {
    timeout: 15_000,
  });
  await page.getByTestId("composer-input").focus();
  await page.keyboard.press("Escape");
  await expect(page.getByTestId("composer-confirm-title")).toHaveText("打断当前 turn");
  // Esc while the dialog owns focus = cancel, not confirm.
  await page.keyboard.press("Escape");
  await expect(page.getByTestId("composer-confirm")).toHaveCount(0);
  expect(commands.some((c) => c.operation === "instance.cancel")).toBe(false);

  await page.keyboard.press("Escape");
  await expect(page.getByTestId("composer-confirm")).toBeVisible();
  await page.getByTestId("composer-confirm-ok").click();
  await expect
    .poll(() => commands.some((c) => c.operation === "instance.cancel"))
    .toBeTruthy();
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-status", "idle", {
    timeout: 20_000,
  });
  assertNoNativeDialog();
});

test("a bypass launch keeps the danger mode visible on the collapsed trigger", async ({ page }) => {
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  await expect(hostPicker).toContainText("e2e-fake-node", { timeout: 20_000 });
  const hostId = await hostPicker.locator("option").filter({ hasText: "e2e-fake-node" }).getAttribute("value");
  expect(hostId).toBeTruthy();
  await hostPicker.selectOption(hostId!);
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0, {
    timeout: 20_000,
  });
  await page.getByTestId("new-session-perm-bypassPermissions").click();
  await expect(page.getByTestId("new-session-yolo-hint")).toBeVisible();
  await page.getByTestId("new-session-yolo-ack").click();
  await page.getByTestId("new-session-prompt").fill("bypass composer mobile");
  const creating = page.waitForResponse(
    (response) => response.request().method() === "POST" && new URL(response.url()).pathname === "/v1/instances",
  );
  await page.getByTestId("new-session-start").click();
  const res = await creating;
  expect(res.ok()).toBe(true);
  const instanceId = (await res.json()).instance.instanceId as string;
  created.push(instanceId);
  await expect(page).toHaveURL(new RegExp(`/s/${instanceId}`), { timeout: 20_000 });
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 30_000 });

  const trigger = page.getByTestId("model-effort-chip");
  await expect(trigger).toHaveAttribute("data-permission", "bypassPermissions");
  // Danger must be readable WITHOUT opening the options sheet.
  await expect(trigger).toHaveAttribute("data-permission-danger", "1");
  await expect(page.getByTestId("composer-trigger-permission")).toHaveText("绕过全部");
  expect(await page.getByTestId("composer-options-sheet").count()).toBe(0);
  await shot(page, "ux2026-composer-1-danger-trigger-390.png");
});
