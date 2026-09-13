import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";

const here = path.dirname(fileURLToPath(import.meta.url));
/**
 * A default run must not touch tracked files, so shots land in the gitignored
 * `test-results/`. Re-capture the committed evidence with REMUDA_EVIDENCE=1.
 */
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence
  ? path.join(here, "../../../docs/design/evidence")
  : path.join(here, "../../test-results/providers-2");
/** Fake Anthropic-Messages gateway started by the hub_e2e harness. */
const upstream = process.env.VITE_E2E_UPSTREAM ?? "http://127.0.0.1:58881";
const token = "sk-fake-e2e-discover-qqqq";

async function shot(page: Page, name: string) {
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

test.describe.configure({ mode: "serial" });

test("discover a gateway catalog, expose two models, and launch with them", async ({ page }) => {
  await login(page, "e2e-providers");
  await page.goto("/providers");
  await expect(page.getByTestId("providers-page")).toBeVisible();

  await page.getByTestId("provider-add").click();
  await expect(page.getByTestId("provider-form")).toBeVisible();
  await page.getByTestId("provider-name").fill("e2e-fake-upstream");
  await page.getByTestId("provider-base-url").fill(`${upstream}/v1`);
  await page.getByTestId("provider-token").fill(token);
  await expect(page.getByTestId("provider-models-empty")).toBeVisible();

  // Real POST /v1/providers/discover against the fake upstream.
  const probed = page.waitForResponse(
    (response) => new URL(response.url()).pathname === "/v1/providers/discover",
  );
  await page.getByTestId("provider-discover").click();
  const response = await probed;
  expect(response.ok()).toBe(true);
  // The Hub must not echo the token back into the discovery result.
  expect(await response.text()).not.toContain(token);

  // The fake upstream answers /v1/models differently per header, so discovery
  // must union both listings: 3 ids from the plain OpenAI-style one, plus
  // cursor/e2e-wide only there and claude-e2e-only only in the Anthropic one.
  await expect(page.getByTestId("provider-model-row")).toHaveCount(5);
  await expect(page.getByTestId("provider-models-count")).toContainText("5/5 已启用");
  const auto = page.locator('[data-testid=provider-model-row][data-model="e2e/auto"]');
  await expect(auto).toContainText("E2E Auto");
  await expect(auto.getByTestId("provider-model-context")).toHaveText("1m");
  // Served by both listings, so both surfaces are chipped.
  await expect(auto.getByTestId("provider-model-surface")).toHaveText(["openai", "anthropic"]);
  // An id only the Anthropic listing serves would be missed by a single probe.
  const claudeOnly = page.locator(
    '[data-testid=provider-model-row][data-model="claude-e2e-only"]',
  );
  await expect(claudeOnly.getByTestId("provider-model-surface")).toHaveText(["anthropic"]);
  // ...and one only the plain listing serves, which the old probe also missed.
  const cursorOnly = page.locator(
    '[data-testid=provider-model-row][data-model="cursor/e2e-wide"]',
  );
  await expect(cursorOnly.getByTestId("provider-model-surface")).toHaveText(["openai"]);
  // Ids are bucketed by prefix so a large catalog stays scannable.
  await expect(page.getByTestId("provider-model-group")).toHaveCount(3);
  // A first probe has no prior catalog, so no row is badged 新增.
  await expect(page.locator('[data-testid=provider-model-row][data-new="1"]')).toHaveCount(0);
  const fast = page.locator('[data-testid=provider-model-row][data-model="e2e/fast"]');
  await expect(fast).toContainText("200k");
  await shot(page, "providers-2-discover-1440.png");

  // The search box narrows a catalog too long to scan.
  await page.getByTestId("provider-model-search").fill("cursor");
  await expect(page.getByTestId("provider-model-row")).toHaveCount(1);
  await page.getByTestId("provider-model-search").fill("");
  await expect(page.getByTestId("provider-model-row")).toHaveCount(5);

  // Expose two of the five and keep e2e/auto as the default.
  const plain = page.locator('[data-testid=provider-model-row][data-model="e2e/plain"]');
  await plain.getByTestId("provider-model-enabled").uncheck();
  await cursorOnly.getByTestId("provider-model-enabled").uncheck();
  await claudeOnly.getByTestId("provider-model-enabled").uncheck();
  await expect(page.getByTestId("provider-models-count")).toContainText("2/5 已启用");
  await auto.getByTestId("provider-model-default").check();
  await shot(page, "providers-2-checklist-1440.png");
  await page.getByTestId("provider-save").click();
  await expect(page.getByTestId("provider-form")).toHaveCount(0);

  await page.getByText("e2e-fake-upstream").click();
  await expect(page.getByTestId("provider-detail")).toBeVisible();
  await expect(page.getByTestId("provider-model-summary")).toContainText("2/5 已启用");
  await expect(page.getByTestId("provider-default-model")).toContainText("e2e/auto");
  // The saved profile never renders the token.
  await expect(page.locator("body")).not.toContainText(token);
  await expect(page.getByTestId("provider-secret")).toContainText("••••qqqq");

  // /test reports the same unioned catalog the probe found.
  await page.getByTestId("provider-test").click();
  await expect(page.getByTestId("provider-test-result")).toContainText("5 models");
  await shot(page, "providers-2-detail-1440.png");

  // Editing re-probes with the stored token: no model is newly discovered and
  // the unticked ones stay hidden.
  await page.getByTestId("provider-edit").click();
  await expect(page.getByTestId("provider-model-row")).toHaveCount(5);
  await expect(plain.getByTestId("provider-model-enabled")).not.toBeChecked();
  await page.getByTestId("provider-discover").click();
  await expect(page.getByTestId("provider-models-count")).toContainText("2/5 已启用");
  await expect(page.locator('[data-testid=provider-model-row][data-new="1"]')).toHaveCount(0);
  await page.getByRole("button", { name: "取消" }).click();

  // New Session offers exactly the enabled models. Wait for the picker's own
  // GET /v1/providers to land and assert it carries the post-save catalog, so
  // the picker assertions never race a slower runner's in-flight fetch.
  const listed = page.waitForResponse(
    (response) =>
      new URL(response.url()).pathname === "/v1/providers" &&
      response.request().method() === "GET",
  );
  await page.goto("/sessions/new");
  await expect(page.getByTestId("new-session-sheet")).toBeVisible();
  const profiles = (await (await listed).json()) as {
    items: { name: string; models: { id: string; enabled: boolean }[] }[];
  };
  const saved = profiles.items.find((item) => item.name === "e2e-fake-upstream");
  // The list endpoint must carry the post-save catalog, not a pre-save one.
  expect(saved?.models.filter((m) => m.enabled).map((m) => m.id)).toEqual(["e2e/auto", "e2e/fast"]);
  await page.getByTestId("new-session-delegation-gateway").click();
  await expect(page.getByTestId("new-session-gateway-profile")).toContainText("e2e-fake-upstream");
  const picker = page.getByTestId("new-session-model");
  await expect(picker.locator("option")).toHaveCount(2);
  await expect(picker).toHaveValue("e2e/auto");
  await expect(picker.locator('option[value="e2e/plain"]')).toHaveCount(0);
  await shot(page, "providers-2-new-session-1440.png");
});

test("discovery against a dead gateway reports unreachable inline", async ({ page }) => {
  await login(page, "e2e-providers-dead");
  await page.goto("/providers");
  await page.getByTestId("provider-add").click();
  await page.getByTestId("provider-name").fill("dead-gateway");
  await page.getByTestId("provider-base-url").fill("http://127.0.0.1:1");
  await page.getByTestId("provider-discover").click();
  await expect(page.getByTestId("provider-discover-error")).toContainText("unreachable");
  await expect(page.getByTestId("provider-models-empty")).toBeVisible();
  await shot(page, "providers-2-unreachable-1440.png");
});
