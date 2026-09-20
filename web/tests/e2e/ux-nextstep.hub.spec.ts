import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * P0-6 (`docs/design/plans/workbench-ux.md` §B-2 c-nextstep): the session
 * list row is dot + title + one next-step sentence. The wire triple lives
 * behind a closed `session-wire` disclosure, and the per-row send/keys
 * controls sit in an overflow Sheet — every old testid kept.
 *
 * Backed by the single in-process fake node
 * (`crates/remuda-hub/examples/hub_e2e.rs`):
 *  - a `blocked-question` launch parks on the scripted `fake_approval`
 *    (description "echo e2e") with native status blocked, so the row lands
 *    in 待处理 and its sentence is the approval summary;
 *  - a `workflow card … row` launch journals a running workflow run/phase
 *    without raising an approval, so a working row's sentence is the live
 *    run phrase;
 *  - a `terminal` instance exposes a cooked TTY whose screen buffer answers
 *    `tty.screen`, and an ESC logical key (what the row sheet sends) is
 *    acknowledged on screen with QUICKFIND_ESC_RECEIVED.
 */

test.describe.configure({ mode: "serial" });

const here = path.dirname(fileURLToPath(import.meta.url));
const withEvidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = withEvidence
  ? path.join(here, "../../../docs/design/evidence")
  : path.join(here, "../../test-results/ux-nextstep");

const created: string[] = [];
let cap: { hostId: string; previous: number } | null = null;

