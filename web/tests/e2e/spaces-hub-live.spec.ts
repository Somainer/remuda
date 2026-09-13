import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

const evidence = path.join(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence/spaces-1");
const primary = { id: "wsp_e2e", name: "remuda-e2e", root: "/tmp/remuda-e2e" };
const secondary = { id: "wsp_e2e_second", name: "remuda-e2e-second", root: "/tmp/remuda-e2e-second" };

function tab(page: Page, instanceId: string) {
  return page.getByTestId("space-tabs").getByTestId("session-tab")
    .and(page.locator(`[data-instance-id="${instanceId}"]`));
}

function space(page: Page, workspaceId: string) {
  return page.getByTestId("spaces-panel").getByTestId("space-select")
    .and(page.locator(`[data-space-id*='"${workspaceId}"']`));
}

async function createSession(page: Page, workspace: typeof primary, prompt: string) {
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  await expect(hostPicker).toContainText("e2e-fake-node", { timeout: 20_000 });
  const hostId = await hostPicker.locator("option").filter({ hasText: "e2e-fake-node" }).getAttribute("value");
  expect(hostId).toBeTruthy();
  await hostPicker.selectOption(hostId!);
  await expect(page.getByTestId("new-session-workspace").locator(`option[value="${workspace.id}"]`))
    .toHaveCount(1, { timeout: 20_000 });
  await page.getByTestId("new-session-workspace").selectOption(workspace.id);
  await page.getByTestId("new-session-prompt").fill(prompt);
  await expect(page.getByTestId("new-session-start")).toBeEnabled();
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
  const instanceId = new URL(page.url()).pathname.split("/")[2];
  await expect(page.getByTestId("message").filter({ hasText: `echo: ${prompt}` }))
    .toBeVisible({ timeout: 20_000 });
  await expect(tab(page, instanceId)).toHaveAttribute("aria-selected", "true");
  return { instanceId, hostId: hostId! };
}

async function screenshot(page: Page, name: string, theme: "night" | "ledger") {
  await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
  await page.evaluate(() => document.fonts.ready);
  await mkdir(evidence, { recursive: true });
  const image = await page.screenshot({ path: path.join(evidence, name), animations: "disabled", scale: "css" });
  expect(image.byteLength, `${name} must stay below 300 KB`).toBeLessThanOrEqual(300_000);
}

test("registered spaces isolate tabs, remember selection and collapse, and fit a phone drawer", async ({ page }) => {
  test.setTimeout(120_000);
  const pageErrors: string[] = [];
  page.on("pageerror", (error) => pageErrors.push(error.message));
  await page.setViewportSize({ width: 1440, height: 900 });
  await login(page, "spaces-e2e-browser");
  const first = await createSession(page, primary, "Review the project plan");
  const second = await createSession(page, primary, "Check the project tests");
  const other = await createSession(page, secondary, "Plan the second project");
  const panel = page.getByTestId("spaces-panel");
  const strip = page.getByTestId("space-tabs");

  await expect(tab(page, first.instanceId)).toHaveCount(0);
  await expect(tab(page, second.instanceId)).toHaveCount(0);
  await space(page, primary.id).click();
  await expect(tab(page, second.instanceId)).toHaveAttribute("aria-selected", "true");
  await expect(tab(page, first.instanceId)).toBeVisible();
  await expect(tab(page, other.instanceId)).toHaveCount(0);
  await tab(page, first.instanceId).click();
  await space(page, secondary.id).click();
  await expect(tab(page, other.instanceId)).toHaveAttribute("aria-selected", "true");
  await space(page, primary.id).click();
  await expect(tab(page, first.instanceId)).toHaveAttribute("aria-selected", "true");

  // A deep link restores both project and tab, including after a fresh load.
  await page.goto(`/s/${other.instanceId}`);
  await expect(tab(page, other.instanceId)).toHaveAttribute("aria-selected", "true");
  await expect(tab(page, first.instanceId)).toHaveCount(0);
  await page.reload();
  await expect(tab(page, other.instanceId)).toHaveAttribute("aria-selected", "true");

  // The global New action inherits the selected project rather than another
  // project's last successful creation preferences.
  await space(page, primary.id).click();
  await page.getByTitle("新建", { exact: true }).click();
  await expect(page.getByTestId("new-session-host")).toHaveValue(first.hostId);
  await expect(page.getByTestId("new-session-workspace")).toHaveValue(primary.id);
  await expect(page.getByTestId("new-session-workspace")).toContainText(primary.root);
  await page.getByTestId("new-session-sheet").getByRole("button", { name: "关闭", exact: true }).click();
  await space(page, primary.id).click();

  // Use the rendered order so legacy sessions from the other live spec do not
  // make numeric keyboard shortcuts depend on the fixture's session count.
  const tabs = strip.getByRole("tab");
  await tabs.first().click();
  await page.keyboard.press("ControlOrMeta+2");
  await expect(tabs.nth(1)).toHaveAttribute("aria-selected", "true");
  await page.keyboard.press("ControlOrMeta+1");
  await expect(tabs.first()).toHaveAttribute("aria-selected", "true");
  const orderedSpaces = panel.getByTestId("space-select");
  const spaceIds = await orderedSpaces.evaluateAll((items) => items.map((item) => item.getAttribute("data-space-id")));
  const primaryId = await space(page, primary.id).getAttribute("data-space-id");
  const nextIndex = (spaceIds.indexOf(primaryId) + 1) % spaceIds.length;
  await page.keyboard.press("ControlOrMeta+]");
  await expect(orderedSpaces.nth(nextIndex)).toHaveAttribute("aria-pressed", "true");
  await page.keyboard.press("ControlOrMeta+[");
  await expect(space(page, primary.id)).toHaveAttribute("aria-pressed", "true");

  await page.getByTestId("panel-toggle").click();
  await expect(panel).toHaveAttribute("data-collapsed", "true");
  await page.reload();
  await expect(panel).toHaveAttribute("data-collapsed", "true");
  await expect(page.getByRole("button", { name: "展开空间面板", exact: true })).toBeVisible();
  await screenshot(page, "desktop-collapsed-dark.png", "night");
  await page.keyboard.press("ControlOrMeta+b");
  await expect(panel).toHaveAttribute("data-collapsed", "false");
  await tab(page, first.instanceId).click();
  await screenshot(page, "desktop-dark.png", "night");
  await screenshot(page, "desktop-light.png", "ledger");

  await page.setViewportSize({ width: 400, height: 860 });
  await expect(page.getByTestId("spaces-chips")).toBeVisible();
  await expect(strip).toBeVisible();
  await expect(page.getByTestId("spaces-drawer")).toHaveCount(0);
  await screenshot(page, "phone-light.png", "ledger");
  await screenshot(page, "phone-dark.png", "night");
  await page.getByTestId("spaces-drawer-open").click();
  await expect(page.getByTestId("spaces-drawer")).toBeVisible();
  await screenshot(page, "phone-drawer-dark.png", "night");
  await space(page, secondary.id).click();
  await expect(page.getByTestId("spaces-drawer")).toHaveCount(0);
  await expect(tab(page, other.instanceId)).toHaveAttribute("aria-selected", "true");
  await expect(tab(page, first.instanceId)).toHaveCount(0);
  const dimensions = await page.evaluate(() => ({ width: window.innerWidth, content: document.documentElement.scrollWidth }));
  expect(dimensions.content).toBeLessThanOrEqual(dimensions.width);

  await page.setViewportSize({ width: 1440, height: 900 });
  await space(page, primary.id).click();
  await tab(page, second.instanceId).click();
  const closed = page.waitForResponse((response) => response.request().method() === "POST"
    && new URL(response.url()).pathname === `/v1/instances/${second.instanceId}/commands`
    && response.request().postDataJSON().operation === "instance.close");
  await tab(page, second.instanceId).locator("..").getByRole("button", { name: /^关闭会话 / }).click();
  expect((await closed).ok()).toBe(true);
  await expect(tab(page, second.instanceId)).toHaveCount(0);
  await expect(strip.getByRole("tab", { selected: true })).toHaveCount(1);
  await page.reload();
  await expect(tab(page, second.instanceId)).toHaveCount(0);
  await expect(tab(page, first.instanceId)).toBeVisible();
  await space(page, secondary.id).click();
  await expect(tab(page, other.instanceId)).toHaveAttribute("aria-selected", "true");
  expect(pageErrors).toEqual([]);
});
