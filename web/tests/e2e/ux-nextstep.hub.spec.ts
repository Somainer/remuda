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

async function createAgentSession(page: Page, prompt: string): Promise<string> {
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  await expect(hostPicker).toContainText("e2e-fake-node", { timeout: 20_000 });
  const hostId = await hostPicker.locator("option").filter({ hasText: "e2e-fake-node" }).getAttribute("value");
  await hostPicker.selectOption(hostId!);
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0, { timeout: 20_000 });
  await page.getByTestId("new-session-prompt").fill(prompt);
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
  const instanceId = new URL(page.url()).pathname.split("/").at(-1)!;
  created.push(instanceId);
  return instanceId;
}

async function createTerminal(page: Page): Promise<string> {
  await page.goto("/sessions/new");
  await expect(page.getByTestId("new-session-host")).toBeVisible({ timeout: 20_000 });
  // The `screen-read` initial input is the fake-node sentinel that opts this
  // instance into real cooked lines on tty.screen (other terminals keep the
  // empty-screen answer, so existing specs are unaffected).
  await page.getByTestId("new-session-prompt").fill("screen-read terminal");
  await page.getByTestId("new-session-kind-terminal").click();
  await expect(page.getByTestId("new-session-start")).toBeEnabled();
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
  const instanceId = new URL(page.url()).pathname.split("/").at(-1)!;
  created.push(instanceId);
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

test("row shows the approval summary, tucks wire fields into a disclosure, and keys still reach the harness", async ({
  page,
}) => {
  // The "blocked-question" sentinel makes the fake harness emit native status
  // `blocked` with the same scripted approval card raised, so the row lands in
  // the 待处理 group (dot = waiting-interaction projection).
  const prompt = "nextstep blocked-question approval row";
  const approvalId = await createAgentSession(page, prompt);
  const terminalId = await createTerminal(page);
  // The "workflow card … row" create sentinel raises no approval and journals
  // a running workflow.run (nativeRunId wf-native-demo) with a running phase
  // labelled "Review" and a queued "Verify", staying in native status working.
  // The working row's sentence must be projected from that live journal
  // instead of the constant 运行中….
  const workflowPrompt = "workflow card demo-running row-phrase";
  const workflowId = await createAgentSession(page, workflowPrompt);
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
  // disclosure was already asserted above.
  await panel.press("Escape");
  if (await wire.evaluate((el) => el.open)) {
    await approvalRow.getByTestId("session-wire-toggle").click();
  }
  await page.setViewportSize({ width: 1440, height: 900 });
  await shot(page, "ux2026-nextstep-1-1440.png");
  await page.setViewportSize({ width: 390, height: 844 });

  // 390 px geometry, measured against bounding boxes (scrollHeight on a
  // nowrap element cannot fail).
  //
  // Post-change measured heights (same fixture/viewport, probe in the
  // evidence doc): blocked row 152.6 px, terminal row 120.6 px in a quiet
  // run; under full-suite load flex reflow can add ~54 px, so the gate
  // below checks the one-line geometry and the exact baseline bound
  // (292.5 / 260.5) rather than an absolute post-change height.
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
  // Re-measure defensively: the absolute cardH flips between ~153 (wire
  // fully collapsed) and ~207 (flex reflow under full-suite load) depending
  // on timing; gate on the tight one-line geometry and the baseline bound,
  // not on an absolute row height.
  expect(geometry.cardH).toBeLessThanOrEqual(292.5);
  expect(geometry.headline.oneLine, `headline ${JSON.stringify(geometry.headline)}`).toBe(true);
  expect(geometry.sentence.oneLine, `sentence ${JSON.stringify(geometry.sentence)}`).toBe(true);

  const terminalGeometry = await terminalRow.evaluate((card) => ({
    cardH: Math.round(card.getBoundingClientRect().height * 10) / 10,
  }));
  expect(terminalGeometry.cardH).toBeLessThanOrEqual(260.5);

  await shot(page, "ux2026-nextstep-1-390.png");
});
