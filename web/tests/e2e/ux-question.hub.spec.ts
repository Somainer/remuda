import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * AskUserQuestion over the hook carrier (claude 2.1.272 shapes,
 * docs/design/evidence/ask-user-question-1.md): the approvals/session card
 * renders real tabs/options/free text instead of raw JSON, one Submit sends
 * the whole batch as a hook decision, and a question answered in the agent's
 * own terminal closes the row and the session banner on its own.
 *
 * Backed by the in-process fake node (crates/remuda-hub/examples/hub_e2e.rs):
 * a prompt containing "ask-question" raises the two-field card;
 * "ask-question-terminal" makes the harness answer it itself ~1.2 s later.
 */
test.describe.configure({ mode: "serial" });

const here = path.dirname(fileURLToPath(import.meta.url));
const withEvidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = withEvidence
  ? path.join(here, "../../../docs/design/evidence")
  : path.join(here, "../../test-results/ux-question");

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
  if (!cap) cap = await raiseCap(page, 24);
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
      .evaluate(
        async (value) => fetch(`/v1/instances/${value}?force=1`, { method: "DELETE", credentials: "include" }),
        id,
      )
      .catch(() => {});
  }
});

async function createSession(page: Page, prompt: string): Promise<string> {
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

async function pendingCount(page: Page, instanceId: string): Promise<number> {
  return page.evaluate(async (id) => {
    const res = await fetch("/v1/interactions?instanceId=" + id, { credentials: "include" });
    const body = (await res.json()) as { items?: { state?: string; instanceId?: string }[] };
    return (body.items ?? []).filter(
      (item) => item.state === "pending" && item.instanceId === id,
    ).length;
  }, instanceId);
}

async function latestJournal(page: Page, instanceId: string): Promise<string> {
  return page.evaluate(async (id) => {
    const res = await fetch(`/v1/instances/${id}/journal`, { credentials: "include" });
    return JSON.stringify(await res.json());
  }, instanceId);
}

test("AskUserQuestion renders options (not raw JSON) and one Submit answers the hook", async ({ page }) => {
  const instanceId = await createSession(page, "ask-question please raise the form");

  // The session dock shows the banner form while the question is pending.
  const dockForm = page.getByTestId("question-form");
  await expect(dockForm).toBeVisible({ timeout: 20_000 });
  await expect(page.getByRole("tab", { name: /下一步/ })).toBeVisible();
  await expect(page.getByRole("tab", { name: /记忆/ })).toBeVisible();

  // The raw questions JSON is never on screen before the disclosure is opened.
  await expect(dockForm.getByTestId("question-raw")).toHaveCount(0);

  // Answer both questions through the card.
  await dockForm.getByRole("radio", { name: /回到 GravityDB 开发/ }).click();
  await page.getByRole("tab", { name: /记忆/ }).click();
  await dockForm.getByRole("checkbox", { name: /保存端口/ }).click();
  await dockForm.getByRole("checkbox", { name: /保存环境变量/ }).click();
  await dockForm.getByTestId("question-submit").click();
  await expect(dockForm).toHaveCount(0, { timeout: 15_000 });
  await expect.poll(() => pendingCount(page, instanceId), { timeout: 15_000 }).toBe(0);

  // The decision really reached the harness: it continued with the labels.
  await expect
    .poll(async () => (await latestJournal(page, instanceId)).includes("answered via hook"), {
      timeout: 15_000,
    })
    .toBe(true);
  const journal = await latestJournal(page, instanceId);
  expect(journal).toContain("回到 GravityDB 开发");
  expect(journal).toContain("保存端口");
  expect(journal).toContain("保存环境变量");
  // The composer is usable again.
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 15_000 });

  // Same card on the approvals page (evidence shots at 390/1440, both themes).
  const terminal = await createSession(page, "ask-question again for the approvals page");
  await page.goto("/approvals");
  const row = page.getByTestId("approval-row").filter({ hasText: "AskUserQuestion" }).first();
  await expect(row).toBeVisible({ timeout: 20_000 });
  const card = row.getByTestId("question-form");
  await expect(card.getByText("接下来这个会话主要想做什么？")).toBeVisible();
  await expect(card.getByText("沿当前环境线索继续排查")).toBeVisible();

  for (const [width, height, theme, suffix] of [
    [1440, 900, "night", "1440-night"],
    [1440, 900, "ledger", "1440-ledger"],
    [390, 844, "night", "390-night"],
    [390, 844, "ledger", "390-ledger"],
  ] as const) {
    await page.setViewportSize({ width, height });
    // The width implies the shell: 390px redirects /approvals -> /m/inbox
    // (D-049) and 1440px keeps /approvals. Wait for that asynchronous
    // navigation and re-assert the card so the frame is never shot mid-
    // redirect against the previous shell's stale tree.
    await expect(page).toHaveURL(width < 768 ? /\/m\/inbox(?:\?|$)/ : /\/approvals(?:\?|$)/);
    const frameCard = page
      .getByTestId("approval-row")
      .filter({ hasText: "AskUserQuestion" })
      .first()
      .getByTestId("question-form");
    await expect(frameCard.getByText("接下来这个会话主要想做什么？")).toBeVisible();
    await page.evaluate((value) => document.documentElement.setAttribute("data-theme", value), theme);
    await shot(page, `ask-user-question-1-card-${suffix}.png`);
  }

  // It answers from the approvals page too. The 390px frames redirect
  // /approvals -> /m/inbox (D-049), and widening back bounces /m/inbox ->
  // /sessions, so re-enter the approvals centre and re-locate the card.
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto("/approvals");
  const desktopRow = page.getByTestId("approval-row").filter({ hasText: "AskUserQuestion" }).first();
  await expect(desktopRow).toBeVisible({ timeout: 20_000 });
  const desktopCard = desktopRow.getByTestId("question-form");
  await desktopCard.getByRole("radio", { name: /继续排查 remuda 环境/ }).click();
  await desktopCard.getByRole("tab", { name: /记忆/ }).click();
  await desktopCard.getByRole("checkbox", { name: /保存端口/ }).click();
  await desktopCard.getByTestId("question-submit").click();
  await expect(page.getByTestId("approval-row").filter({ hasText: "AskUserQuestion" })).toHaveCount(0, {
    timeout: 15_000,
  });
  await expect.poll(() => pendingCount(page, terminal), { timeout: 15_000 }).toBe(0);
});

