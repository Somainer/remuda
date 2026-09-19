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
  : path.join(here, "../../test-results/evidence");

async function shot(page: Page, name: string): Promise<void> {
  if (!evidence) return;
  await mkdir(shotDir, { recursive: true });
  await page.evaluate(() => document.fonts.ready);
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

/**
 * D-045 §6.2 / c-cua-media: a screenshot a computer-use MCP tool returns
 * survives from the (fake) tool result into the session card.
 *
 * The fake Node stages a fixed synthetic 1x1 PNG through the host-token
 * object route and appends an `mcp__codex-computer-use__get_app_state` call
 * with a text + image result. The card must render a bounded thumbnail whose
 * bytes come from `/v1/objects/{id}`, and the journal row must name the object
 * id without ever carrying the base64 payload.
 *
 * Never a real desktop: the image is the same hard-coded PNG the hub_e2e
 * example embeds (`codex-cua.md` §6.4).
 */

test.describe.configure({ mode: "serial" });

test.skip(process.env.HUB_E2E_EXTERNAL === "1", "Needs the in-process fake Node");

async function patchMaxInstances(page: Page, value: number): Promise<void> {
  await page.evaluate(async (next) => {
    const list = await fetch("/v1/hosts", { credentials: "include" });
    const body = (await list.json()) as {
      items?: { hostId?: string; maxInstances?: number }[];
    };
    const id = body.items?.find((host) => host.hostId)?.hostId;
    if (!id) return;
    await fetch(`/v1/hosts/${id}`, {
      method: "PATCH",
      credentials: "include",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ maxInstances: next }),
    });
  }, value);
}

async function forceDeleteAllInstances(page: Page): Promise<void> {
  await page.evaluate(async () => {
    const list = await fetch("/v1/instances", { credentials: "include" });
    const body = (await list.json()) as { items?: { instanceId?: string }[] };
    await Promise.all(
      (body.items ?? []).map((instance) =>
        fetch(`/v1/instances/${instance.instanceId}?force=1`, {
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

/** Create the default (structured) session and answer the launch approval. */
async function createSession(page: Page): Promise<string> {
  await forceDeleteAllInstances(page);
  await page.goto("/sessions/new");
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
  await page.getByTestId("new-session-prompt").fill("cua media session");
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
  const instanceId = new URL(page.url()).pathname.split("/").pop() as string;
  await expect(page.getByTestId("session-page")).toBeVisible();

  // Answer the fake Node's create approval through the API.
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
  return instanceId;
}

test("a computer-use screenshot renders as a bounded thumbnail from the object route, and the journal has no base64", async ({
  page,
}) => {
  await login(page);
  const instanceId = await createSession(page);

  // The fake node's c-cua-media sentinel: stage a PNG, then append the MCP
  // call and its text+image result.
  await page.getByTestId("composer-input").fill("cua screenshot of the safari window");
  await page.getByTestId("composer-send").click();

  // The MCP card names server/tool.
  const card = page.getByTestId("tool-card").filter({ hasText: "codex-computer-use" }).first();
  await expect(card).toBeVisible({ timeout: 20_000 });
  await expect(card).toContainText("get_app_state");

  // The thumbnail resolves to the object route and actually decodes.
  const thumb = card.getByTestId("tool-thumb");
  await expect(thumb).toHaveAttribute("src", /^\/v1\/objects\/obj_/);
  await expect(thumb).toHaveAttribute("alt", "screen-1.png");
  await expect(thumb).toHaveAttribute("loading", "lazy");
  const src = (await thumb.getAttribute("src")) as string;
  await expect
    .poll(
      async () =>
        thumb.evaluate((img) => (img as HTMLImageElement).naturalWidth > 0 ? 1 : 0),
      { timeout: 10_000 },
    )
    .toBe(1);

  // The click target is the object route only (no lightbox).
  const link = card.getByTestId("tool-media-link");
  await expect(link).toHaveAttribute("href", src);
  expect((await link.getAttribute("target")) ?? "").toBe("_blank");

  // The text half of the result is still rendered.
  await expect(card).toContainText("window state captured");

  // Same-origin cookie fetch serves the staged bytes inline as image/png.
  const response = await page.request.get(src);
  expect(response.status()).toBe(200);
  expect(response.headers()["content-type"]).toBe("image/png");
  expect((response.headers()["content-disposition"] ?? "").startsWith("inline")).toBe(true);
  const body = await response.body();
  expect(body.subarray(0, 8)).toEqual(
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
  );

  // The journal names the object id in the image block but never carries the
  // base64 payload (D-045 §6.2).
  const journal = await page.evaluate(async (id) => {
    const response = await fetch(`/v1/instances/${id}/journal`, { credentials: "include" });
    return response.text();
  }, instanceId);
  expect(journal).toContain(`"type":"image"`);
  expect(journal).toContain(src.replace("/v1/objects/", ""));
  // Fixed base64 alphabet start of the synthetic PNG — must never be journaled.
  expect(journal).not.toContain("iVBORw0KGgo");

  if (evidence) {
    const parsed = JSON.parse(journal) as {
      events?: {
        kind?: string;
        payload?: unknown;
        event?: { kind?: string; payload?: unknown };
      }[];
    };
    const toolResultRow = parsed.events?.find((row) => {
      const event = row.event ?? row;
      const payload = event.payload as { blocks?: { type?: string }[] } | undefined;
      return event.kind === "tool_result" &&
        (payload?.blocks ?? []).some((block) => block.type === "image");
    });
    await mkdir(shotDir, { recursive: true });
    const { writeFile } = await import("node:fs/promises");
    await writeFile(
      path.join(shotDir, "codex-cua-4-journal-row.json"),
      `${JSON.stringify(toolResultRow ?? null, null, 2)}\n`,
    );
  }

  await shot(page, "codex-cua-4-tool-card-thumbnail.png");
});
