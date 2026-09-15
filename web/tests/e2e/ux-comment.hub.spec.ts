import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

const here = path.dirname(fileURLToPath(import.meta.url));
/** Committed evidence only under REMUDA_EVIDENCE=1; default runs stay git-clean. */
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence
  ? path.join(here, "../../../docs/design/evidence")
  : path.join(here, "../../test-results/ux-comment");

async function shot(page: Page, name: string) {
  if (!evidence) return;
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

/**
 * 评论 code-block quote anchors (workbench-code-2, 2026-09-15).
 *
 * Fake node + the in-process fake harness in
 * crates/remuda-hub/examples/hub_e2e.rs — never a real model. A prompt
 * containing "show me code" makes the harness reply with a fenced ts block;
 * pressing the block's 评论 action inserts a [Code #n] chip into the
 * composer, and on send the quoted block expands in front of the prompt text
 * — the fake harness echoes it verbatim, so this spec asserts the expansion
 * reached the model.
 */

test.describe.configure({ mode: "serial" });

test.skip(process.env.HUB_E2E_EXTERNAL === "1", "Needs the in-process fake Node");

async function patchMaxInstances(page: Page, value: number): Promise<number | undefined> {
  return page.evaluate(async (next) => {
    const list = await fetch("/v1/hosts", { credentials: "include" });
    const body = (await list.json()) as {
      items?: { hostId?: string; maxInstances?: number }[];
    };
    const id = body.items?.find((host) => host.hostId)?.hostId;
    if (!id) return undefined;
    const previous = body.items?.find((host) => host.hostId === id)?.maxInstances;
    await fetch(`/v1/hosts/${id}`, {
      method: "PATCH",
      credentials: "include",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ maxInstances: next }),
    });
    return previous;
  }, value);
}

async function forceDeleteAllInstances(page: Page) {
  await page.evaluate(async () => {
    const list = await fetch("/v1/instances", { credentials: "include" });
    const body = (await list.json()) as { items?: { id: string }[] };
    await Promise.all(
      (body.items ?? []).map((instance) =>
        fetch(`/v1/instances/${instance.id}?force=1`, {
          method: "DELETE",
          credentials: "include",
        }).catch(() => undefined),
      ),
    );
  });
}

test.beforeAll(async ({ browser }) => {
  const page = await browser.newPage();
  await login(page);
  await patchMaxInstances(page, 24);
  await page.close();
});

test.afterAll(async ({ browser }) => {
  const page = await browser.newPage();
  await login(page);
  await patchMaxInstances(page, 8);
  await forceDeleteAllInstances(page).catch(() => undefined);
  await page.close();
});

test.afterEach(async ({ page }) => {
  await forceDeleteAllInstances(page).catch(() => undefined);
});

async function createSession(page: Page, prompt: string): Promise<void> {
  await forceDeleteAllInstances(page);
  await page.getByTitle("新建", { exact: true }).click();
  await expect(page.getByTestId("new-session-sheet")).toBeVisible();
  await expect(page.getByTestId("new-session-host")).toContainText("e2e-fake-node", {
    timeout: 20_000,
  });
  const host = await page
    .getByTestId("new-session-host")
    .locator("option")
    .filter({ hasText: "e2e-fake-node" })
    .getAttribute("value");
  await page.getByTestId("new-session-host").selectOption(host!);
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0);
  await page.getByTestId("new-session-prompt").fill(prompt);
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
  const instanceId = new URL(page.url()).pathname.split("/").pop() as string;
  await expect(page.getByTestId("session-page")).toBeVisible();

  // Answer the create approval so the composer unlocks (fake-node trap).
  await expect
    .poll(
      async () =>
        page.evaluate(async (id) => {
          const list = await fetch("/v1/interactions", { credentials: "include" });
          const body = (await list.json()) as {
            items?: {
              id: string;
              instanceId?: string;
              state?: string;
              request?: { kind?: string; inputDigest?: string; options?: { id: string }[] };
            }[];
          };
          const mine = (body.items ?? []).filter(
            (item) => item.instanceId === id && item.state === "pending",
          );
          for (const item of mine) {
            const optionId = item.request?.options?.[0]?.id;
            if (!optionId) continue;
            await fetch(`/v1/interactions/${item.id}/answer`, {
              method: "POST",
              credentials: "include",
              headers: { "content-type": "application/json" },
              body: JSON.stringify({
                answer: {
                  kind: "approval",
                  optionId,
                  inputDigest: item.request?.inputDigest ?? "",
                },
              }),
            });
          }
          return mine.length;
        }, instanceId),
      { timeout: 20_000 },
    )
    .toBe(0);
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });
}