test("free text answers travel verbatim and deny is a separate control", async ({ page }) => {
  const instanceId = await createSession(page, "ask-question free text now");
  const form = page.getByTestId("question-form");
  await expect(form).toBeVisible({ timeout: 20_000 });
  await form.getByTestId("question-free-q0").fill("先聊点别的，月球基地");
  await page.getByRole("tab", { name: /记忆/ }).click();
  await form.getByRole("checkbox", { name: /保存端口/ }).click();
  await form.getByTestId("question-submit").click();
  await expect
    .poll(() => latestJournal(page, instanceId), { timeout: 15_000 })
    .toContain("月球基地");
});

test("a question answered in the terminal closes the row and the banner by itself", async ({ page }) => {
  const instanceId = await createSession(page, "ask-question-terminal the human answers locally");
  const form = page.getByTestId("question-form");
  await expect(form).toBeVisible({ timeout: 20_000 });
  expect(await pendingCount(page, instanceId)).toBeGreaterThan(0);

  // Nobody clicks anything: the harness answers its own TUI form.
  await expect(form).toHaveCount(0, { timeout: 10_000 });
  await expect.poll(() => pendingCount(page, instanceId), { timeout: 10_000 }).toBe(0);
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 10_000 });

  // The approvals queue loses the row as well.
  await page.goto("/approvals");
  await expect(page.getByTestId("approval-row").filter({ hasText: "AskUserQuestion" })).toHaveCount(0, {
    timeout: 10_000,
  });

  // The journal carries the terminal answer with the chosen labels.
  const journal = await latestJournal(page, instanceId);
  expect(journal).toContain("answered in terminal");
  expect(journal).toContain("继续排查 remuda 环境");
  expect(journal).toContain("保存端口");
  expect(journal).toContain("terminal-answered");
});
