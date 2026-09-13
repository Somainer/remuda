import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

const evidence = path.join(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence/spaces-1");
const realNode = process.env.HUB_E2E_EXTERNAL === "1";
type Workspace = { id: string; name: string; root: string; hostId?: string };
type RegisteredWorkspace = { workspaceId: string; hostId: string; root: string };

async function registeredSpaces(page: Page): Promise<[Workspace, Workspace]> {
  if (!realNode) return [
    { id: "wsp_e2e", name: "remuda-e2e", root: "/tmp/remuda-e2e" },
    { id: "wsp_e2e_second", name: "remuda-e2e-second", root: "/tmp/remuda-e2e-second" },
  ];
  const hostId = process.env.HUB_E2E_HOST_ID;
  expect(hostId, "HUB_E2E_HOST_ID must identify the owned remuda dev Node").toBeTruthy();
  const roots = [process.env.HUB_E2E_SPACE_PRIMARY, process.env.HUB_E2E_SPACE_SECONDARY];
  expect(roots.every(Boolean), "Set HUB_E2E_SPACE_PRIMARY and HUB_E2E_SPACE_SECONDARY to registered project roots").toBe(true);
  expect(roots[0]).not.toBe(roots[1]);
  const response = await page.request.get(`/v1/hosts/${hostId}/workspaces`);
  expect(response.ok()).toBe(true);
  const snapshot = await response.json();
  expect(snapshot.workspaceRevision).toBeGreaterThan(0);
  const workspaces: RegisteredWorkspace[] = snapshot.workspaces;
  const result = roots.map((root) => {
    const matches = workspaces.filter((row) => row.root === root);
    expect(matches, "Each project root must resolve to exactly one registered workspace").toHaveLength(1);
    const workspace = matches[0];
    expect(workspace.hostId).toBe(hostId);
    return { id: workspace.workspaceId, hostId, root: workspace.root, name: path.posix.basename(workspace.root) };
  });
  expect(result[0].id).not.toBe(result[1].id);
  return [result[0], result[1]];
}

function tab(page: Page, instanceId: string) {
  return page.getByTestId("space-tabs").getByTestId("session-tab")
    .and(page.locator(`[data-instance-id="${instanceId}"]`));
}

function closeButton(page: Page, instanceId: string) {
  return tab(page, instanceId).locator("..").getByTestId("tab-close");
}

function sidebarSession(page: Page, instanceId: string) {
  return page.getByTestId("spaces-panel").getByTestId("space-session")
    .and(page.locator(`[href="/s/${instanceId}"]`));
}

/**
 * The first session, stopped so the exited-tab and exited-group paths have a
 * real subject. The fake Node only acknowledges a close, so it never settles an
 * exited lifecycle; that mode skips these assertions rather than faking one.
 */
async function exitedInstance(page: Page, instanceId: string): Promise<string | undefined> {
  await page.request.post(`/v1/instances/${instanceId}/commands`, {
    headers: { Origin: new URL(page.url()).origin },
    data: { operation: "instance.close", payload: {} },
  });
  const settled = await expect.poll(async () => {
    const record = await page.request.get(`/v1/instances/${instanceId}`);
    return (await record.json()).lifecycle;
  }, { timeout: realNode ? 30_000 : 5_000 }).toBe("exited").then(() => true, () => false);
  return settled ? instanceId : undefined;
}

function space(page: Page, workspaceId: string) {
  return page.getByTestId("spaces-panel").getByTestId("space-select")
    .and(page.locator(`[data-space-id*='"${workspaceId}"']`));
}

async function createSession(page: Page, workspace: Workspace, prompt: string, created: string[]) {
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  let hostId = workspace.hostId;
  if (!hostId) {
    await expect(hostPicker).toContainText("e2e-fake-node", { timeout: 20_000 });
    hostId = await hostPicker.locator("option").filter({ hasText: "e2e-fake-node" }).getAttribute("value") ?? undefined;
  }
  expect(hostId).toBeTruthy();
  await hostPicker.selectOption(hostId!);
  await expect(page.getByTestId("new-session-workspace").locator(`option[value="${workspace.id}"]`))
    .toHaveCount(1, { timeout: 20_000 });
  await page.getByTestId("new-session-workspace").selectOption(workspace.id);
  if (realNode) {
    await page.getByTestId("new-session-kind-terminal").click();
    await page.getByTestId("new-session-advanced").click();
    await page.getByTestId("new-session-sheet").getByLabel("name", { exact: true }).fill(prompt);
  } else {
    await page.getByTestId("new-session-prompt").fill(prompt);
  }
  await expect(page.getByTestId("new-session-start")).toBeEnabled();
  const creating = page.waitForResponse((response) => response.request().method() === "POST"
    && new URL(response.url()).pathname === "/v1/instances");
  await page.getByTestId("new-session-start").click();
  const creation = await creating;
  expect(creation.ok()).toBe(true);
  const instanceId = (await creation.json()).instance.instanceId as string;
  expect(instanceId).toBeTruthy();
  created.push(instanceId);
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
  expect(new URL(page.url()).pathname).toBe(`/s/${instanceId}`);
  const record = await page.request.get(`/v1/instances/${instanceId}`);
  expect(record.ok()).toBe(true);
  expect(await record.json()).toMatchObject({ instanceId, hostId, workspaceId: workspace.id, cwd: workspace.root });
  if (realNode) {
    await expect(page.getByTestId("session-page")).toHaveAttribute("data-view", "tty");
    await expect(page.locator("[data-tty-lab='1']")).toHaveAttribute("data-tty-status", "live", { timeout: 30_000 });
    const terminalInput = page.getByRole("textbox", { name: "Terminal input", exact: true });
    await terminalInput.focus();
    // The shell keeps its registered cwd while replacing personal startup
    // customizations with a clean environment and clearing screen/scrollback.
    await page.keyboard.type("exec /usr/bin/env -i PATH=/usr/bin:/bin TERM=xterm-256color PS1='remuda> ' /bin/sh -c 'printf \"\\033[2J\\033[3J\\033[HSPACE_CWD=%s\\n\" \"$PWD\"; exec /bin/sh -i'");
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("tty-ansi-preview")).toContainText(`SPACE_CWD=${workspace.root}`, { timeout: 20_000 });
    expect(await (await page.request.get(`/v1/instances/${instanceId}`)).json())
      .toMatchObject({ lifecycle: "running", kind: "terminal", driver: "shell-pty" });
  } else {
    await expect(page.getByTestId("message").filter({ hasText: `echo: ${prompt}` }))
      .toBeVisible({ timeout: 20_000 });
  }
  await expect(tab(page, instanceId)).toHaveAttribute("aria-selected", "true");
  return { instanceId, hostId: hostId! };
}