test("评论 on an assistant code block quotes it into the composer and the harness receives the expansion", async ({
  page,
}) => {
  await login(page);
  await createSession(page, "show me code");

  // The fake harness's reply renders a fenced ts block with a path.
  const codeBlock = page.getByTestId("code-block").first();
  await expect(codeBlock).toBeVisible({ timeout: 20_000 });
  await expect(codeBlock).toContainText("export function add");

  // The third toolbar action exists only because a composer is mounted.
  const comment = codeBlock.getByTestId("code-comment");
  await expect(comment).toHaveAttribute("title", "评论");
  await comment.click();

  // A numbered quote chip appears and the token lands at the caret.
  const chip = page.getByTestId("code-quote-chip");
  await expect(chip).toHaveCount(1);
  await expect(chip.getByTestId("code-quote-index")).toHaveText("1");
  await expect(chip).toContainText("src/math.ts");
  const input = page.getByTestId("composer-input");
  await expect(input).toHaveValue(/\[Code #1\]/);
  await shot(page, "workbench-code-2-1440-night.png");

  // Removing the chip strips the token and the quote.
  await chip.getByTestId("code-quote-remove").click();
  await expect(chip).toHaveCount(0);
  await expect(input).toHaveValue("");

  // Quote again, then write the actual question after the token.
  await comment.click();
  await expect(chip).toHaveCount(1);
  await input.fill("[Code #1] what does this return for 1 and 2?");
  await page.getByTestId("composer-send").click();

  // The fake harness records the prompt verbatim: the quoted block expanded
  // immediately before the draft text, in the [Code #1] shape.
  // The user message (first match) carries the expansion verbatim; the
  // harness's echo below repeats the same text.
  const echoed = page
    .getByTestId("message")
    .filter({ hasText: "[Code #1] quoted from the assistant's message" })
    .first();
  await expect(echoed).toBeVisible({ timeout: 20_000 });
  await expect(echoed).toContainText("(lines 1-3 of src/math.ts):");
  await expect(echoed).toContainText("export function add(a: number, b: number): number {");
  await expect(echoed).toContainText("return a + b;");
  await expect(echoed).toContainText("[Code #1] what does this return for 1 and 2?");
});

test("deleting the [Code #n] token marks the chip 未引用 but the quote is still sent", async ({
  page,
}) => {
  await login(page);
  await createSession(page, "show me code");
  const codeBlock = page.getByTestId("code-block").first();
  await expect(codeBlock).toBeVisible({ timeout: 20_000 });
  await codeBlock.getByTestId("code-comment").click();
  const chip = page.getByTestId("code-quote-chip");
  await expect(chip).toHaveCount(1);
  const input = page.getByTestId("composer-input");
  await expect(input).toHaveValue(/\[Code #1\]/);

  await input.fill("no token here");
  await expect(chip).toHaveAttribute("data-unreferenced", "1");
  await expect(chip).toContainText("未引用（仍会发送）");

  await page.getByTestId("composer-send").click();
  // The user message (first match) carries the expansion verbatim; the
  // harness's echo below repeats the same text.
  const echoed = page
    .getByTestId("message")
    .filter({ hasText: "[Code #1] quoted from the assistant's message" })
    .first();
  await expect(echoed).toBeVisible({ timeout: 20_000 });
  await expect(echoed).toContainText("export function add");
});

test("evidence shots: toolbar 评论 + quote chip at 390 and 1440", async ({ page }) => {
  test.skip(!evidence, "set REMUDA_EVIDENCE=1 to capture committed screenshots");
  await login(page);
  await createSession(page, "show me code");
  const codeBlock = page.getByTestId("code-block").first();
  await expect(codeBlock).toBeVisible({ timeout: 20_000 });
  await codeBlock.getByTestId("code-comment").click();
  await expect(page.getByTestId("code-quote-chip")).toHaveCount(1);
  await shot(page, "workbench-code-2-1440-night.png");
  await page.setViewportSize({ width: 390, height: 844 });
  await expect(page.getByTestId("composer-input")).toBeVisible();
  await shot(page, "workbench-code-2-390-night.png");
});
