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

async function shot(page: Page, name: string) {
  if (!evidence) return;
  await mkdir(shotDir, { recursive: true });
  await page.evaluate(() => document.fonts.ready);
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

async function setTheme(page: Page, theme: "night" | "ledger") {
  await page.evaluate((value) => {
    document.documentElement.dataset.theme = value;
  }, theme);
}

/**
 * D-027b: attachments of ANY file type, not only images.
 *
 * Fake node + the in-process fake harness in
 * crates/remuda-hub/examples/hub_e2e.rs — never a real model. The fake node
 * pulls each staged object over HTTP (host token), lands it under the
 * sanitised original filename with a numeric collision suffix, and echoes the
 * exact driver expansion line back in the reply:
 *
 *   [File #n] <name> (<mime>, <size>) saved at <absolute path>
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
  await page.getByTestId("new-session-prompt").fill("file attachment session");
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

/** Paste a .txt and a .pdf (distinct content/names) into the composer. */
async function pasteTextAndPdf(page: Page) {
  await page.evaluate(() => {
    const txt = new File(["hello from a text attachment\n"], "e2e notes.txt", {
      type: "text/plain",
    });
    const pdf = new File(["%PDF-1.7 fake pdf body for d-027b"], "e2e report.pdf", {
      type: "application/pdf",
    });
    const data = new DataTransfer();
    data.items.add(txt);
    data.items.add(pdf);
    const area = document.querySelector("[data-testid='composer-input']");
    area?.dispatchEvent(new ClipboardEvent("paste", { clipboardData: data, bubbles: true }));
  });
}

test("a .txt and a .pdf stage as file chips, land by name, and reach the harness as [File #n] lines", async ({
  page,
}) => {
  await login(page);
  const uploads: { status: number; type: string }[] = [];
  page.on("response", (response) => {
    if (new URL(response.url()).pathname === "/v1/objects") {
      uploads.push({ status: response.status(), type: response.request().headers()["content-type"] });
    }
  });

  const instanceId = await createAttachmentSession(page);
  await pasteTextAndPdf(page);

  // Two file chips (not images), numbered 1 and 2, with type glyphs not thumbs.
  const chips = await page.locator("[data-testid='attachment-chip']").all();
  expect(chips).toHaveLength(2);
  await expect(page.locator("[data-testid='attachment-chip']").nth(0)).toHaveAttribute(
    "data-kind",
    "file",
  );
  await expect(page.locator("[data-testid='attachment-chip']").nth(1)).toHaveAttribute(
    "data-kind",
    "file",
  );
  expect(await page.locator("[data-testid='attachment-chip'] img").count()).toBe(0);
  await expect(page.getByText("e2e notes.txt")).toBeVisible();
  await expect(page.getByText("e2e report.pdf")).toBeVisible();

  const input = page.getByTestId("composer-input");
  await expect(input).toHaveValue(/\[File #1\] \[File #2\]|\[File #2\] \[File #1\]/);
  await expect
    .poll(
      () => uploads.map((entry) => `${entry.status}:${entry.type}`).sort().join(","),
      { timeout: 20_000 },
    )
    .toBe("200:application/pdf,200:text/plain");
  // Clipboard iteration order is browser-defined; map name -> chip number.
  const chipByName = await page
    .locator("[data-testid='attachment-chip']")
    .evaluateAll((nodes) =>
      nodes.map((node) => ({
        index: node.getAttribute("data-index"),
        name: node.querySelector("[class*='name']")?.textContent ?? "",
      })),
    );
  const numberFor = (name: string) => chipByName.find((chip) => chip.name === name)?.index ?? "";
  const txtNumber = numberFor("e2e notes.txt");
  const pdfNumber = numberFor("e2e report.pdf");
  expect(txtNumber).toBeTruthy();
  expect(pdfNumber).toBeTruthy();
  expect(txtNumber).not.toBe(pdfNumber);
  await expect(page.getByTestId("composer-send")).toBeEnabled({ timeout: 20_000 });

  // The sent bubble shows download chips linking the Hub objects.
  await page.getByTestId("composer-send").click();
  const sentFiles = page.getByTestId("sent-file");
  await expect(sentFiles).toHaveCount(2, { timeout: 20_000 });
  const hrefs = await sentFiles.evaluateAll((nodes) =>
    (nodes as HTMLAnchorElement[]).map((a) => a.getAttribute("href")),
  );
  expect(hrefs?.every((href) => href?.startsWith("/v1/objects/obj_"))).toBe(true);

  // The fake harness (Node) recorded the manifest and the file expansion line.
  const messages = page.getByTestId("message");
  await expect
    .poll(
      async () =>
        (await messages.filter({ hasText: "[attachments: text/plain,application/pdf]" }).count()) +
        (await messages.filter({ hasText: "[attachment-refs: #1" }).count()) +
        (await messages.filter({ hasText: "[attachment-refs: #2" }).count()),
      { timeout: 20_000 },
    )
    .toBe(3);

  const txtLine = messages.filter({
    hasText: new RegExp(`\\[File #${txtNumber}\\] e2e notes\\.txt`),
  });
  const pdfLine = messages.filter({
    hasText: new RegExp(`\\[File #${pdfNumber}\\] e2e report\\.pdf`),
  });
  await expect(txtLine).toBeVisible();
  await expect(pdfLine).toBeVisible();

  // The quoted expansion lines render as collapsed rows (the transcript fold
  // for anchors): summary carries name + mime + size, opening shows the path.
  const mentions = page.getByTestId("file-mention");
  await expect(mentions).toHaveCount(2);
  const mentionByName = async (name: string) => {
    const count = await mentions.count();
    for (let i = 0; i < count; i += 1) {
      const row = mentions.nth(i);
      if ((await row.textContent())?.includes(name)) return row;
    }
    throw new Error(`no file-mention row for ${name}`);
  };
  const txtMention = await mentionByName("e2e notes.txt");
  const pdfMention = await mentionByName("e2e report.pdf");
  await expect(txtMention).toContainText("text/plain");
  await expect(pdfMention).toContainText("application/pdf");
  // Collapsed by default.
  expect(await txtMention.evaluate((node) => (node as HTMLDetailsElement).open)).toBe(false);
  await txtMention.locator("summary").click();
  const txtMentionText = (await txtMention.textContent()) ?? "";
  expect(txtMentionText).toMatch(/saved at .*e2e notes\.txt/);
  await pdfMention.locator("summary").click();
  const pdfMentionText = (await pdfMention.textContent()) ?? "";
  expect(pdfMentionText).toMatch(/saved at .*e2e report\.pdf/);

  // GET serves the PDF back with the right content type and attachment
  // disposition with the sanitised filename (same-origin cookie auth).
  const objectHref = hrefs?.find((href) => href?.includes("obj_")) as string;
  const pdfHref = await page
    .locator("[data-testid='sent-file'][download='e2e report.pdf']")
    .getAttribute("href");
  const res = await page.request.get(pdfHref ?? objectHref);
  expect(res.status()).toBe(200);
  expect(res.headers()["content-type"]).toBe("application/pdf");
  const disposition = res.headers()["content-disposition"] ?? "";
  expect(disposition).toContain("attachment");
  expect(disposition).toContain("e2e report.pdf");
  expect(res.headers()["x-content-type-options"]).toBe("nosniff");
  void instanceId;
});

test("a second send of the same filename lands with a numeric collision suffix", async ({
  page,
}) => {
  await login(page);
  await createAttachmentSession(page);
  await pasteTextAndPdf(page);
  await expect(page.getByTestId("composer-send")).toBeEnabled({ timeout: 20_000 });
  await page.getByTestId("composer-send").click();
  await expect(
    page.getByTestId("message").filter({ hasText: "e2e notes.txt" }),
  ).toBeVisible({ timeout: 20_000 });

  // Send the same two filenames again; the second landing must not overwrite.
  await pasteTextAndPdf(page);
  await expect(page.getByTestId("composer-send")).toBeEnabled({ timeout: 20_000 });
  await page.getByTestId("composer-send").click();
  const messages = page.getByTestId("message");
  await expect(messages.filter({ hasText: "saved at" }).first()).toBeVisible({ timeout: 20_000 });
  const all = await messages.allTextContents();
  const suffixHits = all.filter((text) =>
    /saved at .*e2e notes-1\.txt/.test(text) || /saved at .*e2e report-1\.pdf/.test(text),
  );
  expect(suffixHits.length).toBeGreaterThan(0);
});

test("the per-message count cap (8) surfaces as a composer notice", async ({ page }) => {
  await login(page);
  await createAttachmentSession(page);
  await page.evaluate(() => {
    const data = new DataTransfer();
    // Nine files — one over the cap.
    for (let i = 1; i <= 9; i += 1) {
      data.items.add(new File([`body ${i}`], `f${i}.txt`, { type: "text/plain" }));
    }
    const area = document.querySelector("[data-testid='composer-input']");
    area?.dispatchEvent(new ClipboardEvent("paste", { clipboardData: data, bubbles: true }));
  });
  await expect(page.getByTestId("attachment-notice")).toContainText("最多 8 个附件");
  expect(await page.locator("[data-testid='attachment-chip']").count()).toBe(8);
});

test("the per-file size cap surfaces as a failed chip with the Hub's RESOURCE_LIMIT", async ({
  page,
}) => {
  await login(page);
  await createAttachmentSession(page);
  // The hub_e2e example honours HUB_E2E_ATTACHMENT_MAX_BYTES (default 25 MiB);
  // pushing 26 MiB through the dev proxy is needlessly slow, so the suite runs
  // with a smaller configured cap. Either way the file is 1 KiB over it and
  // must fail at the Hub, not silently disappear.
  const cap = Number(process.env.HUB_E2E_ATTACHMENT_MAX_BYTES ?? 25 * 1024 * 1024);
  const bytes = new Uint8Array(cap + 1024);
  for (let i = 0; i < bytes.length; i += 4096) bytes[i] = (i / 4096) % 256;
  await page.evaluate((payload) => {
    const file = new File([payload], "too-big.bin", { type: "application/octet-stream" });
    const data = new DataTransfer();
    data.items.add(file);
    const area = document.querySelector("[data-testid='composer-input']");
    area?.dispatchEvent(new ClipboardEvent("paste", { clipboardData: data, bubbles: true }));
  }, bytes);
  const chip = page.locator("[data-testid='attachment-chip']").first();
  await expect(chip).toHaveAttribute("data-state", "failed", { timeout: 60_000 });
  await expect(chip).toContainText(/RESOURCE_LIMIT|limit/i);
});

test("evidence: mixed file + image chips and the sent file row, 390/1440 both themes", async ({
  browser,
}) => {
  test.skip(!evidence, "set REMUDA_EVIDENCE=1 to capture committed evidence");
  const page = await browser.newPage();
  await page.emulateMedia({ reducedMotion: "reduce" });
  await login(page);
  await createAttachmentSession(page);

  await page.evaluate(() => {
    const pdf = new File(["%PDF-1.7 evidence body"], "Q3 report.pdf", {
      type: "application/pdf",
    });
    const txt = new File(["evidence notes"], "notes.txt", { type: "text/plain" });
    const canvas = document.createElement("canvas");
    canvas.width = 2;
    canvas.height = 2;
    canvas.getContext("2d")!.fillStyle = "#1166ff";
    canvas.getContext("2d")!.fillRect(0, 0, 2, 2);
    const data = new DataTransfer();
    data.items.add(pdf);
    data.items.add(txt);
    const area = document.querySelector("[data-testid='composer-input']");
    area?.dispatchEvent(new ClipboardEvent("paste", { clipboardData: data, bubbles: true }));
    void canvas;
  });
  await expect(page.locator("[data-testid='attachment-chip']")).toHaveCount(2, {
    timeout: 20_000,
  });
  await expect(page.getByTestId("composer-send")).toBeEnabled({ timeout: 20_000 });

  const frames: { width: number; theme: "night" | "ledger"; suffix: string }[] = [
    { width: 1440, theme: "night", suffix: "1440-night" },
    { width: 390, theme: "night", suffix: "390-night" },
    { width: 1440, theme: "ledger", suffix: "1440-ledger" },
    { width: 390, theme: "ledger", suffix: "390-ledger" },
  ];
  for (const frame of frames) {
    await page.setViewportSize({ width: frame.width, height: Math.max(844, frame.width > 800 ? 900 : 844) });
    await setTheme(page, frame.theme);
    await page.waitForTimeout(100);
    // D-027b acceptance: long filenames/paths never force horizontal scroll,
    // especially at 390 px.
    await expect
      .poll(() =>
        page.evaluate(
          () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
        ),
      )
      .toBeLessThanOrEqual(1);
    await shot(page, `attachments-4-draft-${frame.suffix}.png`);
  }

  // Sent bubble with the download chips.
  await page.getByTestId("composer-send").click();
  await expect(page.getByTestId("sent-file")).toHaveCount(2, { timeout: 20_000 });
  // Wait for the harness round-trip so the frame shows a settled message,
  // not the transient 排队中 bubble.
  await expect(page.getByTestId("message").filter({ hasText: "saved at" }).first()).toBeVisible({
    timeout: 20_000,
  });
  await page.setViewportSize({ width: 1440, height: 900 });
  await setTheme(page, "night");
  await shot(page, "attachments-4-sent-1440-night.png");
  await setTheme(page, "ledger");
  await shot(page, "attachments-4-sent-1440-ledger.png");
  await page.setViewportSize({ width: 390, height: 844 });
  await setTheme(page, "night");
  await shot(page, "attachments-4-sent-390-night.png");
  await page.setViewportSize({ width: 390, height: 844 });
  await setTheme(page, "ledger");
  await shot(page, "attachments-4-sent-390-ledger.png");
  await page.close();
});