async function closeCreatedSessions(page: Page, instanceIds: string[]) {
  const outcomes = await Promise.allSettled(instanceIds.map(async (instanceId) => {
    const response = await page.request.get(`/v1/instances/${instanceId}`);
    expect(response.ok()).toBe(true);
    if ((await response.json()).lifecycle !== "exited") {
      const closed = await page.request.post(`/v1/instances/${instanceId}/commands`, {
        headers: { Origin: new URL(page.url()).origin },
        data: { operation: "instance.close", payload: {} },
      });
      expect(closed.ok()).toBe(true);
    }
    await expect.poll(async () => {
      const record = await page.request.get(`/v1/instances/${instanceId}`);
      return (await record.json()).lifecycle;
    }, { timeout: 30_000 }).toBe("exited");
  }));
  expect(outcomes.filter((result) => result.status === "rejected"), "Every owned real shell session must exit").toEqual([]);
}

async function screenshot(page: Page, name: string, theme: "night" | "ledger", live = true) {
  if (realNode && live) {
    await expect(page.locator("[data-tty-lab='1']")).toHaveAttribute("data-tty-status", "live", { timeout: 30_000 });
    await expect(page.locator("[data-tty-ready]")).toHaveAttribute("data-tty-ready", "1");
    await expect(page.getByTestId("tty-ansi-preview")).toContainText("SPACE_CWD=", { timeout: 20_000 });
  }
  await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
  await page.evaluate(() => document.fonts.ready);
  await mkdir(evidence, { recursive: true });
  const image = await page.screenshot({ path: path.join(evidence, `${realNode ? "" : "fake-"}${name}`), animations: "disabled", scale: "css" });
  expect(image.byteLength, `${name} must stay below 300 KB`).toBeLessThanOrEqual(300_000);
}

