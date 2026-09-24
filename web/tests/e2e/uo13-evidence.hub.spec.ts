import { expect, test, type Page } from "@playwright/test";
import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";
import os from "node:os";
import { fileURLToPath } from "node:url";
import { setMode } from "./appearanceHelper";
import { login } from "./hub-auth";

/**
 * UO-13 evidence: the management surfaces (§4.10) — hosts + detail, fleet,
 * projects, providers + detail, bots + detail, and the login/pair card — in
 * both modes at 390, 768, 1024 and 1440. A default run skips everything.
 *
 * REMUDA_EVIDENCE=1 captures:
 *  - 390/1440 into the committed docs/design/evidence/ui-overhaul/ tree
 *  - 768/1024 into an OS temp dir (layout verification is not committed)
 */

test.skip(!process.env.REMUDA_EVIDENCE, "set REMUDA_EVIDENCE=1 to capture the committed screenshots");
test.describe.configure({ mode: "serial" });

const committedDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence/ui-overhaul");
/** 768/1024 captures stay out of the repo even when capturing. */
const scratchDir = path.join(os.tmpdir(), "remuda-uo13-evidence");
const COMMITTED_WIDTHS = new Set([390, 1440]);
const MODES = ["dark", "light"] as const;
const WIDTHS = [390, 768, 1024, 1440] as const;
type Mode = (typeof MODES)[number];

/** Fake Anthropic-Messages gateway the hub_e2e harness serves (same source
 *  as providers-discovery.spec.ts: process env, falling back to the config
 *  default port). The value is the origin; /v1 is appended per endpoint.
 *  A truthy check (not ??) is required: an inherited empty env var must
 *  fall through to the default, not serialize as baseUrl "/v1". */
const upstream = (process.env.VITE_E2E_UPSTREAM || `http://${process.env.HUB_E2E_UPSTREAM_LISTEN || "127.0.0.1:58881"}`).replace(
  /\/v1\/?$/,
  "",
);
const GATEWAY_NAME = "e2e-uo13-gateway";
const GATEWAY_TOKEN = "sk-e2e-uo13-evidence-qqqq";

async function shoot(page: Page, surface: string, mode: Mode, width: number): Promise<void> {
  await setMode(page, mode);
  await page.evaluate(() => document.fonts.ready.then(() => undefined));
  await page.waitForTimeout(300);
  // 390/1440 are the committed goldens; the intermediate widths are
  // captured for local review only and never written into the repo.
  const dir = COMMITTED_WIDTHS.has(width) ? committedDir : scratchDir;
  await writeFile(path.join(dir, `UO-13-${surface}-${mode}-${width}.png`), await page.screenshot({ animations: "disabled" }));
}

test.beforeAll(async () => {
  await mkdir(committedDir, { recursive: true });
  await mkdir(scratchDir, { recursive: true });
});

test.beforeEach(async ({ page }) => {
  await login(page, "e2e-uo13-evidence");
});