async function shot(page: Page, name: string) {
  if (!withEvidence) return;
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

async function raiseCap(page: Page, to: number) {
  const hosts = await page.evaluate(async () => {
    const list = await fetch("/v1/hosts", { credentials: "include" });
    return (await list.json()) as { items?: { id?: string; label?: string; maxInstances?: number }[] };
  });
  const host = (hosts.items ?? []).find((h) => h.label === "e2e-fake-node");
  if (!host?.id) return null;
  const previous = host.maxInstances ?? 8;
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
  return { hostId: host.id, previous };
}

test.beforeEach(async ({ page }) => {
  await login(page);
  if (!cap) cap = await raiseCap(page, 16);
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
  const ids = created.splice(0);
  for (const id of ids) {
    await page
      .evaluate(async (value) => fetch(`/v1/instances/${value}?force=1`, { method: "DELETE", credentials: "include" }), id)
      .catch(() => {});
  }
});

/**
 * Confirm the Hub now projects the newly created instance as a connected,
 * listed row (and, for scripted agents, with the exact activity the board
 * assertions read). create() returns as soon as the Node acks the RPC and the
 * store refreshes once, but the fake node fire-and-forgets its journal frames
 * ahead of that ack; on a loaded shared node the frames that move the row out
 * of `requested` and set its activity can land a beat later. The list poll
 * then self-heals within seconds, and a board assertion fired in that window
 * raced a missing/wrong row for its whole budget. Wait on the real server
 * projection rather than a fixed delay.
 */
async function waitForProjectedInstance(
  page: Page,
  instanceId: string,
  expectedActivity?: string,
): Promise<void> {
  await expect
    .poll(
      async () => {
        const res = await page.evaluate(
          async ({ id, want }) => {
            const r = await fetch(`/v1/instances`, { credentials: "include" });
            const body = (await r.json()) as {
              items?: Array<{
                instanceId?: string;
                connectivity?: string;
                activity?: string | { state?: string; value?: string };
              }>;
            };
            const row = body.items?.find((item) => item.instanceId === id);
            if (!row || row.connectivity !== "connected") return false;
            if (!want) return true;
            const activity =
              typeof row.activity === "string" ? row.activity : (row.activity?.value ?? null);
            return activity === want;
          },
          { id: instanceId, want: expectedActivity ?? null },
        );
        return res;
      },
      { timeout: 30_000 },
    )
    .toBe(true);
}

async function createAgentSession(page: Page, prompt: string, expectedActivity: string): Promise<string> {
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  await expect(hostPicker).toContainText("e2e-fake-node", { timeout: 20_000 });
  const hostId = await hostPicker.locator("option").filter({ hasText: "e2e-fake-node" }).getAttribute("value");
  await hostPicker.selectOption(hostId!);
  await pinWorkspace(page);
  await page.getByTestId("new-session-prompt").fill(prompt);
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
  const instanceId = new URL(page.url()).pathname.split("/").at(-1)!;
  created.push(instanceId);
  await waitForProjectedInstance(page, instanceId, expectedActivity);
  return instanceId;
}

/**
 * Pin the canonical fake-node workspace for every session this spec creates.
 *
 * Without an explicit choice the helper used to accept the picker's first
 * option, which is only implicitly wsp_e2e: the host advertises eight
 * workspaces and the picker order is the server's array order re-sorted by an
 * operator recent-workspace preference once one exists. All three of this
 * spec's sessions are asserted by href on ONE Space-scoped board, so a
 * different implicit default (different space) makes every card lookup fail
 * for the whole budget. wsp_e2e is the fake node's stock workspace in
 * hub_e2e.rs; select it explicitly so the create, the board filter and the
 * href assertions always address the same Space regardless of ordering.
 */
async function pinWorkspace(page: Page): Promise<void> {
  const workspacePicker = page.getByTestId("new-session-workspace");
  await expect(workspacePicker.locator("option")).not.toHaveCount(0, { timeout: 20_000 });
  const option = workspacePicker.locator('option[value="wsp_e2e"]');
  await expect(option).toHaveCount(1);
  await workspacePicker.selectOption("wsp_e2e");
}

async function createTerminal(page: Page): Promise<string> {
  await page.goto("/sessions/new");
  await expect(page.getByTestId("new-session-host")).toBeVisible({ timeout: 20_000 });
  // The `screen-read` initial input is the fake-node sentinel that opts this
  // instance into real cooked lines on tty.screen (other terminals keep the
  // empty-screen answer, so existing specs are unaffected).
  const hostPicker = page.getByTestId("new-session-host");
  const hostId = await hostPicker.locator("option").filter({ hasText: "e2e-fake-node" }).getAttribute("value");
  await hostPicker.selectOption(hostId!);
  await pinWorkspace(page);
  await page.getByTestId("new-session-prompt").fill("screen-read terminal");
  await page.getByTestId("new-session-kind-terminal").click();
  await expect(page.getByTestId("new-session-start")).toBeEnabled();
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
  const instanceId = new URL(page.url()).pathname.split("/").at(-1)!;
  created.push(instanceId);
  await waitForProjectedInstance(page, instanceId);
  return instanceId;
}

async function pendingInteractionId(page: Page, instanceId: string): Promise<string> {
  await expect
    .poll(
      async () =>
        page.evaluate(async (id) => {
          const res = await fetch(`/v1/interactions?instanceId=${id}`, { credentials: "include" });
          const body = (await res.json()) as { items?: { id?: string; state?: string }[] };
          return body.items?.find((item) => item.state === "pending")?.id ?? null;
        }, instanceId),
      { timeout: 15_000 },
    )
    .toBeTruthy();
  const interactionId = await page.evaluate(async (id) => {
    const res = await fetch(`/v1/interactions?instanceId=${id}`, { credentials: "include" });
    const body = (await res.json()) as { items?: { id?: string; state?: string }[] };
    return body.items?.find((item) => item.state === "pending")?.id ?? "";
  }, instanceId);
  return interactionId;
}

async function screenLines(page: Page, instanceId: string): Promise<string> {
  return page.evaluate(async (id) => {
    const res = await fetch(`/v1/instances/${id}/screen?lines=80`, { credentials: "include" });
    const body = (await res.json()) as { lines?: string[] };
    return (body.lines ?? []).join("\n");
  }, instanceId);
}

/**
 * Keep the desktop SessionList mounted under a phone-width CSS viewport.
 *
 * D-049's ViewportGate (web/src/app/router.tsx) redirects /sessions to /m as
 * soon as COMPACT_WORKBENCH_QUERY starts matching, so shrinking the page to
 * 390 px unmounts every board-card: the geometry measurement used to resolve
 * a card handle around that navigation and read the now-detached headline at
 * zero height with `line-height: normal` ({"h":0,"lineH":null}).
 *
 * The geometry gate is about the FULL board row under 390 px *CSS* rules —
 * CSS media queries key off the real viewport, so they still apply — and not
 * the /m HomeList (a separate component with its own rows). Pin the app's
 * compact JavaScript reading to non-compact while the layout really runs at
 * 390 px. Per-instance patching does not work: Chromium hands each
 * matchMedia() call a fresh MediaQueryList wrapper, so intercept the
 * prototype getter for the compact query only (change events still fire; the
 * listeners read the pinned value). No sleep, no retry — the fresh browser
 * context per test restores the prototype.
 */
async function holdDesktopShellAtPhoneWidth(page: Page) {
  await page.evaluate(() => {
    const compactMedia = window.matchMedia(
      "(max-width: 767px), (pointer: coarse) and (max-width: 1023px) and (max-height: 600px)",
    ).media;
    const proto = MediaQueryList.prototype;
    if (Object.getOwnPropertyDescriptor(proto, "matches")?.get?.name === "pinnedCompact") return;
    const native = Object.getOwnPropertyDescriptor(proto, "matches")?.get;
    Object.defineProperty(proto, "matches", {
      configurable: true,
      enumerable: true,
      get: function pinnedCompact() {
        if (this.media === compactMedia) return false;
        return native ? native.call(this) : undefined;
      },
    });
  });
}

/**
 * Resolved px line-height of an attached, laid-out element; 0 while it is
 * detached or display:none (computed line-height stays `normal` there). The
 * geometry measurement waits on this real condition rather than reading a
 * node mid-navigation.
 */
async function laidOutLineHeight(scope: ReturnType<Page["locator"]>, selector: string): Promise<number> {
  return scope.evaluate((card, sel) => {
    const el = card.querySelector(sel);
    if (!(el instanceof HTMLElement) || el.offsetParent === null) return 0;
    const lineH = parseFloat(getComputedStyle(el).lineHeight);
    return Number.isFinite(lineH) && lineH > 0 ? lineH : 0;
  }, selector);
}

test("row shows the approval summary, tucks wire fields into a disclosure, and keys still reach the harness", async ({
  page,
}) => {
  // The "blocked-question" sentinel makes the fake harness emit native status
  // `blocked` with the same scripted approval card raised, so the row lands in
  // the 待处理 group (dot = waiting-interaction projection).
  const prompt = "nextstep blocked-question approval row";
  const approvalId = await createAgentSession(page, prompt, "blocked");
  const terminalId = await createTerminal(page);
  // The "workflow card … row" create sentinel raises no approval and journals
  // a running workflow.run (nativeRunId wf-native-demo) with a running phase
  // labelled "Review" and a queued "Verify", staying in native status working.
  // The working row's sentence must be projected from that live journal
  // instead of the constant 运行中….
  const workflowPrompt = "workflow card demo-running row-phrase";
  const workflowId = await createAgentSession(page, workflowPrompt, "working");
  const interactionId = await pendingInteractionId(page, approvalId);

  await page.goto("/sessions");

  const approvalRow = page.getByTestId("board-card").filter({ hasText: prompt }).first();
  await expect(approvalRow).toBeVisible();
  await expect(approvalRow).toHaveAttribute("data-status", "blocked");
  await expect(page.getByTestId("session-group-blocked")).toContainText(prompt);

  // A working row projects the live run phrase from its journal:
  // `Workflow <native run id> · phase <running phase label>`. The queued
  // "Verify" phase must not replace the active "Review".
  const workflowRow = page.getByTestId("board-card").filter({ has: page.locator(`a[href="/s/${workflowId}"]`) });
  await expect(workflowRow).toHaveAttribute("data-status", "working");
  await expect(workflowRow.getByTestId("session-next-step")).toHaveText("Workflow wf-native-demo · phase Review");

  // The one sentence is the approval description; the wire activity word is
  // not painted on the row while the disclosure is closed.
  await expect(approvalRow.getByTestId("session-next-step")).toHaveText("echo e2e");
  await expect(approvalRow.getByTestId("session-next-step")).not.toContainText("waiting-interaction");

  // Pending rows keep exactly one primary handle, straight to the approval.
  await expect(approvalRow.getByTestId("board-go-handle")).toHaveAttribute(
    "href",
    `/approvals?focus=${interactionId}`,
  );

  // Wire fields: hidden by default, one click away.
  const wire = approvalRow.getByTestId("session-wire");
  await expect(wire).not.toHaveAttribute("open", "", { timeout: 5_000 });
  await expect(approvalRow.getByTestId("session-lifecycle")).toBeHidden();
  await approvalRow.getByTestId("session-wire-toggle").click();
  await expect(approvalRow.getByTestId("session-lifecycle")).toBeVisible();
  await expect(approvalRow.getByTestId("session-lifecycle")).toContainText("connected");
  await expect(wire).toContainText(approvalId.slice(0, 8));

  // Remote controls live in the overflow sheet, with every old testid.
  const terminalRow = page.getByTestId("board-card").filter({ has: page.locator(`a[href="/s/${terminalId}"]`) });
  await expect(terminalRow.getByTestId("board-prompt")).toHaveCount(0);
  await terminalRow.getByTestId("board-more").click();
  const panel = page.getByTestId("board-actions-panel");
  await expect(panel).toBeVisible();
  for (const testid of ["board-prompt", "board-send", "board-key-enter", "board-key-esc", "board-key-ctrl-c", "board-stop"]) {
    await expect(panel.getByTestId(testid)).toBeVisible();
  }

  // An ESC sent from the sheet reaches the fake-harness process: its screen
  // answers with the marker, and the list poll surfaces it on the row.
  await panel.getByTestId("board-key-esc").click();
  await expect.poll(async () => screenLines(page, terminalId), { timeout: 15_000 }).toContain("QUICKFIND_ESC_RECEIVED");
  await expect(terminalRow.getByTestId("board-snippet")).toContainText("QUICKFIND_ESC_RECEIVED", { timeout: 15_000 });

  // Evidence shots show the default (collapsed) row shape; the expanded
  // disclosure was already asserted above. Hold the desktop list mounted
  // before shrinking: at 390 px D-049 otherwise redirects /sessions to /m
  // and unmounts these cards mid-measurement.
  await holdDesktopShellAtPhoneWidth(page);
  await panel.press("Escape");
  if (await wire.evaluate((el) => el.open)) {
    await approvalRow.getByTestId("session-wire-toggle").click();
  }
  await page.setViewportSize({ width: 1440, height: 900 });
  await shot(page, "ux2026-nextstep-1-1440.png");
  await page.setViewportSize({ width: 390, height: 844 });
  // The seam keeps the desktop route at phone width; the genuine 390 px CSS
  // media rules still drive the layout.
  await expect(page).toHaveURL(/\/sessions/);

  // 390 px geometry, measured against bounding boxes (scrollHeight on a
  // nowrap element cannot fail). Two bounds per row:
  //  - the pre-change baseline on b0dd8389 (292.5 / 260.5): the redesign
  //    must never exceed the old row;
  //  - a tight post-change bound: quiet-run maxima 152.6 (blocked) and
  //    120.6 (terminal), full-suite loaded maxima 206.6 and ~132.6, plus
  //    ~10 px tolerance → 215 / 185. A wrapping regression blows this long
  //    before it reaches the baseline.
  const BASELINE_BLOCKED_H = 292.5;
  const BASELINE_TERMINAL_H = 260.5;
  const REDESIGN_BLOCKED_H = 215;
  const REDESIGN_TERMINAL_H = 185;
  // Wait for the rows to be laid out at 390 px: attached (offsetParent
  // non-null) with a resolved px line-height. Measuring immediately after the
  // resize used to catch detached nodes; this polls a real layout condition.
  await expect
    .poll(() => laidOutLineHeight(approvalRow, "a[data-testid='session-row'] > div:first-child"), {
      timeout: 10_000,
    })
    .toBeGreaterThan(0);
  await expect
    .poll(() => laidOutLineHeight(approvalRow, "[data-testid='session-next-step']"), { timeout: 10_000 })
    .toBeGreaterThan(0);
  const geometry = await approvalRow.evaluate((card) => {
    const lineMetrics = (selector: string) => {
      const el = card.querySelector(selector);
      if (!el) return { h: 0, lineH: 0, oneLine: false };
      const h = el.getBoundingClientRect().height;
      const lineH = parseFloat(getComputedStyle(el).lineHeight);
      return { h: Math.round(h * 10) / 10, lineH, oneLine: h <= lineH + 1 };
    };
    return {
      cardH: Math.round(card.getBoundingClientRect().height * 10) / 10,
      headline: lineMetrics("a[data-testid='session-row'] > div:first-child"),
      sentence: lineMetrics("[data-testid='session-next-step']"),
    };
  });
  expect(geometry.cardH).toBeLessThanOrEqual(REDESIGN_BLOCKED_H);
  expect(geometry.cardH).toBeLessThanOrEqual(BASELINE_BLOCKED_H);
  expect(geometry.headline.oneLine, `headline ${JSON.stringify(geometry.headline)}`).toBe(true);
  expect(geometry.sentence.oneLine, `sentence ${JSON.stringify(geometry.sentence)}`).toBe(true);

  const terminalGeometry = await terminalRow.evaluate((card) => ({
    cardH: Math.round(card.getBoundingClientRect().height * 10) / 10,
  }));
  expect(terminalGeometry.cardH).toBeLessThanOrEqual(REDESIGN_TERMINAL_H);
  expect(terminalGeometry.cardH).toBeLessThanOrEqual(BASELINE_TERMINAL_H);

  await shot(page, "ux2026-nextstep-1-390.png");
});