test("registered spaces isolate tabs, remember selection and collapse, and fit a phone drawer", async ({ page }) => {
  test.setTimeout(realNode ? 240_000 : 120_000);
  const pageErrors: string[] = [];
  page.on("pageerror", (error) => pageErrors.push(error.message));
  await page.setViewportSize({ width: 1440, height: 900 });
  await login(page, "spaces-e2e-browser");
  const [primary, secondary] = await registeredSpaces(page);
  test.info().annotations.push({ type: "engine", description: realNode
    ? "Owned remuda dev Node: registered workspaces, native shell-pty, actual PWD and settled cleanup"
    : "Disposable real Hub: fake Node inventory and echo engine" });
  const created: string[] = [];
  try {
    const first = await createSession(page, primary, "Review the project plan", created);
    const second = await createSession(page, primary, "Check the project tests", created);
    const other = await createSession(page, secondary, "Plan the second project", created);
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
    await page.getByTestId("panel-toggle").focus();
    await page.keyboard.press("ControlOrMeta+2");
    await expect(tabs.nth(1)).toHaveAttribute("aria-selected", "true");
    await page.getByTestId("panel-toggle").focus();
    await page.keyboard.press("ControlOrMeta+1");
    await expect(tabs.first()).toHaveAttribute("aria-selected", "true");
    const orderedSpaces = panel.getByTestId("space-select");
    const spaceIds = await orderedSpaces.evaluateAll((items) => items.map((item) => item.getAttribute("data-space-id")));
    const primaryId = await space(page, primary.id).getAttribute("data-space-id");
    const nextIndex = (spaceIds.indexOf(primaryId) + 1) % spaceIds.length;
    await page.getByTestId("panel-toggle").focus();
    await page.keyboard.press("ControlOrMeta+]");
    await expect(orderedSpaces.nth(nextIndex)).toHaveAttribute("aria-pressed", "true");
    await page.getByTestId("panel-toggle").focus();
    await page.keyboard.press("ControlOrMeta+[");
    await expect(space(page, primary.id)).toHaveAttribute("aria-pressed", "true");

    await tab(page, first.instanceId).click();
    await page.getByTestId("panel-toggle").click();
    await expect(panel).toHaveAttribute("data-collapsed", "true");
    await page.reload();
    await expect(panel).toHaveAttribute("data-collapsed", "true");
    await expect(page.getByRole("button", { name: "展开空间面板", exact: true })).toBeVisible();
    await screenshot(page, "desktop-collapsed-dark.png", "night");
    await page.getByTestId("panel-toggle").focus();
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

    // Dismissing a running tab must not send a close command: the session
    // keeps running and only this device's tab goes away.
    const commands: string[] = [];
    const watchCommands = (request: import("@playwright/test").Request) => {
      if (request.method() === "POST" && new URL(request.url()).pathname === `/v1/instances/${second.instanceId}/commands`) {
        commands.push(request.postDataJSON().operation);
      }
    };
    page.on("request", watchCommands);
    await closeButton(page, second.instanceId).click();
    await expect(page.getByTestId("tab-close-sheet")).toBeVisible();
    await screenshot(page, "desktop-close-sheet-dark.png", "night");
    await page.getByTestId("tab-close-keep").click();
    await expect(tab(page, second.instanceId)).toHaveCount(0);
    await expect(strip.getByRole("tab", { selected: true })).toHaveCount(1);
    await page.reload();
    await expect(tab(page, second.instanceId)).toHaveCount(0);
    await expect(tab(page, first.instanceId)).toBeVisible();
    expect(commands, "仅关闭标签 must not stop the session").toEqual([]);
    expect((await (await page.request.get(`/v1/instances/${second.instanceId}`)).json()).lifecycle)
      .not.toBe("exited");

    // Clicking the dismissed session in the sidebar re-opens its tab.
    await sidebarSession(page, second.instanceId).click();
    await expect(tab(page, second.instanceId)).toHaveAttribute("aria-selected", "true");

    // Stopping through the same sheet is a separate, explicit choice.
    await closeButton(page, second.instanceId).click();
    const stopped = page.waitForResponse((response) => response.request().method() === "POST"
      && new URL(response.url()).pathname === `/v1/instances/${second.instanceId}/commands`
      && response.request().postDataJSON().operation === "instance.close");
    await page.getByTestId("tab-close-stop").click();
    expect((await stopped).ok()).toBe(true);
    page.off("request", watchCommands);
    await expect(tab(page, second.instanceId)).toHaveCount(0);
    expect(commands).toEqual(["instance.close"]);

    // An exited tab closes with one click and no sheet at all.
    const exited = await exitedInstance(page, first.instanceId);
    if (exited) {
      await expect(tab(page, exited)).toBeVisible();
      await closeButton(page, exited).click();
      await expect(page.getByTestId("tab-close-sheet")).toHaveCount(0);
      await expect(tab(page, exited)).toHaveCount(0);

      // The sidebar keeps it in its own collapsed 已退出 group.
      const group = panel.getByTestId("exited-toggle").first();
      await expect(group).toContainText("已退出");
      await expect(group).toHaveAttribute("aria-expanded", "false");
      await group.click();
      await expect(panel.getByTestId("exited-session").and(page.locator(`[data-instance-id="${exited}"]`))).toBeVisible();
      await screenshot(page, "desktop-exited-group-dark.png", "night", false);
      await expect(panel.getByTestId("exited-resume").first()).toBeVisible();
      await panel.getByTestId("exited-delete").first().click();
      await expect(page.getByTestId("delete-session-sheet")).toContainText("删除会话及其记录？");
      await page.getByTestId("delete-session-sheet-cancel").click();
      await expect(page.getByTestId("delete-session-sheet")).toHaveCount(0);
      // Cancelling leaves the record untouched.
      expect((await page.request.get(`/v1/instances/${exited}`)).ok()).toBe(true);
    }

    await space(page, secondary.id).click();
    await expect(tab(page, other.instanceId)).toHaveAttribute("aria-selected", "true");
    expect(pageErrors).toEqual([]);
  } finally {
    if (realNode) await closeCreatedSessions(page, created);
  }
});