for (const width of WIDTHS) {
  test(`UO-13 management surfaces at ${width}`, async ({ page }) => {
    test.setTimeout(240_000);
    await page.setViewportSize({ width, height: width < 768 ? 844 : 900 });
    await page.emulateMedia({ reducedMotion: "reduce" });

    await page.goto("/hosts");
    await expect(page.getByTestId("hosts-page")).toBeVisible();
    await page.waitForTimeout(300);
    for (const mode of MODES) await shoot(page, "hosts", mode, width);

    // Required coverage: a host must exist on the enrolled fake Node.
    const firstHost = page.getByTestId("host-row").first();
    await expect(firstHost).toBeVisible();
    await firstHost.click();
    await expect(page.getByTestId("host-detail")).toBeVisible();
    for (const mode of MODES) await shoot(page, "host-detail", mode, width);

    await page.goto("/fleet");
    await expect(page.getByTestId("fleet-page")).toBeVisible();
    for (const mode of MODES) await shoot(page, "fleet", mode, width);

    await page.goto("/projects");
    await expect(page.getByTestId("projects-page")).toBeVisible();
    await page.waitForTimeout(300);
    for (const mode of MODES) await shoot(page, "projects", mode, width);

    // Deterministic provider detail. The harness data dir is ephemeral and a
    // name is NOT unique on re-create, so delete any stale rows first and POST
    // exactly one fresh gateway. Required coverage must never silently skip.
    const origin = new URL(page.url()).origin;
    const existing = await page.request.get("/v1/providers", { headers: { Origin: origin } });
    if (existing.ok()) {
      const stale = ((await existing.json()).items ?? []) as { id?: string; name?: string }[];
      for (const row of stale) {
        if (row.id && row.name === GATEWAY_NAME) {
          await page.request.delete(`/v1/providers/${row.id}`, { headers: { Origin: origin } });
        }
      }
    }
    const create = await page.request.post("/v1/providers", {
      headers: { Origin: origin },
      data: {
        name: GATEWAY_NAME,
        kind: "gateway",
        baseUrl: `${upstream}/v1`,
        authToken: GATEWAY_TOKEN,
        defaultGateway: true,
      },
    });
    if (!create.ok()) {
      throw new Error(`gateway seed failed: ${create.status()} ${await create.text()}`);
    }
    const createdId = ((await create.json()) as { id?: string }).id;
    if (!createdId) throw new Error("gateway seed returned no id");

    await page.goto("/providers");
    await expect(page.getByTestId("providers-page")).toBeVisible();
    for (const mode of MODES) await shoot(page, "providers", mode, width);

    // Navigate by id rather than clicking a possibly duplicated name row.
    await page.goto(`/providers/${createdId}`);
    const detail = page.getByTestId("provider-detail");
    await expect(detail).toBeVisible();
    // The detail must be the seeded gateway's populated card, never the
    // empty 「未知 profile」 shell: heading/delegation/baseUrl present in
    // every captured frame (dark and light).
    await expect(detail.locator("h1")).toContainText(GATEWAY_NAME);
    await expect(detail).toHaveAttribute("data-delegation", "gateway");
    await expect(detail).toContainText(upstream.replace(/\/v1\/?$/, ""));

    for (const mode of MODES) {
      await setMode(page, mode);
      await page.evaluate(() => document.fonts.ready.then(() => undefined));
      // Re-assert populated content inside the mode loop so a dark frame
      // cannot silently capture the shell.
      await expect(detail.locator("h1")).toContainText(GATEWAY_NAME);
      await expect(detail).toHaveAttribute("data-delegation", "gateway");
      await shoot(page, "provider-detail", mode, width);
    }

    await page.goto("/bots");
    await expect(page.getByTestId("bots-page")).toBeVisible();
    for (const mode of MODES) await shoot(page, "bots", mode, width);

    // Required coverage: the feishu static channel must exist.
    const feishu = page.locator('[data-testid=bot-row][data-channel="feishu"]');
    await expect(feishu).toBeVisible();
    await feishu.click();
    await expect(page.getByTestId("bot-detail")).toBeVisible();
    for (const mode of MODES) await shoot(page, "bot-detail", mode, width);
  });

  test(`UO-13 login and pair card at ${width}`, async ({ browser }) => {
    test.setTimeout(120_000);
    // The card only renders while unauthenticated: a fresh context that never
    // logs in renders both /login and /pair. Clear its storage so the mock's
    // auto-bootstrap (dev only) cannot turn it back into an authed session.
    const context = await browser.newContext({
      viewport: { width, height: width < 768 ? 844 : 900 },
      reducedMotion: "reduce",
    });
    const page = await context.newPage();
    await page.addInitScript(() => {
      localStorage.clear();
      localStorage.setItem("runtime.logged-out", "1");
    });
    try {
      await page.goto("/login");
      await expect(page.getByTestId("login-page")).toBeVisible();
      await expect(page.getByTestId("login-head")).toBeVisible();
      for (const mode of MODES) await shoot(page, "login", mode, width);

      await page.goto("/pair");
      await expect(page.getByTestId("login-page")).toHaveAttribute("data-mode", "pair");
      for (const mode of MODES) await shoot(page, "pair", mode, width);
    } finally {
      await context.close();
    }
  });
}
