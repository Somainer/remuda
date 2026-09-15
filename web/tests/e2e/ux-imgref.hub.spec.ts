import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * Image anchors in prompts (owner nit, 2026-09-15).
 *
 * Fake node + the in-process fake harness in
 * crates/remuda-hub/examples/hub_e2e.rs — never a real model. The fake node
 * appends one `[attachment-refs: #n obj_… media/type]` line per manifest
 * entry, in the order the Hub forwarded, so this spec can assert both token
 * numbering in the textarea and the manifest the harness receives.
 */

test.describe.configure({ mode: "serial" });

test.skip(process.env.HUB_E2E_EXTERNAL === "1", "Needs the in-process fake Node");

/**
 * The fake node advertises maxInstances 8 and earlier specs in the serial run
 * leave sessions behind, and a force-DELETE first closes each instance
 * (~seconds against the fake node), so the documented escape is the same one
 * ux-quickfind uses: raise the ceiling in beforeAll and restore it after.
 */
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

/** Best-effort concurrent slot reclamation (each DELETE is slow on the fake). */
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

/** Two distinct valid PNGs (1x1 red, 2x2 blue) that stay distinct through the
 *  browser's canvas re-encode and the Hub's digest de-dupe. */
async function pasteTwoImages(page: Page) {
  await page.evaluate(async () => {
    const red =
      "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
    const toFile = (bytes: Uint8Array, name: string) =>
      new File([bytes], name, { type: "image/png" });
    const redFile = toFile(
      Uint8Array.from(atob(red), (char) => char.charCodeAt(0)),
      "red.png",
    );
    // A real 2x2 blue PNG from a canvas — guaranteed to decode and to survive
    // re-encode with different bytes from the 1x1 red image.
    const canvas = document.createElement("canvas");
    canvas.width = 2;
    canvas.height = 2;
    const ctx = canvas.getContext("2d");
    ctx!.fillStyle = "#1166ff";
    ctx!.fillRect(0, 0, 2, 2);
    const blob = await new Promise<Blob | null>((resolve) => canvas.toBlob(resolve, "image/png"));
    if (!blob) throw new Error("toBlob failed");
    const blueFile = new File([blob], "blue.png", { type: "image/png" });

    const data = new DataTransfer();
    data.items.add(redFile);
    data.items.add(blueFile);
    const area = document.querySelector("[data-testid='composer-input']");
    area?.dispatchEvent(new ClipboardEvent("paste", { clipboardData: data, bubbles: true }));
  });
}

async function createAttachmentSession(page: Page): Promise<string> {
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
  await page.getByTestId("new-session-prompt").fill("image anchor session");
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
  const instanceId = new URL(page.url()).pathname.split("/").pop() as string;
  await expect(page.getByTestId("session-page")).toBeVisible();

  // The fake Node raises an approval on create; answer it through the API so
  // the composer unlocks (copied from hub-live.spec's attachment case).
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

test("two pastes get tokens 1 and 2; removing chip 1 renumbers; manifest arrives in order", async ({
  page,
}) => {
  await login(page);
  const uploads: number[] = [];
  page.on("response", (response) => {
    if (new URL(response.url()).pathname === "/v1/objects") uploads.push(response.status());
  });

  await createAttachmentSession(page);

  await pasteTwoImages(page);

  // Both chips stage, numbered 1 and 2, and matching tokens land in the draft.
  await expect(page.getByTestId("attachment-chip")).toHaveCount(2, { timeout: 20_000 });
  const input = page.getByTestId("composer-input");
  await expect(input).toHaveValue("[Image #1] [Image #2]");
  const badges = page.getByTestId("attachment-index");
  await expect(badges.nth(0)).toHaveText("1");
  await expect(badges.nth(1)).toHaveText("2");
  await expect.poll(() => uploads, { timeout: 20_000 }).toEqual([200, 200]);
  await expect(page.getByTestId("composer-send")).toBeEnabled({ timeout: 20_000 });

  // Delete the first chip: its token leaves the text and #2 renumbers to #1.
  await page.getByTestId("attachment-remove").first().click();
  await expect(page.getByTestId("attachment-chip")).toHaveCount(1);
  await expect(input).toHaveValue("[Image #1]");
  await expect(page.getByTestId("attachment-index")).toHaveText("1");

  await page.getByTestId("composer-send").click();

  // The fake harness receives exactly one manifest entry, numbered #1.
  await expect(
    page
      .getByTestId("message")
      .filter({ hasText: "[attachment-refs: #1 obj_" }),
  ).toBeVisible({ timeout: 20_000 });
  // …and not a stale #2.
  await expect(
    page.getByTestId("message").filter({ hasText: "[attachment-refs: #2" }),
  ).toHaveCount(0);
  // The prompt arrived verbatim, token included.
  await expect(
    page.getByTestId("message").filter({ hasText: "echo: [Image #1] [attachments: image/png]" }),
  ).toBeVisible({ timeout: 20_000 });
});

test("deleting the token text marks the chip 未引用 but the image is still sent", async ({
  page,
}) => {
  await login(page);
  await createAttachmentSession(page);

  await pasteTwoImages(page);
  await expect(page.getByTestId("attachment-chip")).toHaveCount(2, { timeout: 20_000 });
  const input = page.getByTestId("composer-input");
  await expect(input).toHaveValue("[Image #1] [Image #2]");

  // Edit the token away by hand: nothing un-stages, the chip just flags it.
  await input.fill("no tokens now");
  const firstChip = page.getByTestId("attachment-chip").first();
  await expect(firstChip).toHaveAttribute("data-unreferenced", "1");
  await expect(firstChip).toContainText("未引用（仍会发送）");

  await page.getByTestId("composer-send").click();
  // Both staged images still ride the send, chips become sent thumbnails.
  await expect(page.getByTestId("sent-attachments").first()).toBeVisible({ timeout: 20_000 });
  await expect(
    page.getByTestId("message").filter({ hasText: "[attachments: image/png,image/png]" }),
  ).toBeVisible({ timeout: 20_000 });
});
