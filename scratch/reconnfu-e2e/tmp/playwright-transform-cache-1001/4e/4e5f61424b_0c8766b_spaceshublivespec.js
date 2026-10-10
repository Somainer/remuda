// 46d1fe4cc0b4cff3bc9d986f1603c35818cac40a
import { expect, test } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { setMode } from "./appearanceHelper";
import { login } from "./hub-auth";

// Committed evidence is refreshed only on request (REMUDA_EVIDENCE=1); every other run —
// including the merge gate, whose verify-tree step rejects a dirty worktree — writes
// the same screenshots under the gitignored test-results/ instead.
const evidence = process.env.REMUDA_EVIDENCE === "1" ? path.join(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence/spaces-1") : path.join(path.dirname(fileURLToPath(import.meta.url)), "../../test-results/evidence/spaces-1");
const realNode = process.env.HUB_E2E_EXTERNAL === "1";
async function registeredSpaces(page) {
  if (!realNode) return [{
    id: "wsp_e2e",
    name: "remuda-e2e",
    root: "/tmp/remuda-e2e"
  }, {
    id: "wsp_e2e_second",
    name: "remuda-e2e-second",
    root: "/tmp/remuda-e2e-second"
  }];
  const hostId = process.env.HUB_E2E_HOST_ID;
  expect(hostId, "HUB_E2E_HOST_ID must identify the owned remuda dev Node").toBeTruthy();
  const roots = [process.env.HUB_E2E_SPACE_PRIMARY, process.env.HUB_E2E_SPACE_SECONDARY];
  expect(roots.every(Boolean), "Set HUB_E2E_SPACE_PRIMARY and HUB_E2E_SPACE_SECONDARY to registered project roots").toBe(true);
  expect(roots[0]).not.toBe(roots[1]);
  const response = await page.request.get(`/v1/hosts/${hostId}/workspaces`);
  expect(response.ok()).toBe(true);
  const snapshot = await response.json();
  expect(snapshot.workspaceRevision).toBeGreaterThan(0);
  const workspaces = snapshot.workspaces;
  const result = roots.map(root => {
    const matches = workspaces.filter(row => row.root === root);
    expect(matches, "Each project root must resolve to exactly one registered workspace").toHaveLength(1);
    const workspace = matches[0];
    expect(workspace.hostId).toBe(hostId);
    return {
      id: workspace.workspaceId,
      hostId,
      root: workspace.root,
      name: path.posix.basename(workspace.root)
    };
  });
  expect(result[0].id).not.toBe(result[1].id);
  return [result[0], result[1]];
}
function tab(page, instanceId) {
  return page.getByTestId("space-tabs").getByTestId("session-tab").and(page.locator(`[data-instance-id="${instanceId}"]`));
}
function closeButton(page, instanceId) {
  return tab(page, instanceId).locator("..").getByTestId("tab-close");
}
function sidebarSession(page, instanceId) {
  return page.getByTestId("spaces-panel").getByTestId("space-session").and(page.locator(`[href="/s/${instanceId}"]`));
}

/**
 * The first session, stopped so the exited-tab and exited-group paths have a
 * real subject. The fake Node only acknowledges a close, so it never settles an
 * exited lifecycle; that mode skips these assertions rather than faking one.
 */
async function exitedInstance(page, instanceId) {
  await page.request.post(`/v1/instances/${instanceId}/commands`, {
    headers: {
      Origin: new URL(page.url()).origin
    },
    data: {
      operation: "instance.close",
      payload: {}
    }
  });
  const settled = await expect.poll(async () => {
    const record = await page.request.get(`/v1/instances/${instanceId}`);
    return (await record.json()).lifecycle;
  }, {
    timeout: realNode ? 30000 : 5000
  }).toBe("exited").then(() => true, () => false);
  return settled ? instanceId : undefined;
}
function space(page, workspaceId) {
  return page.getByTestId("spaces-panel").getByTestId("space-select").and(page.locator(`[data-space-id*='"${workspaceId}"']`));
}

/** UO-2a: the Space index lives on /sessions, reached in-app from the sidebar. */
async function toList(page) {
  await page.getByRole("navigation", {
    name: "主导航"
  }).getByRole("link", {
    name: "会话"
  }).click();
  await expect(page).toHaveURL(/\/sessions$/);
}
async function createSession(page, workspace, prompt, created) {
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  let hostId = workspace.hostId;
  if (!hostId) {
    var _await$hostPicker$loc;
    await expect(hostPicker).toContainText("e2e-fake-node", {
      timeout: 20000
    });
    hostId = (_await$hostPicker$loc = await hostPicker.locator("option").filter({
      hasText: "e2e-fake-node"
    }).getAttribute("value")) !== null && _await$hostPicker$loc !== void 0 ? _await$hostPicker$loc : undefined;
  }
  expect(hostId).toBeTruthy();
  await hostPicker.selectOption(hostId);
  await expect(page.getByTestId("new-session-workspace").locator(`option[value="${workspace.id}"]`)).toHaveCount(1, {
    timeout: 20000
  });
  await page.getByTestId("new-session-workspace").selectOption(workspace.id);
  if (realNode) {
    await page.getByTestId("new-session-kind-terminal").click();
    await page.getByTestId("new-session-advanced").click();
    await page.getByTestId("new-session-sheet").getByLabel("name", {
      exact: true
    }).fill(prompt);
  } else {
    await page.getByTestId("new-session-prompt").fill(prompt);
  }
  await expect(page.getByTestId("new-session-start")).toBeEnabled();
  const creating = page.waitForResponse(response => response.request().method() === "POST" && new URL(response.url()).pathname === "/v1/instances");
  await page.getByTestId("new-session-start").click();
  const creation = await creating;
  expect(creation.ok()).toBe(true);
  const instanceId = (await creation.json()).instance.instanceId;
  expect(instanceId).toBeTruthy();
  created.push(instanceId);
  await expect(page).toHaveURL(/\/s\//, {
    timeout: 20000
  });
  expect(new URL(page.url()).pathname).toBe(`/s/${instanceId}`);
  const record = await page.request.get(`/v1/instances/${instanceId}`);
  expect(record.ok()).toBe(true);
  expect(await record.json()).toMatchObject({
    instanceId,
    hostId,
    workspaceId: workspace.id,
    cwd: workspace.root
  });
  if (realNode) {
    await expect(page.getByTestId("session-page")).toHaveAttribute("data-view", "tty");
    await expect(page.locator("[data-tty-lab='1']")).toHaveAttribute("data-tty-status", "live", {
      timeout: 30000
    });
    const terminalInput = page.getByRole("textbox", {
      name: "Terminal input",
      exact: true
    });
    await terminalInput.focus();
    // The shell keeps its registered cwd while replacing personal startup
    // customizations with a clean environment and clearing screen/scrollback.
    await page.keyboard.type("exec /usr/bin/env -i PATH=/usr/bin:/bin TERM=xterm-256color PS1='remuda> ' /bin/sh -c 'printf \"\\033[2J\\033[3J\\033[HSPACE_CWD=%s\\n\" \"$PWD\"; exec /bin/sh -i'");
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("tty-ansi-preview")).toContainText(`SPACE_CWD=${workspace.root}`, {
      timeout: 20000
    });
    expect(await (await page.request.get(`/v1/instances/${instanceId}`)).json()).toMatchObject({
      lifecycle: "running",
      kind: "terminal",
      driver: "shell-pty"
    });
  } else {
    await expect(page.getByTestId("message").filter({
      hasText: `echo: ${prompt}`
    })).toBeVisible({
      timeout: 20000
    });
  }
  await expect(tab(page, instanceId)).toHaveAttribute("aria-selected", "true");
  return {
    instanceId,
    hostId: hostId
  };
}
async function closeCreatedSessions(page, instanceIds) {
  const outcomes = await Promise.allSettled(instanceIds.map(async instanceId => {
    const response = await page.request.get(`/v1/instances/${instanceId}`);
    expect(response.ok()).toBe(true);
    if ((await response.json()).lifecycle !== "exited") {
      const closed = await page.request.post(`/v1/instances/${instanceId}/commands`, {
        headers: {
          Origin: new URL(page.url()).origin
        },
        data: {
          operation: "instance.close",
          payload: {}
        }
      });
      expect(closed.ok()).toBe(true);
    }
    await expect.poll(async () => {
      const record = await page.request.get(`/v1/instances/${instanceId}`);
      return (await record.json()).lifecycle;
    }, {
      timeout: 30000
    }).toBe("exited");
  }));
  expect(outcomes.filter(result => result.status === "rejected"), "Every owned real shell session must exit").toEqual([]);
}
async function screenshot(page, name, theme) {
  if (realNode) {
    await expect(page.locator("[data-tty-lab='1']")).toHaveAttribute("data-tty-status", "live", {
      timeout: 30000
    });
    await expect(page.locator("[data-tty-ready]")).toHaveAttribute("data-tty-ready", "1");
    await expect(page.getByTestId("tty-ansi-preview")).toContainText("SPACE_CWD=", {
      timeout: 20000
    });
  }
  await setMode(page, theme);
  await page.evaluate(() => document.fonts.ready);
  await mkdir(evidence, {
    recursive: true
  });
  const image = await page.screenshot({
    path: path.join(evidence, `${realNode ? "" : "fake-"}${name}`),
    animations: "disabled",
    scale: "css"
  });
  expect(image.byteLength, `${name} must stay below 300 KB`).toBeLessThanOrEqual(300000);
}
test("registered spaces isolate tabs, remember selection and collapse, and fit a phone drawer", async ({
  page
}) => {
  test.setTimeout(realNode ? 240000 : 120000);
  const pageErrors = [];
  page.on("pageerror", error => pageErrors.push(error.message));
  await page.setViewportSize({
    width: 1440,
    height: 900
  });
  await login(page, "spaces-e2e-browser");
  const [primary, secondary] = await registeredSpaces(page);
  test.info().annotations.push({
    type: "engine",
    description: realNode ? "Owned remuda dev Node: registered workspaces, native shell-pty, actual PWD and settled cleanup" : "Disposable real Hub: fake Node inventory and echo engine"
  });
  const created = [];
  try {
    const first = await createSession(page, primary, "Review the project plan", created);
    const second = await createSession(page, primary, "Check the project tests", created);
    const other = await createSession(page, secondary, "Plan the second project", created);
    const panel = page.getByTestId("spaces-panel");
    const strip = page.getByTestId("space-tabs");
    await expect(tab(page, first.instanceId)).toHaveCount(0);
    await expect(tab(page, second.instanceId)).toHaveCount(0);
    // UO-2a: the Space index lives on /sessions (no strip there); the strip
    // belongs to /s/*.
    await toList(page);
    await expect(strip).toHaveCount(0);
    await space(page, primary.id).click();
    await expect(space(page, primary.id)).toHaveAttribute("aria-pressed", "true");
    await expect(page).toHaveURL(/\/sessions$/);
    await sidebarSession(page, second.instanceId).click();
    await expect(tab(page, second.instanceId)).toHaveAttribute("aria-selected", "true");
    await expect(tab(page, first.instanceId)).toBeVisible();
    await expect(tab(page, other.instanceId)).toHaveCount(0);
    await tab(page, first.instanceId).click();
    await toList(page);
    await expect(space(page, secondary.id)).toBeVisible();
    const orderedSpaces = panel.getByTestId("space-select");
    const spaceIds = await orderedSpaces.evaluateAll(items => items.map(item => item.getAttribute("data-space-id")));
    const primaryId = await space(page, primary.id).getAttribute("data-space-id");
    const secondaryId = await space(page, secondary.id).getAttribute("data-space-id");
    await space(page, secondary.id).click();
    await sidebarSession(page, other.instanceId).click();
    await expect(tab(page, other.instanceId)).toHaveAttribute("aria-selected", "true");
    // Switching Spaces by keyboard lands on the Space's remembered tab. Other
    // live specs add Spaces, so walk the shorter way round the rendered order.
    const forward = (spaceIds.indexOf(primaryId) - spaceIds.indexOf(secondaryId) + spaceIds.length) % spaceIds.length;
    const [key, steps] = forward <= spaceIds.length / 2 ? ["ControlOrMeta+]", forward] : ["ControlOrMeta+[", spaceIds.length - forward];
    for (let step = 0; step < steps; step++) {
      await page.getByTestId("sidebar-toggle").focus();
      await page.keyboard.press(key);
    }
    await expect(page).toHaveURL(new RegExp(`/s/${first.instanceId}$`));
    await expect(tab(page, first.instanceId)).toHaveAttribute("aria-selected", "true");

    // A deep link restores both project and tab, including after a fresh load.
    await page.goto(`/s/${other.instanceId}`);
    await expect(tab(page, other.instanceId)).toHaveAttribute("aria-selected", "true");
    await expect(tab(page, first.instanceId)).toHaveCount(0);
    await page.reload();
    await expect(tab(page, other.instanceId)).toHaveAttribute("aria-selected", "true");

    // The global New action inherits the selected project rather than another
    // project's last successful creation preferences.
    await toList(page);
    await space(page, primary.id).click();
    await page.getByTitle("新建", {
      exact: true
    }).click();
    await expect(page.getByTestId("new-session-host")).toHaveValue(first.hostId);
    await expect(page.getByTestId("new-session-workspace")).toHaveValue(primary.id);
    await expect(page.getByTestId("new-session-workspace")).toContainText(primary.root);
    await page.getByTestId("new-session-sheet").getByRole("button", {
      name: "关闭",
      exact: true
    }).click();
    await toList(page);
    await space(page, primary.id).click();
    await sidebarSession(page, first.instanceId).click();

    // Use the rendered order so legacy sessions from the other live spec do not
    // make numeric keyboard shortcuts depend on the fixture's session count.
    const tabs = strip.getByRole("tab");
    await tabs.first().click();
    await page.getByTestId("sidebar-toggle").focus();
    await page.keyboard.press("ControlOrMeta+2");
    await expect(tabs.nth(1)).toHaveAttribute("aria-selected", "true");
    await page.getByTestId("sidebar-toggle").focus();
    await page.keyboard.press("ControlOrMeta+1");
    await expect(tabs.first()).toHaveAttribute("aria-selected", "true");
    const nextIndex = (spaceIds.indexOf(primaryId) + 1) % spaceIds.length;
    await page.getByTestId("sidebar-toggle").focus();
    await page.keyboard.press("ControlOrMeta+]");
    await toList(page);
    await expect(orderedSpaces.nth(nextIndex)).toHaveAttribute("aria-pressed", "true");
    await page.getByTestId("sidebar-toggle").focus();
    await page.keyboard.press("ControlOrMeta+[");
    await toList(page);
    await expect(space(page, primary.id)).toHaveAttribute("aria-pressed", "true");

    // ⌘/Ctrl+B folds the sidebar (prefs.collapsed) on every desktop route.
    await sidebarSession(page, first.instanceId).click();
    await expect(tab(page, first.instanceId)).toHaveAttribute("aria-selected", "true");
    const sidebar = page.getByTestId("sidebar");
    await page.getByTestId("sidebar-toggle").click();
    await expect(sidebar).toHaveAttribute("data-collapsed", "true");
    await page.reload();
    await expect(sidebar).toHaveAttribute("data-collapsed", "true");
    await expect(page.getByRole("button", {
      name: "展开侧栏",
      exact: true
    })).toBeVisible();
    await screenshot(page, "desktop-collapsed-dark.png", "night");
    await page.getByTestId("sidebar-toggle").focus();
    await page.keyboard.press("ControlOrMeta+b");
    await expect(sidebar).toHaveAttribute("data-collapsed", "false");
    await tab(page, first.instanceId).click();
    await screenshot(page, "desktop-dark.png", "night");
    await screenshot(page, "desktop-light.png", "ledger");
    await page.setViewportSize({
      width: 400,
      height: 860
    });
    // D-049: on the compact /s/:id route the full chips strip is folded into
    // the one header space chip (same spaces-chips wrapper testid) and the
    // SpaceTabs row does not render at all; the header chip's drawer keeps
    // the switching capability.
    await expect(page.getByTestId("spaces-chips")).toBeVisible();
    await expect(strip).toHaveCount(0);
    await expect(page.getByTestId("spaces-drawer")).toHaveCount(0);
    await screenshot(page, "phone-light.png", "ledger");
    await screenshot(page, "phone-dark.png", "night");
    await page.getByTestId("spaces-drawer-open").click();
    await expect(page.getByTestId("spaces-drawer")).toBeVisible();
    await screenshot(page, "phone-drawer-dark.png", "night");
    await space(page, secondary.id).click();
    await expect(page.getByTestId("spaces-drawer")).toHaveCount(0);
    // The drawer replaces the tabs row as the switcher: picking the other
    // project navigates to its active session, and the tabs row stays gone
    // on the compact session route (isolation still holds via the URL).
    await expect(page).toHaveURL(new RegExp(`/s/${other.instanceId}(?:$|[/?])`));
    await expect(strip).toHaveCount(0);
    const dimensions = await page.evaluate(() => ({
      width: window.innerWidth,
      content: document.documentElement.scrollWidth
    }));
    expect(dimensions.content).toBeLessThanOrEqual(dimensions.width);
    await page.setViewportSize({
      width: 1440,
      height: 900
    });
    await toList(page);
    await space(page, primary.id).click();
    await sidebarSession(page, second.instanceId).click();
    await expect(tab(page, second.instanceId)).toHaveAttribute("aria-selected", "true");

    // Dismissing a running tab must not send a close command: the session
    // keeps running and only this device's tab goes away.
    const commands = [];
    const watchCommands = request => {
      if (request.method() === "POST" && new URL(request.url()).pathname === `/v1/instances/${second.instanceId}/commands`) {
        commands.push(request.postDataJSON().operation);
      }
    };
    page.on("request", watchCommands);
    await closeButton(page, second.instanceId).click();
    await expect(page.getByTestId("tab-close-sheet")).toBeVisible();
    await page.getByTestId("tab-close-keep").click();
    await expect(tab(page, second.instanceId)).toHaveCount(0);
    await expect(strip.getByRole("tab", {
      selected: true
    })).toHaveCount(1);
    await page.reload();
    await expect(tab(page, second.instanceId)).toHaveCount(0);
    await expect(tab(page, first.instanceId)).toBeVisible();
    expect(commands, "仅关闭标签 must not stop the session").toEqual([]);
    expect((await (await page.request.get(`/v1/instances/${second.instanceId}`)).json()).lifecycle).not.toBe("exited");

    // Clicking the dismissed session in the /sessions index re-opens its tab.
    await toList(page);
    await sidebarSession(page, second.instanceId).click();
    await expect(tab(page, second.instanceId)).toHaveAttribute("aria-selected", "true");

    // Stopping through the same sheet is a separate, explicit choice.
    await closeButton(page, second.instanceId).click();
    const stopped = page.waitForResponse(response => response.request().method() === "POST" && new URL(response.url()).pathname === `/v1/instances/${second.instanceId}/commands` && response.request().postDataJSON().operation === "instance.close");
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

      // The /sessions index keeps it in its own collapsed 已退出 group.
      await toList(page);
      const group = panel.getByTestId("exited-toggle").first();
      await expect(group).toContainText("已退出");
      await expect(group).toHaveAttribute("aria-expanded", "false");
      await group.click();
      await expect(panel.getByTestId("exited-session").and(page.locator(`[data-instance-id="${exited}"]`))).toBeVisible();
      await expect(panel.getByTestId("exited-resume").first()).toBeVisible();
      await panel.getByTestId("exited-delete").first().click();
      await expect(page.getByTestId("delete-session-sheet")).toContainText("删除会话及其记录？");
      await page.getByTestId("delete-session-sheet-cancel").click();
      await expect(page.getByTestId("delete-session-sheet")).toHaveCount(0);
      // Cancelling leaves the record untouched.
      expect((await page.request.get(`/v1/instances/${exited}`)).ok()).toBe(true);
    }
    await toList(page);
    await space(page, secondary.id).click();
    await sidebarSession(page, other.instanceId).click();
    await expect(tab(page, other.instanceId)).toHaveAttribute("aria-selected", "true");
    expect(pageErrors).toEqual([]);
  } finally {
    if (realNode) await closeCreatedSessions(page, created);
  }
});
//# sourceMappingURL=data:application/json;charset=utf-8;base64,eyJ2ZXJzaW9uIjozLCJuYW1lcyI6WyJleHBlY3QiLCJ0ZXN0IiwibWtkaXIiLCJwYXRoIiwiZmlsZVVSTFRvUGF0aCIsInNldE1vZGUiLCJsb2dpbiIsImV2aWRlbmNlIiwicHJvY2VzcyIsImVudiIsIlJFTVVEQV9FVklERU5DRSIsImpvaW4iLCJkaXJuYW1lIiwiaW1wb3J0IiwibWV0YSIsInVybCIsInJlYWxOb2RlIiwiSFVCX0UyRV9FWFRFUk5BTCIsInJlZ2lzdGVyZWRTcGFjZXMiLCJwYWdlIiwiaWQiLCJuYW1lIiwicm9vdCIsImhvc3RJZCIsIkhVQl9FMkVfSE9TVF9JRCIsInRvQmVUcnV0aHkiLCJyb290cyIsIkhVQl9FMkVfU1BBQ0VfUFJJTUFSWSIsIkhVQl9FMkVfU1BBQ0VfU0VDT05EQVJZIiwiZXZlcnkiLCJCb29sZWFuIiwidG9CZSIsIm5vdCIsInJlc3BvbnNlIiwicmVxdWVzdCIsImdldCIsIm9rIiwic25hcHNob3QiLCJqc29uIiwid29ya3NwYWNlUmV2aXNpb24iLCJ0b0JlR3JlYXRlclRoYW4iLCJ3b3Jrc3BhY2VzIiwicmVzdWx0IiwibWFwIiwibWF0Y2hlcyIsImZpbHRlciIsInJvdyIsInRvSGF2ZUxlbmd0aCIsIndvcmtzcGFjZSIsIndvcmtzcGFjZUlkIiwicG9zaXgiLCJiYXNlbmFtZSIsInRhYiIsImluc3RhbmNlSWQiLCJnZXRCeVRlc3RJZCIsImFuZCIsImxvY2F0b3IiLCJjbG9zZUJ1dHRvbiIsInNpZGViYXJTZXNzaW9uIiwiZXhpdGVkSW5zdGFuY2UiLCJwb3N0IiwiaGVhZGVycyIsIk9yaWdpbiIsIlVSTCIsIm9yaWdpbiIsImRhdGEiLCJvcGVyYXRpb24iLCJwYXlsb2FkIiwic2V0dGxlZCIsInBvbGwiLCJyZWNvcmQiLCJsaWZlY3ljbGUiLCJ0aW1lb3V0IiwidGhlbiIsInVuZGVmaW5lZCIsInNwYWNlIiwidG9MaXN0IiwiZ2V0QnlSb2xlIiwiY2xpY2siLCJ0b0hhdmVVUkwiLCJjcmVhdGVTZXNzaW9uIiwicHJvbXB0IiwiY3JlYXRlZCIsImdvdG8iLCJob3N0UGlja2VyIiwiX2F3YWl0JGhvc3RQaWNrZXIkbG9jIiwidG9Db250YWluVGV4dCIsImhhc1RleHQiLCJnZXRBdHRyaWJ1dGUiLCJzZWxlY3RPcHRpb24iLCJ0b0hhdmVDb3VudCIsImdldEJ5TGFiZWwiLCJleGFjdCIsImZpbGwiLCJ0b0JlRW5hYmxlZCIsImNyZWF0aW5nIiwid2FpdEZvclJlc3BvbnNlIiwibWV0aG9kIiwicGF0aG5hbWUiLCJjcmVhdGlvbiIsImluc3RhbmNlIiwicHVzaCIsInRvTWF0Y2hPYmplY3QiLCJjd2QiLCJ0b0hhdmVBdHRyaWJ1dGUiLCJ0ZXJtaW5hbElucHV0IiwiZm9jdXMiLCJrZXlib2FyZCIsInR5cGUiLCJwcmVzcyIsImtpbmQiLCJkcml2ZXIiLCJ0b0JlVmlzaWJsZSIsImNsb3NlQ3JlYXRlZFNlc3Npb25zIiwiaW5zdGFuY2VJZHMiLCJvdXRjb21lcyIsIlByb21pc2UiLCJhbGxTZXR0bGVkIiwiY2xvc2VkIiwic3RhdHVzIiwidG9FcXVhbCIsInNjcmVlbnNob3QiLCJ0aGVtZSIsImV2YWx1YXRlIiwiZG9jdW1lbnQiLCJmb250cyIsInJlYWR5IiwicmVjdXJzaXZlIiwiaW1hZ2UiLCJhbmltYXRpb25zIiwic2NhbGUiLCJieXRlTGVuZ3RoIiwidG9CZUxlc3NUaGFuT3JFcXVhbCIsInNldFRpbWVvdXQiLCJwYWdlRXJyb3JzIiwib24iLCJlcnJvciIsIm1lc3NhZ2UiLCJzZXRWaWV3cG9ydFNpemUiLCJ3aWR0aCIsImhlaWdodCIsInByaW1hcnkiLCJzZWNvbmRhcnkiLCJpbmZvIiwiYW5ub3RhdGlvbnMiLCJkZXNjcmlwdGlvbiIsImZpcnN0Iiwic2Vjb25kIiwib3RoZXIiLCJwYW5lbCIsInN0cmlwIiwib3JkZXJlZFNwYWNlcyIsInNwYWNlSWRzIiwiZXZhbHVhdGVBbGwiLCJpdGVtcyIsIml0ZW0iLCJwcmltYXJ5SWQiLCJzZWNvbmRhcnlJZCIsImZvcndhcmQiLCJpbmRleE9mIiwibGVuZ3RoIiwia2V5Iiwic3RlcHMiLCJzdGVwIiwiUmVnRXhwIiwicmVsb2FkIiwiZ2V0QnlUaXRsZSIsInRvSGF2ZVZhbHVlIiwidGFicyIsIm50aCIsIm5leHRJbmRleCIsInNpZGViYXIiLCJkaW1lbnNpb25zIiwid2luZG93IiwiaW5uZXJXaWR0aCIsImNvbnRlbnQiLCJkb2N1bWVudEVsZW1lbnQiLCJzY3JvbGxXaWR0aCIsImNvbW1hbmRzIiwid2F0Y2hDb21tYW5kcyIsInBvc3REYXRhSlNPTiIsInNlbGVjdGVkIiwic3RvcHBlZCIsIm9mZiIsImV4aXRlZCIsImdyb3VwIl0sInNvdXJjZXMiOlsic3BhY2VzLWh1Yi1saXZlLnNwZWMudHMiXSwic291cmNlc0NvbnRlbnQiOlsiaW1wb3J0IHsgZXhwZWN0LCB0ZXN0LCB0eXBlIFBhZ2UgfSBmcm9tIFwiQHBsYXl3cmlnaHQvdGVzdFwiO1xuaW1wb3J0IHsgbWtkaXIgfSBmcm9tIFwibm9kZTpmcy9wcm9taXNlc1wiO1xuaW1wb3J0IHBhdGggZnJvbSBcIm5vZGU6cGF0aFwiO1xuaW1wb3J0IHsgZmlsZVVSTFRvUGF0aCB9IGZyb20gXCJub2RlOnVybFwiO1xuaW1wb3J0IHsgc2V0TW9kZSB9IGZyb20gXCIuL2FwcGVhcmFuY2VIZWxwZXJcIjtcbmltcG9ydCB7IGxvZ2luIH0gZnJvbSBcIi4vaHViLWF1dGhcIjtcblxuLy8gQ29tbWl0dGVkIGV2aWRlbmNlIGlzIHJlZnJlc2hlZCBvbmx5IG9uIHJlcXVlc3QgKFJFTVVEQV9FVklERU5DRT0xKTsgZXZlcnkgb3RoZXIgcnVuIOKAlFxuLy8gaW5jbHVkaW5nIHRoZSBtZXJnZSBnYXRlLCB3aG9zZSB2ZXJpZnktdHJlZSBzdGVwIHJlamVjdHMgYSBkaXJ0eSB3b3JrdHJlZSDigJQgd3JpdGVzXG4vLyB0aGUgc2FtZSBzY3JlZW5zaG90cyB1bmRlciB0aGUgZ2l0aWdub3JlZCB0ZXN0LXJlc3VsdHMvIGluc3RlYWQuXG5jb25zdCBldmlkZW5jZSA9IHByb2Nlc3MuZW52LlJFTVVEQV9FVklERU5DRSA9PT0gXCIxXCJcbiAgPyBwYXRoLmpvaW4ocGF0aC5kaXJuYW1lKGZpbGVVUkxUb1BhdGgoaW1wb3J0Lm1ldGEudXJsKSksIFwiLi4vLi4vLi4vZG9jcy9kZXNpZ24vZXZpZGVuY2Uvc3BhY2VzLTFcIilcbiAgOiBwYXRoLmpvaW4ocGF0aC5kaXJuYW1lKGZpbGVVUkxUb1BhdGgoaW1wb3J0Lm1ldGEudXJsKSksIFwiLi4vLi4vdGVzdC1yZXN1bHRzL2V2aWRlbmNlL3NwYWNlcy0xXCIpO1xuY29uc3QgcmVhbE5vZGUgPSBwcm9jZXNzLmVudi5IVUJfRTJFX0VYVEVSTkFMID09PSBcIjFcIjtcbnR5cGUgV29ya3NwYWNlID0geyBpZDogc3RyaW5nOyBuYW1lOiBzdHJpbmc7IHJvb3Q6IHN0cmluZzsgaG9zdElkPzogc3RyaW5nIH07XG50eXBlIFJlZ2lzdGVyZWRXb3Jrc3BhY2UgPSB7IHdvcmtzcGFjZUlkOiBzdHJpbmc7IGhvc3RJZDogc3RyaW5nOyByb290OiBzdHJpbmcgfTtcblxuYXN5bmMgZnVuY3Rpb24gcmVnaXN0ZXJlZFNwYWNlcyhwYWdlOiBQYWdlKTogUHJvbWlzZTxbV29ya3NwYWNlLCBXb3Jrc3BhY2VdPiB7XG4gIGlmICghcmVhbE5vZGUpIHJldHVybiBbXG4gICAgeyBpZDogXCJ3c3BfZTJlXCIsIG5hbWU6IFwicmVtdWRhLWUyZVwiLCByb290OiBcIi90bXAvcmVtdWRhLWUyZVwiIH0sXG4gICAgeyBpZDogXCJ3c3BfZTJlX3NlY29uZFwiLCBuYW1lOiBcInJlbXVkYS1lMmUtc2Vjb25kXCIsIHJvb3Q6IFwiL3RtcC9yZW11ZGEtZTJlLXNlY29uZFwiIH0sXG4gIF07XG4gIGNvbnN0IGhvc3RJZCA9IHByb2Nlc3MuZW52LkhVQl9FMkVfSE9TVF9JRDtcbiAgZXhwZWN0KGhvc3RJZCwgXCJIVUJfRTJFX0hPU1RfSUQgbXVzdCBpZGVudGlmeSB0aGUgb3duZWQgcmVtdWRhIGRldiBOb2RlXCIpLnRvQmVUcnV0aHkoKTtcbiAgY29uc3Qgcm9vdHMgPSBbcHJvY2Vzcy5lbnYuSFVCX0UyRV9TUEFDRV9QUklNQVJZLCBwcm9jZXNzLmVudi5IVUJfRTJFX1NQQUNFX1NFQ09OREFSWV07XG4gIGV4cGVjdChyb290cy5ldmVyeShCb29sZWFuKSwgXCJTZXQgSFVCX0UyRV9TUEFDRV9QUklNQVJZIGFuZCBIVUJfRTJFX1NQQUNFX1NFQ09OREFSWSB0byByZWdpc3RlcmVkIHByb2plY3Qgcm9vdHNcIikudG9CZSh0cnVlKTtcbiAgZXhwZWN0KHJvb3RzWzBdKS5ub3QudG9CZShyb290c1sxXSk7XG4gIGNvbnN0IHJlc3BvbnNlID0gYXdhaXQgcGFnZS5yZXF1ZXN0LmdldChgL3YxL2hvc3RzLyR7aG9zdElkfS93b3Jrc3BhY2VzYCk7XG4gIGV4cGVjdChyZXNwb25zZS5vaygpKS50b0JlKHRydWUpO1xuICBjb25zdCBzbmFwc2hvdCA9IGF3YWl0IHJlc3BvbnNlLmpzb24oKTtcbiAgZXhwZWN0KHNuYXBzaG90LndvcmtzcGFjZVJldmlzaW9uKS50b0JlR3JlYXRlclRoYW4oMCk7XG4gIGNvbnN0IHdvcmtzcGFjZXM6IFJlZ2lzdGVyZWRXb3Jrc3BhY2VbXSA9IHNuYXBzaG90LndvcmtzcGFjZXM7XG4gIGNvbnN0IHJlc3VsdCA9IHJvb3RzLm1hcCgocm9vdCkgPT4ge1xuICAgIGNvbnN0IG1hdGNoZXMgPSB3b3Jrc3BhY2VzLmZpbHRlcigocm93KSA9PiByb3cucm9vdCA9PT0gcm9vdCk7XG4gICAgZXhwZWN0KG1hdGNoZXMsIFwiRWFjaCBwcm9qZWN0IHJvb3QgbXVzdCByZXNvbHZlIHRvIGV4YWN0bHkgb25lIHJlZ2lzdGVyZWQgd29ya3NwYWNlXCIpLnRvSGF2ZUxlbmd0aCgxKTtcbiAgICBjb25zdCB3b3Jrc3BhY2UgPSBtYXRjaGVzWzBdO1xuICAgIGV4cGVjdCh3b3Jrc3BhY2UuaG9zdElkKS50b0JlKGhvc3RJZCk7XG4gICAgcmV0dXJuIHsgaWQ6IHdvcmtzcGFjZS53b3Jrc3BhY2VJZCwgaG9zdElkLCByb290OiB3b3Jrc3BhY2Uucm9vdCwgbmFtZTogcGF0aC5wb3NpeC5iYXNlbmFtZSh3b3Jrc3BhY2Uucm9vdCkgfTtcbiAgfSk7XG4gIGV4cGVjdChyZXN1bHRbMF0uaWQpLm5vdC50b0JlKHJlc3VsdFsxXS5pZCk7XG4gIHJldHVybiBbcmVzdWx0WzBdLCByZXN1bHRbMV1dO1xufVxuXG5mdW5jdGlvbiB0YWIocGFnZTogUGFnZSwgaW5zdGFuY2VJZDogc3RyaW5nKSB7XG4gIHJldHVybiBwYWdlLmdldEJ5VGVzdElkKFwic3BhY2UtdGFic1wiKS5nZXRCeVRlc3RJZChcInNlc3Npb24tdGFiXCIpXG4gICAgLmFuZChwYWdlLmxvY2F0b3IoYFtkYXRhLWluc3RhbmNlLWlkPVwiJHtpbnN0YW5jZUlkfVwiXWApKTtcbn1cblxuZnVuY3Rpb24gY2xvc2VCdXR0b24ocGFnZTogUGFnZSwgaW5zdGFuY2VJZDogc3RyaW5nKSB7XG4gIHJldHVybiB0YWIocGFnZSwgaW5zdGFuY2VJZCkubG9jYXRvcihcIi4uXCIpLmdldEJ5VGVzdElkKFwidGFiLWNsb3NlXCIpO1xufVxuXG5mdW5jdGlvbiBzaWRlYmFyU2Vzc2lvbihwYWdlOiBQYWdlLCBpbnN0YW5jZUlkOiBzdHJpbmcpIHtcbiAgcmV0dXJuIHBhZ2UuZ2V0QnlUZXN0SWQoXCJzcGFjZXMtcGFuZWxcIikuZ2V0QnlUZXN0SWQoXCJzcGFjZS1zZXNzaW9uXCIpXG4gICAgLmFuZChwYWdlLmxvY2F0b3IoYFtocmVmPVwiL3MvJHtpbnN0YW5jZUlkfVwiXWApKTtcbn1cblxuLyoqXG4gKiBUaGUgZmlyc3Qgc2Vzc2lvbiwgc3RvcHBlZCBzbyB0aGUgZXhpdGVkLXRhYiBhbmQgZXhpdGVkLWdyb3VwIHBhdGhzIGhhdmUgYVxuICogcmVhbCBzdWJqZWN0LiBUaGUgZmFrZSBOb2RlIG9ubHkgYWNrbm93bGVkZ2VzIGEgY2xvc2UsIHNvIGl0IG5ldmVyIHNldHRsZXMgYW5cbiAqIGV4aXRlZCBsaWZlY3ljbGU7IHRoYXQgbW9kZSBza2lwcyB0aGVzZSBhc3NlcnRpb25zIHJhdGhlciB0aGFuIGZha2luZyBvbmUuXG4gKi9cbmFzeW5jIGZ1bmN0aW9uIGV4aXRlZEluc3RhbmNlKHBhZ2U6IFBhZ2UsIGluc3RhbmNlSWQ6IHN0cmluZyk6IFByb21pc2U8c3RyaW5nIHwgdW5kZWZpbmVkPiB7XG4gIGF3YWl0IHBhZ2UucmVxdWVzdC5wb3N0KGAvdjEvaW5zdGFuY2VzLyR7aW5zdGFuY2VJZH0vY29tbWFuZHNgLCB7XG4gICAgaGVhZGVyczogeyBPcmlnaW46IG5ldyBVUkwocGFnZS51cmwoKSkub3JpZ2luIH0sXG4gICAgZGF0YTogeyBvcGVyYXRpb246IFwiaW5zdGFuY2UuY2xvc2VcIiwgcGF5bG9hZDoge30gfSxcbiAgfSk7XG4gIGNvbnN0IHNldHRsZWQgPSBhd2FpdCBleHBlY3QucG9sbChhc3luYyAoKSA9PiB7XG4gICAgY29uc3QgcmVjb3JkID0gYXdhaXQgcGFnZS5yZXF1ZXN0LmdldChgL3YxL2luc3RhbmNlcy8ke2luc3RhbmNlSWR9YCk7XG4gICAgcmV0dXJuIChhd2FpdCByZWNvcmQuanNvbigpKS5saWZlY3ljbGU7XG4gIH0sIHsgdGltZW91dDogcmVhbE5vZGUgPyAzMF8wMDAgOiA1XzAwMCB9KS50b0JlKFwiZXhpdGVkXCIpLnRoZW4oKCkgPT4gdHJ1ZSwgKCkgPT4gZmFsc2UpO1xuICByZXR1cm4gc2V0dGxlZCA/IGluc3RhbmNlSWQgOiB1bmRlZmluZWQ7XG59XG5cbmZ1bmN0aW9uIHNwYWNlKHBhZ2U6IFBhZ2UsIHdvcmtzcGFjZUlkOiBzdHJpbmcpIHtcbiAgcmV0dXJuIHBhZ2UuZ2V0QnlUZXN0SWQoXCJzcGFjZXMtcGFuZWxcIikuZ2V0QnlUZXN0SWQoXCJzcGFjZS1zZWxlY3RcIilcbiAgICAuYW5kKHBhZ2UubG9jYXRvcihgW2RhdGEtc3BhY2UtaWQqPSdcIiR7d29ya3NwYWNlSWR9XCInXWApKTtcbn1cblxuLyoqIFVPLTJhOiB0aGUgU3BhY2UgaW5kZXggbGl2ZXMgb24gL3Nlc3Npb25zLCByZWFjaGVkIGluLWFwcCBmcm9tIHRoZSBzaWRlYmFyLiAqL1xuYXN5bmMgZnVuY3Rpb24gdG9MaXN0KHBhZ2U6IFBhZ2UpIHtcbiAgYXdhaXQgcGFnZS5nZXRCeVJvbGUoXCJuYXZpZ2F0aW9uXCIsIHsgbmFtZTogXCLkuLvlr7zoiKpcIiB9KS5nZXRCeVJvbGUoXCJsaW5rXCIsIHsgbmFtZTogXCLkvJror51cIiB9KS5jbGljaygpO1xuICBhd2FpdCBleHBlY3QocGFnZSkudG9IYXZlVVJMKC9cXC9zZXNzaW9ucyQvKTtcbn1cblxuYXN5bmMgZnVuY3Rpb24gY3JlYXRlU2Vzc2lvbihwYWdlOiBQYWdlLCB3b3Jrc3BhY2U6IFdvcmtzcGFjZSwgcHJvbXB0OiBzdHJpbmcsIGNyZWF0ZWQ6IHN0cmluZ1tdKSB7XG4gIGF3YWl0IHBhZ2UuZ290byhcIi9zZXNzaW9ucy9uZXdcIik7XG4gIGNvbnN0IGhvc3RQaWNrZXIgPSBwYWdlLmdldEJ5VGVzdElkKFwibmV3LXNlc3Npb24taG9zdFwiKTtcbiAgbGV0IGhvc3RJZCA9IHdvcmtzcGFjZS5ob3N0SWQ7XG4gIGlmICghaG9zdElkKSB7XG4gICAgYXdhaXQgZXhwZWN0KGhvc3RQaWNrZXIpLnRvQ29udGFpblRleHQoXCJlMmUtZmFrZS1ub2RlXCIsIHsgdGltZW91dDogMjBfMDAwIH0pO1xuICAgIGhvc3RJZCA9IGF3YWl0IGhvc3RQaWNrZXIubG9jYXRvcihcIm9wdGlvblwiKS5maWx0ZXIoeyBoYXNUZXh0OiBcImUyZS1mYWtlLW5vZGVcIiB9KS5nZXRBdHRyaWJ1dGUoXCJ2YWx1ZVwiKSA/PyB1bmRlZmluZWQ7XG4gIH1cbiAgZXhwZWN0KGhvc3RJZCkudG9CZVRydXRoeSgpO1xuICBhd2FpdCBob3N0UGlja2VyLnNlbGVjdE9wdGlvbihob3N0SWQhKTtcbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi13b3Jrc3BhY2VcIikubG9jYXRvcihgb3B0aW9uW3ZhbHVlPVwiJHt3b3Jrc3BhY2UuaWR9XCJdYCkpXG4gICAgLnRvSGF2ZUNvdW50KDEsIHsgdGltZW91dDogMjBfMDAwIH0pO1xuICBhd2FpdCBwYWdlLmdldEJ5VGVzdElkKFwibmV3LXNlc3Npb24td29ya3NwYWNlXCIpLnNlbGVjdE9wdGlvbih3b3Jrc3BhY2UuaWQpO1xuICBpZiAocmVhbE5vZGUpIHtcbiAgICBhd2FpdCBwYWdlLmdldEJ5VGVzdElkKFwibmV3LXNlc3Npb24ta2luZC10ZXJtaW5hbFwiKS5jbGljaygpO1xuICAgIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1hZHZhbmNlZFwiKS5jbGljaygpO1xuICAgIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1zaGVldFwiKS5nZXRCeUxhYmVsKFwibmFtZVwiLCB7IGV4YWN0OiB0cnVlIH0pLmZpbGwocHJvbXB0KTtcbiAgfSBlbHNlIHtcbiAgICBhd2FpdCBwYWdlLmdldEJ5VGVzdElkKFwibmV3LXNlc3Npb24tcHJvbXB0XCIpLmZpbGwocHJvbXB0KTtcbiAgfVxuICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcIm5ldy1zZXNzaW9uLXN0YXJ0XCIpKS50b0JlRW5hYmxlZCgpO1xuICBjb25zdCBjcmVhdGluZyA9IHBhZ2Uud2FpdEZvclJlc3BvbnNlKChyZXNwb25zZSkgPT4gcmVzcG9uc2UucmVxdWVzdCgpLm1ldGhvZCgpID09PSBcIlBPU1RcIlxuICAgICYmIG5ldyBVUkwocmVzcG9uc2UudXJsKCkpLnBhdGhuYW1lID09PSBcIi92MS9pbnN0YW5jZXNcIik7XG4gIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1zdGFydFwiKS5jbGljaygpO1xuICBjb25zdCBjcmVhdGlvbiA9IGF3YWl0IGNyZWF0aW5nO1xuICBleHBlY3QoY3JlYXRpb24ub2soKSkudG9CZSh0cnVlKTtcbiAgY29uc3QgaW5zdGFuY2VJZCA9IChhd2FpdCBjcmVhdGlvbi5qc29uKCkpLmluc3RhbmNlLmluc3RhbmNlSWQgYXMgc3RyaW5nO1xuICBleHBlY3QoaW5zdGFuY2VJZCkudG9CZVRydXRoeSgpO1xuICBjcmVhdGVkLnB1c2goaW5zdGFuY2VJZCk7XG4gIGF3YWl0IGV4cGVjdChwYWdlKS50b0hhdmVVUkwoL1xcL3NcXC8vLCB7IHRpbWVvdXQ6IDIwXzAwMCB9KTtcbiAgZXhwZWN0KG5ldyBVUkwocGFnZS51cmwoKSkucGF0aG5hbWUpLnRvQmUoYC9zLyR7aW5zdGFuY2VJZH1gKTtcbiAgY29uc3QgcmVjb3JkID0gYXdhaXQgcGFnZS5yZXF1ZXN0LmdldChgL3YxL2luc3RhbmNlcy8ke2luc3RhbmNlSWR9YCk7XG4gIGV4cGVjdChyZWNvcmQub2soKSkudG9CZSh0cnVlKTtcbiAgZXhwZWN0KGF3YWl0IHJlY29yZC5qc29uKCkpLnRvTWF0Y2hPYmplY3QoeyBpbnN0YW5jZUlkLCBob3N0SWQsIHdvcmtzcGFjZUlkOiB3b3Jrc3BhY2UuaWQsIGN3ZDogd29ya3NwYWNlLnJvb3QgfSk7XG4gIGlmIChyZWFsTm9kZSkge1xuICAgIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwic2Vzc2lvbi1wYWdlXCIpKS50b0hhdmVBdHRyaWJ1dGUoXCJkYXRhLXZpZXdcIiwgXCJ0dHlcIik7XG4gICAgYXdhaXQgZXhwZWN0KHBhZ2UubG9jYXRvcihcIltkYXRhLXR0eS1sYWI9JzEnXVwiKSkudG9IYXZlQXR0cmlidXRlKFwiZGF0YS10dHktc3RhdHVzXCIsIFwibGl2ZVwiLCB7IHRpbWVvdXQ6IDMwXzAwMCB9KTtcbiAgICBjb25zdCB0ZXJtaW5hbElucHV0ID0gcGFnZS5nZXRCeVJvbGUoXCJ0ZXh0Ym94XCIsIHsgbmFtZTogXCJUZXJtaW5hbCBpbnB1dFwiLCBleGFjdDogdHJ1ZSB9KTtcbiAgICBhd2FpdCB0ZXJtaW5hbElucHV0LmZvY3VzKCk7XG4gICAgLy8gVGhlIHNoZWxsIGtlZXBzIGl0cyByZWdpc3RlcmVkIGN3ZCB3aGlsZSByZXBsYWNpbmcgcGVyc29uYWwgc3RhcnR1cFxuICAgIC8vIGN1c3RvbWl6YXRpb25zIHdpdGggYSBjbGVhbiBlbnZpcm9ubWVudCBhbmQgY2xlYXJpbmcgc2NyZWVuL3Njcm9sbGJhY2suXG4gICAgYXdhaXQgcGFnZS5rZXlib2FyZC50eXBlKFwiZXhlYyAvdXNyL2Jpbi9lbnYgLWkgUEFUSD0vdXNyL2JpbjovYmluIFRFUk09eHRlcm0tMjU2Y29sb3IgUFMxPSdyZW11ZGE+ICcgL2Jpbi9zaCAtYyAncHJpbnRmIFxcXCJcXFxcMDMzWzJKXFxcXDAzM1szSlxcXFwwMzNbSFNQQUNFX0NXRD0lc1xcXFxuXFxcIiBcXFwiJFBXRFxcXCI7IGV4ZWMgL2Jpbi9zaCAtaSdcIik7XG4gICAgYXdhaXQgcGFnZS5rZXlib2FyZC5wcmVzcyhcIkVudGVyXCIpO1xuICAgIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwidHR5LWFuc2ktcHJldmlld1wiKSkudG9Db250YWluVGV4dChgU1BBQ0VfQ1dEPSR7d29ya3NwYWNlLnJvb3R9YCwgeyB0aW1lb3V0OiAyMF8wMDAgfSk7XG4gICAgZXhwZWN0KGF3YWl0IChhd2FpdCBwYWdlLnJlcXVlc3QuZ2V0KGAvdjEvaW5zdGFuY2VzLyR7aW5zdGFuY2VJZH1gKSkuanNvbigpKVxuICAgICAgLnRvTWF0Y2hPYmplY3QoeyBsaWZlY3ljbGU6IFwicnVubmluZ1wiLCBraW5kOiBcInRlcm1pbmFsXCIsIGRyaXZlcjogXCJzaGVsbC1wdHlcIiB9KTtcbiAgfSBlbHNlIHtcbiAgICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcIm1lc3NhZ2VcIikuZmlsdGVyKHsgaGFzVGV4dDogYGVjaG86ICR7cHJvbXB0fWAgfSkpXG4gICAgICAudG9CZVZpc2libGUoeyB0aW1lb3V0OiAyMF8wMDAgfSk7XG4gIH1cbiAgYXdhaXQgZXhwZWN0KHRhYihwYWdlLCBpbnN0YW5jZUlkKSkudG9IYXZlQXR0cmlidXRlKFwiYXJpYS1zZWxlY3RlZFwiLCBcInRydWVcIik7XG4gIHJldHVybiB7IGluc3RhbmNlSWQsIGhvc3RJZDogaG9zdElkISB9O1xufVxuXG5hc3luYyBmdW5jdGlvbiBjbG9zZUNyZWF0ZWRTZXNzaW9ucyhwYWdlOiBQYWdlLCBpbnN0YW5jZUlkczogc3RyaW5nW10pIHtcbiAgY29uc3Qgb3V0Y29tZXMgPSBhd2FpdCBQcm9taXNlLmFsbFNldHRsZWQoaW5zdGFuY2VJZHMubWFwKGFzeW5jIChpbnN0YW5jZUlkKSA9PiB7XG4gICAgY29uc3QgcmVzcG9uc2UgPSBhd2FpdCBwYWdlLnJlcXVlc3QuZ2V0KGAvdjEvaW5zdGFuY2VzLyR7aW5zdGFuY2VJZH1gKTtcbiAgICBleHBlY3QocmVzcG9uc2Uub2soKSkudG9CZSh0cnVlKTtcbiAgICBpZiAoKGF3YWl0IHJlc3BvbnNlLmpzb24oKSkubGlmZWN5Y2xlICE9PSBcImV4aXRlZFwiKSB7XG4gICAgICBjb25zdCBjbG9zZWQgPSBhd2FpdCBwYWdlLnJlcXVlc3QucG9zdChgL3YxL2luc3RhbmNlcy8ke2luc3RhbmNlSWR9L2NvbW1hbmRzYCwge1xuICAgICAgICBoZWFkZXJzOiB7IE9yaWdpbjogbmV3IFVSTChwYWdlLnVybCgpKS5vcmlnaW4gfSxcbiAgICAgICAgZGF0YTogeyBvcGVyYXRpb246IFwiaW5zdGFuY2UuY2xvc2VcIiwgcGF5bG9hZDoge30gfSxcbiAgICAgIH0pO1xuICAgICAgZXhwZWN0KGNsb3NlZC5vaygpKS50b0JlKHRydWUpO1xuICAgIH1cbiAgICBhd2FpdCBleHBlY3QucG9sbChhc3luYyAoKSA9PiB7XG4gICAgICBjb25zdCByZWNvcmQgPSBhd2FpdCBwYWdlLnJlcXVlc3QuZ2V0KGAvdjEvaW5zdGFuY2VzLyR7aW5zdGFuY2VJZH1gKTtcbiAgICAgIHJldHVybiAoYXdhaXQgcmVjb3JkLmpzb24oKSkubGlmZWN5Y2xlO1xuICAgIH0sIHsgdGltZW91dDogMzBfMDAwIH0pLnRvQmUoXCJleGl0ZWRcIik7XG4gIH0pKTtcbiAgZXhwZWN0KG91dGNvbWVzLmZpbHRlcigocmVzdWx0KSA9PiByZXN1bHQuc3RhdHVzID09PSBcInJlamVjdGVkXCIpLCBcIkV2ZXJ5IG93bmVkIHJlYWwgc2hlbGwgc2Vzc2lvbiBtdXN0IGV4aXRcIikudG9FcXVhbChbXSk7XG59XG5cbmFzeW5jIGZ1bmN0aW9uIHNjcmVlbnNob3QocGFnZTogUGFnZSwgbmFtZTogc3RyaW5nLCB0aGVtZTogXCJuaWdodFwiIHwgXCJsZWRnZXJcIikge1xuICBpZiAocmVhbE5vZGUpIHtcbiAgICBhd2FpdCBleHBlY3QocGFnZS5sb2NhdG9yKFwiW2RhdGEtdHR5LWxhYj0nMSddXCIpKS50b0hhdmVBdHRyaWJ1dGUoXCJkYXRhLXR0eS1zdGF0dXNcIiwgXCJsaXZlXCIsIHsgdGltZW91dDogMzBfMDAwIH0pO1xuICAgIGF3YWl0IGV4cGVjdChwYWdlLmxvY2F0b3IoXCJbZGF0YS10dHktcmVhZHldXCIpKS50b0hhdmVBdHRyaWJ1dGUoXCJkYXRhLXR0eS1yZWFkeVwiLCBcIjFcIik7XG4gICAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJ0dHktYW5zaS1wcmV2aWV3XCIpKS50b0NvbnRhaW5UZXh0KFwiU1BBQ0VfQ1dEPVwiLCB7IHRpbWVvdXQ6IDIwXzAwMCB9KTtcbiAgfVxuICBhd2FpdCBzZXRNb2RlKHBhZ2UsIHRoZW1lKTtcbiAgYXdhaXQgcGFnZS5ldmFsdWF0ZSgoKSA9PiBkb2N1bWVudC5mb250cy5yZWFkeSk7XG4gIGF3YWl0IG1rZGlyKGV2aWRlbmNlLCB7IHJlY3Vyc2l2ZTogdHJ1ZSB9KTtcbiAgY29uc3QgaW1hZ2UgPSBhd2FpdCBwYWdlLnNjcmVlbnNob3QoeyBwYXRoOiBwYXRoLmpvaW4oZXZpZGVuY2UsIGAke3JlYWxOb2RlID8gXCJcIiA6IFwiZmFrZS1cIn0ke25hbWV9YCksIGFuaW1hdGlvbnM6IFwiZGlzYWJsZWRcIiwgc2NhbGU6IFwiY3NzXCIgfSk7XG4gIGV4cGVjdChpbWFnZS5ieXRlTGVuZ3RoLCBgJHtuYW1lfSBtdXN0IHN0YXkgYmVsb3cgMzAwIEtCYCkudG9CZUxlc3NUaGFuT3JFcXVhbCgzMDBfMDAwKTtcbn1cblxudGVzdChcInJlZ2lzdGVyZWQgc3BhY2VzIGlzb2xhdGUgdGFicywgcmVtZW1iZXIgc2VsZWN0aW9uIGFuZCBjb2xsYXBzZSwgYW5kIGZpdCBhIHBob25lIGRyYXdlclwiLCBhc3luYyAoeyBwYWdlIH0pID0+IHtcbiAgdGVzdC5zZXRUaW1lb3V0KHJlYWxOb2RlID8gMjQwXzAwMCA6IDEyMF8wMDApO1xuICBjb25zdCBwYWdlRXJyb3JzOiBzdHJpbmdbXSA9IFtdO1xuICBwYWdlLm9uKFwicGFnZWVycm9yXCIsIChlcnJvcikgPT4gcGFnZUVycm9ycy5wdXNoKGVycm9yLm1lc3NhZ2UpKTtcbiAgYXdhaXQgcGFnZS5zZXRWaWV3cG9ydFNpemUoeyB3aWR0aDogMTQ0MCwgaGVpZ2h0OiA5MDAgfSk7XG4gIGF3YWl0IGxvZ2luKHBhZ2UsIFwic3BhY2VzLWUyZS1icm93c2VyXCIpO1xuICBjb25zdCBbcHJpbWFyeSwgc2Vjb25kYXJ5XSA9IGF3YWl0IHJlZ2lzdGVyZWRTcGFjZXMocGFnZSk7XG4gIHRlc3QuaW5mbygpLmFubm90YXRpb25zLnB1c2goeyB0eXBlOiBcImVuZ2luZVwiLCBkZXNjcmlwdGlvbjogcmVhbE5vZGVcbiAgICA/IFwiT3duZWQgcmVtdWRhIGRldiBOb2RlOiByZWdpc3RlcmVkIHdvcmtzcGFjZXMsIG5hdGl2ZSBzaGVsbC1wdHksIGFjdHVhbCBQV0QgYW5kIHNldHRsZWQgY2xlYW51cFwiXG4gICAgOiBcIkRpc3Bvc2FibGUgcmVhbCBIdWI6IGZha2UgTm9kZSBpbnZlbnRvcnkgYW5kIGVjaG8gZW5naW5lXCIgfSk7XG4gIGNvbnN0IGNyZWF0ZWQ6IHN0cmluZ1tdID0gW107XG4gIHRyeSB7XG4gICAgY29uc3QgZmlyc3QgPSBhd2FpdCBjcmVhdGVTZXNzaW9uKHBhZ2UsIHByaW1hcnksIFwiUmV2aWV3IHRoZSBwcm9qZWN0IHBsYW5cIiwgY3JlYXRlZCk7XG4gICAgY29uc3Qgc2Vjb25kID0gYXdhaXQgY3JlYXRlU2Vzc2lvbihwYWdlLCBwcmltYXJ5LCBcIkNoZWNrIHRoZSBwcm9qZWN0IHRlc3RzXCIsIGNyZWF0ZWQpO1xuICAgIGNvbnN0IG90aGVyID0gYXdhaXQgY3JlYXRlU2Vzc2lvbihwYWdlLCBzZWNvbmRhcnksIFwiUGxhbiB0aGUgc2Vjb25kIHByb2plY3RcIiwgY3JlYXRlZCk7XG4gICAgY29uc3QgcGFuZWwgPSBwYWdlLmdldEJ5VGVzdElkKFwic3BhY2VzLXBhbmVsXCIpO1xuICAgIGNvbnN0IHN0cmlwID0gcGFnZS5nZXRCeVRlc3RJZChcInNwYWNlLXRhYnNcIik7XG5cbiAgICBhd2FpdCBleHBlY3QodGFiKHBhZ2UsIGZpcnN0Lmluc3RhbmNlSWQpKS50b0hhdmVDb3VudCgwKTtcbiAgICBhd2FpdCBleHBlY3QodGFiKHBhZ2UsIHNlY29uZC5pbnN0YW5jZUlkKSkudG9IYXZlQ291bnQoMCk7XG4gICAgLy8gVU8tMmE6IHRoZSBTcGFjZSBpbmRleCBsaXZlcyBvbiAvc2Vzc2lvbnMgKG5vIHN0cmlwIHRoZXJlKTsgdGhlIHN0cmlwXG4gICAgLy8gYmVsb25ncyB0byAvcy8qLlxuICAgIGF3YWl0IHRvTGlzdChwYWdlKTtcbiAgICBhd2FpdCBleHBlY3Qoc3RyaXApLnRvSGF2ZUNvdW50KDApO1xuICAgIGF3YWl0IHNwYWNlKHBhZ2UsIHByaW1hcnkuaWQpLmNsaWNrKCk7XG4gICAgYXdhaXQgZXhwZWN0KHNwYWNlKHBhZ2UsIHByaW1hcnkuaWQpKS50b0hhdmVBdHRyaWJ1dGUoXCJhcmlhLXByZXNzZWRcIiwgXCJ0cnVlXCIpO1xuICAgIGF3YWl0IGV4cGVjdChwYWdlKS50b0hhdmVVUkwoL1xcL3Nlc3Npb25zJC8pO1xuICAgIGF3YWl0IHNpZGViYXJTZXNzaW9uKHBhZ2UsIHNlY29uZC5pbnN0YW5jZUlkKS5jbGljaygpO1xuICAgIGF3YWl0IGV4cGVjdCh0YWIocGFnZSwgc2Vjb25kLmluc3RhbmNlSWQpKS50b0hhdmVBdHRyaWJ1dGUoXCJhcmlhLXNlbGVjdGVkXCIsIFwidHJ1ZVwiKTtcbiAgICBhd2FpdCBleHBlY3QodGFiKHBhZ2UsIGZpcnN0Lmluc3RhbmNlSWQpKS50b0JlVmlzaWJsZSgpO1xuICAgIGF3YWl0IGV4cGVjdCh0YWIocGFnZSwgb3RoZXIuaW5zdGFuY2VJZCkpLnRvSGF2ZUNvdW50KDApO1xuICAgIGF3YWl0IHRhYihwYWdlLCBmaXJzdC5pbnN0YW5jZUlkKS5jbGljaygpO1xuICAgIGF3YWl0IHRvTGlzdChwYWdlKTtcbiAgICBhd2FpdCBleHBlY3Qoc3BhY2UocGFnZSwgc2Vjb25kYXJ5LmlkKSkudG9CZVZpc2libGUoKTtcbiAgICBjb25zdCBvcmRlcmVkU3BhY2VzID0gcGFuZWwuZ2V0QnlUZXN0SWQoXCJzcGFjZS1zZWxlY3RcIik7XG4gICAgY29uc3Qgc3BhY2VJZHMgPSBhd2FpdCBvcmRlcmVkU3BhY2VzLmV2YWx1YXRlQWxsKChpdGVtcykgPT4gaXRlbXMubWFwKChpdGVtKSA9PiBpdGVtLmdldEF0dHJpYnV0ZShcImRhdGEtc3BhY2UtaWRcIikpKTtcbiAgICBjb25zdCBwcmltYXJ5SWQgPSBhd2FpdCBzcGFjZShwYWdlLCBwcmltYXJ5LmlkKS5nZXRBdHRyaWJ1dGUoXCJkYXRhLXNwYWNlLWlkXCIpO1xuICAgIGNvbnN0IHNlY29uZGFyeUlkID0gYXdhaXQgc3BhY2UocGFnZSwgc2Vjb25kYXJ5LmlkKS5nZXRBdHRyaWJ1dGUoXCJkYXRhLXNwYWNlLWlkXCIpO1xuICAgIGF3YWl0IHNwYWNlKHBhZ2UsIHNlY29uZGFyeS5pZCkuY2xpY2soKTtcbiAgICBhd2FpdCBzaWRlYmFyU2Vzc2lvbihwYWdlLCBvdGhlci5pbnN0YW5jZUlkKS5jbGljaygpO1xuICAgIGF3YWl0IGV4cGVjdCh0YWIocGFnZSwgb3RoZXIuaW5zdGFuY2VJZCkpLnRvSGF2ZUF0dHJpYnV0ZShcImFyaWEtc2VsZWN0ZWRcIiwgXCJ0cnVlXCIpO1xuICAgIC8vIFN3aXRjaGluZyBTcGFjZXMgYnkga2V5Ym9hcmQgbGFuZHMgb24gdGhlIFNwYWNlJ3MgcmVtZW1iZXJlZCB0YWIuIE90aGVyXG4gICAgLy8gbGl2ZSBzcGVjcyBhZGQgU3BhY2VzLCBzbyB3YWxrIHRoZSBzaG9ydGVyIHdheSByb3VuZCB0aGUgcmVuZGVyZWQgb3JkZXIuXG4gICAgY29uc3QgZm9yd2FyZCA9IChzcGFjZUlkcy5pbmRleE9mKHByaW1hcnlJZCkgLSBzcGFjZUlkcy5pbmRleE9mKHNlY29uZGFyeUlkKSArIHNwYWNlSWRzLmxlbmd0aCkgJSBzcGFjZUlkcy5sZW5ndGg7XG4gICAgY29uc3QgW2tleSwgc3RlcHNdID0gZm9yd2FyZCA8PSBzcGFjZUlkcy5sZW5ndGggLyAyID8gW1wiQ29udHJvbE9yTWV0YStdXCIsIGZvcndhcmRdIDogW1wiQ29udHJvbE9yTWV0YStbXCIsIHNwYWNlSWRzLmxlbmd0aCAtIGZvcndhcmRdO1xuICAgIGZvciAobGV0IHN0ZXAgPSAwOyBzdGVwIDwgc3RlcHM7IHN0ZXArKykge1xuICAgICAgYXdhaXQgcGFnZS5nZXRCeVRlc3RJZChcInNpZGViYXItdG9nZ2xlXCIpLmZvY3VzKCk7XG4gICAgICBhd2FpdCBwYWdlLmtleWJvYXJkLnByZXNzKGtleSk7XG4gICAgfVxuICAgIGF3YWl0IGV4cGVjdChwYWdlKS50b0hhdmVVUkwobmV3IFJlZ0V4cChgL3MvJHtmaXJzdC5pbnN0YW5jZUlkfSRgKSk7XG4gICAgYXdhaXQgZXhwZWN0KHRhYihwYWdlLCBmaXJzdC5pbnN0YW5jZUlkKSkudG9IYXZlQXR0cmlidXRlKFwiYXJpYS1zZWxlY3RlZFwiLCBcInRydWVcIik7XG5cbiAgICAvLyBBIGRlZXAgbGluayByZXN0b3JlcyBib3RoIHByb2plY3QgYW5kIHRhYiwgaW5jbHVkaW5nIGFmdGVyIGEgZnJlc2ggbG9hZC5cbiAgICBhd2FpdCBwYWdlLmdvdG8oYC9zLyR7b3RoZXIuaW5zdGFuY2VJZH1gKTtcbiAgICBhd2FpdCBleHBlY3QodGFiKHBhZ2UsIG90aGVyLmluc3RhbmNlSWQpKS50b0hhdmVBdHRyaWJ1dGUoXCJhcmlhLXNlbGVjdGVkXCIsIFwidHJ1ZVwiKTtcbiAgICBhd2FpdCBleHBlY3QodGFiKHBhZ2UsIGZpcnN0Lmluc3RhbmNlSWQpKS50b0hhdmVDb3VudCgwKTtcbiAgICBhd2FpdCBwYWdlLnJlbG9hZCgpO1xuICAgIGF3YWl0IGV4cGVjdCh0YWIocGFnZSwgb3RoZXIuaW5zdGFuY2VJZCkpLnRvSGF2ZUF0dHJpYnV0ZShcImFyaWEtc2VsZWN0ZWRcIiwgXCJ0cnVlXCIpO1xuXG4gICAgLy8gVGhlIGdsb2JhbCBOZXcgYWN0aW9uIGluaGVyaXRzIHRoZSBzZWxlY3RlZCBwcm9qZWN0IHJhdGhlciB0aGFuIGFub3RoZXJcbiAgICAvLyBwcm9qZWN0J3MgbGFzdCBzdWNjZXNzZnVsIGNyZWF0aW9uIHByZWZlcmVuY2VzLlxuICAgIGF3YWl0IHRvTGlzdChwYWdlKTtcbiAgICBhd2FpdCBzcGFjZShwYWdlLCBwcmltYXJ5LmlkKS5jbGljaygpO1xuICAgIGF3YWl0IHBhZ2UuZ2V0QnlUaXRsZShcIuaWsOW7ulwiLCB7IGV4YWN0OiB0cnVlIH0pLmNsaWNrKCk7XG4gICAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1ob3N0XCIpKS50b0hhdmVWYWx1ZShmaXJzdC5ob3N0SWQpO1xuICAgIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwibmV3LXNlc3Npb24td29ya3NwYWNlXCIpKS50b0hhdmVWYWx1ZShwcmltYXJ5LmlkKTtcbiAgICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcIm5ldy1zZXNzaW9uLXdvcmtzcGFjZVwiKSkudG9Db250YWluVGV4dChwcmltYXJ5LnJvb3QpO1xuICAgIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1zaGVldFwiKS5nZXRCeVJvbGUoXCJidXR0b25cIiwgeyBuYW1lOiBcIuWFs+mXrVwiLCBleGFjdDogdHJ1ZSB9KS5jbGljaygpO1xuICAgIGF3YWl0IHRvTGlzdChwYWdlKTtcbiAgICBhd2FpdCBzcGFjZShwYWdlLCBwcmltYXJ5LmlkKS5jbGljaygpO1xuICAgIGF3YWl0IHNpZGViYXJTZXNzaW9uKHBhZ2UsIGZpcnN0Lmluc3RhbmNlSWQpLmNsaWNrKCk7XG5cbiAgICAvLyBVc2UgdGhlIHJlbmRlcmVkIG9yZGVyIHNvIGxlZ2FjeSBzZXNzaW9ucyBmcm9tIHRoZSBvdGhlciBsaXZlIHNwZWMgZG8gbm90XG4gICAgLy8gbWFrZSBudW1lcmljIGtleWJvYXJkIHNob3J0Y3V0cyBkZXBlbmQgb24gdGhlIGZpeHR1cmUncyBzZXNzaW9uIGNvdW50LlxuICAgIGNvbnN0IHRhYnMgPSBzdHJpcC5nZXRCeVJvbGUoXCJ0YWJcIik7XG4gICAgYXdhaXQgdGFicy5maXJzdCgpLmNsaWNrKCk7XG4gICAgYXdhaXQgcGFnZS5nZXRCeVRlc3RJZChcInNpZGViYXItdG9nZ2xlXCIpLmZvY3VzKCk7XG4gICAgYXdhaXQgcGFnZS5rZXlib2FyZC5wcmVzcyhcIkNvbnRyb2xPck1ldGErMlwiKTtcbiAgICBhd2FpdCBleHBlY3QodGFicy5udGgoMSkpLnRvSGF2ZUF0dHJpYnV0ZShcImFyaWEtc2VsZWN0ZWRcIiwgXCJ0cnVlXCIpO1xuICAgIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJzaWRlYmFyLXRvZ2dsZVwiKS5mb2N1cygpO1xuICAgIGF3YWl0IHBhZ2Uua2V5Ym9hcmQucHJlc3MoXCJDb250cm9sT3JNZXRhKzFcIik7XG4gICAgYXdhaXQgZXhwZWN0KHRhYnMuZmlyc3QoKSkudG9IYXZlQXR0cmlidXRlKFwiYXJpYS1zZWxlY3RlZFwiLCBcInRydWVcIik7XG4gICAgY29uc3QgbmV4dEluZGV4ID0gKHNwYWNlSWRzLmluZGV4T2YocHJpbWFyeUlkKSArIDEpICUgc3BhY2VJZHMubGVuZ3RoO1xuICAgIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJzaWRlYmFyLXRvZ2dsZVwiKS5mb2N1cygpO1xuICAgIGF3YWl0IHBhZ2Uua2V5Ym9hcmQucHJlc3MoXCJDb250cm9sT3JNZXRhK11cIik7XG4gICAgYXdhaXQgdG9MaXN0KHBhZ2UpO1xuICAgIGF3YWl0IGV4cGVjdChvcmRlcmVkU3BhY2VzLm50aChuZXh0SW5kZXgpKS50b0hhdmVBdHRyaWJ1dGUoXCJhcmlhLXByZXNzZWRcIiwgXCJ0cnVlXCIpO1xuICAgIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJzaWRlYmFyLXRvZ2dsZVwiKS5mb2N1cygpO1xuICAgIGF3YWl0IHBhZ2Uua2V5Ym9hcmQucHJlc3MoXCJDb250cm9sT3JNZXRhK1tcIik7XG4gICAgYXdhaXQgdG9MaXN0KHBhZ2UpO1xuICAgIGF3YWl0IGV4cGVjdChzcGFjZShwYWdlLCBwcmltYXJ5LmlkKSkudG9IYXZlQXR0cmlidXRlKFwiYXJpYS1wcmVzc2VkXCIsIFwidHJ1ZVwiKTtcblxuICAgIC8vIOKMmC9DdHJsK0IgZm9sZHMgdGhlIHNpZGViYXIgKHByZWZzLmNvbGxhcHNlZCkgb24gZXZlcnkgZGVza3RvcCByb3V0ZS5cbiAgICBhd2FpdCBzaWRlYmFyU2Vzc2lvbihwYWdlLCBmaXJzdC5pbnN0YW5jZUlkKS5jbGljaygpO1xuICAgIGF3YWl0IGV4cGVjdCh0YWIocGFnZSwgZmlyc3QuaW5zdGFuY2VJZCkpLnRvSGF2ZUF0dHJpYnV0ZShcImFyaWEtc2VsZWN0ZWRcIiwgXCJ0cnVlXCIpO1xuICAgIGNvbnN0IHNpZGViYXIgPSBwYWdlLmdldEJ5VGVzdElkKFwic2lkZWJhclwiKTtcbiAgICBhd2FpdCBwYWdlLmdldEJ5VGVzdElkKFwic2lkZWJhci10b2dnbGVcIikuY2xpY2soKTtcbiAgICBhd2FpdCBleHBlY3Qoc2lkZWJhcikudG9IYXZlQXR0cmlidXRlKFwiZGF0YS1jb2xsYXBzZWRcIiwgXCJ0cnVlXCIpO1xuICAgIGF3YWl0IHBhZ2UucmVsb2FkKCk7XG4gICAgYXdhaXQgZXhwZWN0KHNpZGViYXIpLnRvSGF2ZUF0dHJpYnV0ZShcImRhdGEtY29sbGFwc2VkXCIsIFwidHJ1ZVwiKTtcbiAgICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVJvbGUoXCJidXR0b25cIiwgeyBuYW1lOiBcIuWxleW8gOS+p+agj1wiLCBleGFjdDogdHJ1ZSB9KSkudG9CZVZpc2libGUoKTtcbiAgICBhd2FpdCBzY3JlZW5zaG90KHBhZ2UsIFwiZGVza3RvcC1jb2xsYXBzZWQtZGFyay5wbmdcIiwgXCJuaWdodFwiKTtcbiAgICBhd2FpdCBwYWdlLmdldEJ5VGVzdElkKFwic2lkZWJhci10b2dnbGVcIikuZm9jdXMoKTtcbiAgICBhd2FpdCBwYWdlLmtleWJvYXJkLnByZXNzKFwiQ29udHJvbE9yTWV0YStiXCIpO1xuICAgIGF3YWl0IGV4cGVjdChzaWRlYmFyKS50b0hhdmVBdHRyaWJ1dGUoXCJkYXRhLWNvbGxhcHNlZFwiLCBcImZhbHNlXCIpO1xuICAgIGF3YWl0IHRhYihwYWdlLCBmaXJzdC5pbnN0YW5jZUlkKS5jbGljaygpO1xuICAgIGF3YWl0IHNjcmVlbnNob3QocGFnZSwgXCJkZXNrdG9wLWRhcmsucG5nXCIsIFwibmlnaHRcIik7XG4gICAgYXdhaXQgc2NyZWVuc2hvdChwYWdlLCBcImRlc2t0b3AtbGlnaHQucG5nXCIsIFwibGVkZ2VyXCIpO1xuXG4gICAgYXdhaXQgcGFnZS5zZXRWaWV3cG9ydFNpemUoeyB3aWR0aDogNDAwLCBoZWlnaHQ6IDg2MCB9KTtcbiAgICAvLyBELTA0OTogb24gdGhlIGNvbXBhY3QgL3MvOmlkIHJvdXRlIHRoZSBmdWxsIGNoaXBzIHN0cmlwIGlzIGZvbGRlZCBpbnRvXG4gICAgLy8gdGhlIG9uZSBoZWFkZXIgc3BhY2UgY2hpcCAoc2FtZSBzcGFjZXMtY2hpcHMgd3JhcHBlciB0ZXN0aWQpIGFuZCB0aGVcbiAgICAvLyBTcGFjZVRhYnMgcm93IGRvZXMgbm90IHJlbmRlciBhdCBhbGw7IHRoZSBoZWFkZXIgY2hpcCdzIGRyYXdlciBrZWVwc1xuICAgIC8vIHRoZSBzd2l0Y2hpbmcgY2FwYWJpbGl0eS5cbiAgICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcInNwYWNlcy1jaGlwc1wiKSkudG9CZVZpc2libGUoKTtcbiAgICBhd2FpdCBleHBlY3Qoc3RyaXApLnRvSGF2ZUNvdW50KDApO1xuICAgIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwic3BhY2VzLWRyYXdlclwiKSkudG9IYXZlQ291bnQoMCk7XG4gICAgYXdhaXQgc2NyZWVuc2hvdChwYWdlLCBcInBob25lLWxpZ2h0LnBuZ1wiLCBcImxlZGdlclwiKTtcbiAgICBhd2FpdCBzY3JlZW5zaG90KHBhZ2UsIFwicGhvbmUtZGFyay5wbmdcIiwgXCJuaWdodFwiKTtcbiAgICBhd2FpdCBwYWdlLmdldEJ5VGVzdElkKFwic3BhY2VzLWRyYXdlci1vcGVuXCIpLmNsaWNrKCk7XG4gICAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJzcGFjZXMtZHJhd2VyXCIpKS50b0JlVmlzaWJsZSgpO1xuICAgIGF3YWl0IHNjcmVlbnNob3QocGFnZSwgXCJwaG9uZS1kcmF3ZXItZGFyay5wbmdcIiwgXCJuaWdodFwiKTtcbiAgICBhd2FpdCBzcGFjZShwYWdlLCBzZWNvbmRhcnkuaWQpLmNsaWNrKCk7XG4gICAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJzcGFjZXMtZHJhd2VyXCIpKS50b0hhdmVDb3VudCgwKTtcbiAgICAvLyBUaGUgZHJhd2VyIHJlcGxhY2VzIHRoZSB0YWJzIHJvdyBhcyB0aGUgc3dpdGNoZXI6IHBpY2tpbmcgdGhlIG90aGVyXG4gICAgLy8gcHJvamVjdCBuYXZpZ2F0ZXMgdG8gaXRzIGFjdGl2ZSBzZXNzaW9uLCBhbmQgdGhlIHRhYnMgcm93IHN0YXlzIGdvbmVcbiAgICAvLyBvbiB0aGUgY29tcGFjdCBzZXNzaW9uIHJvdXRlIChpc29sYXRpb24gc3RpbGwgaG9sZHMgdmlhIHRoZSBVUkwpLlxuICAgIGF3YWl0IGV4cGVjdChwYWdlKS50b0hhdmVVUkwobmV3IFJlZ0V4cChgL3MvJHtvdGhlci5pbnN0YW5jZUlkfSg/OiR8Wy8/XSlgKSk7XG4gICAgYXdhaXQgZXhwZWN0KHN0cmlwKS50b0hhdmVDb3VudCgwKTtcbiAgICBjb25zdCBkaW1lbnNpb25zID0gYXdhaXQgcGFnZS5ldmFsdWF0ZSgoKSA9PiAoeyB3aWR0aDogd2luZG93LmlubmVyV2lkdGgsIGNvbnRlbnQ6IGRvY3VtZW50LmRvY3VtZW50RWxlbWVudC5zY3JvbGxXaWR0aCB9KSk7XG4gICAgZXhwZWN0KGRpbWVuc2lvbnMuY29udGVudCkudG9CZUxlc3NUaGFuT3JFcXVhbChkaW1lbnNpb25zLndpZHRoKTtcblxuICAgIGF3YWl0IHBhZ2Uuc2V0Vmlld3BvcnRTaXplKHsgd2lkdGg6IDE0NDAsIGhlaWdodDogOTAwIH0pO1xuICAgIGF3YWl0IHRvTGlzdChwYWdlKTtcbiAgICBhd2FpdCBzcGFjZShwYWdlLCBwcmltYXJ5LmlkKS5jbGljaygpO1xuICAgIGF3YWl0IHNpZGViYXJTZXNzaW9uKHBhZ2UsIHNlY29uZC5pbnN0YW5jZUlkKS5jbGljaygpO1xuICAgIGF3YWl0IGV4cGVjdCh0YWIocGFnZSwgc2Vjb25kLmluc3RhbmNlSWQpKS50b0hhdmVBdHRyaWJ1dGUoXCJhcmlhLXNlbGVjdGVkXCIsIFwidHJ1ZVwiKTtcblxuICAgIC8vIERpc21pc3NpbmcgYSBydW5uaW5nIHRhYiBtdXN0IG5vdCBzZW5kIGEgY2xvc2UgY29tbWFuZDogdGhlIHNlc3Npb25cbiAgICAvLyBrZWVwcyBydW5uaW5nIGFuZCBvbmx5IHRoaXMgZGV2aWNlJ3MgdGFiIGdvZXMgYXdheS5cbiAgICBjb25zdCBjb21tYW5kczogc3RyaW5nW10gPSBbXTtcbiAgICBjb25zdCB3YXRjaENvbW1hbmRzID0gKHJlcXVlc3Q6IGltcG9ydChcIkBwbGF5d3JpZ2h0L3Rlc3RcIikuUmVxdWVzdCkgPT4ge1xuICAgICAgaWYgKHJlcXVlc3QubWV0aG9kKCkgPT09IFwiUE9TVFwiICYmIG5ldyBVUkwocmVxdWVzdC51cmwoKSkucGF0aG5hbWUgPT09IGAvdjEvaW5zdGFuY2VzLyR7c2Vjb25kLmluc3RhbmNlSWR9L2NvbW1hbmRzYCkge1xuICAgICAgICBjb21tYW5kcy5wdXNoKHJlcXVlc3QucG9zdERhdGFKU09OKCkub3BlcmF0aW9uKTtcbiAgICAgIH1cbiAgICB9O1xuICAgIHBhZ2Uub24oXCJyZXF1ZXN0XCIsIHdhdGNoQ29tbWFuZHMpO1xuICAgIGF3YWl0IGNsb3NlQnV0dG9uKHBhZ2UsIHNlY29uZC5pbnN0YW5jZUlkKS5jbGljaygpO1xuICAgIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwidGFiLWNsb3NlLXNoZWV0XCIpKS50b0JlVmlzaWJsZSgpO1xuICAgIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJ0YWItY2xvc2Uta2VlcFwiKS5jbGljaygpO1xuICAgIGF3YWl0IGV4cGVjdCh0YWIocGFnZSwgc2Vjb25kLmluc3RhbmNlSWQpKS50b0hhdmVDb3VudCgwKTtcbiAgICBhd2FpdCBleHBlY3Qoc3RyaXAuZ2V0QnlSb2xlKFwidGFiXCIsIHsgc2VsZWN0ZWQ6IHRydWUgfSkpLnRvSGF2ZUNvdW50KDEpO1xuICAgIGF3YWl0IHBhZ2UucmVsb2FkKCk7XG4gICAgYXdhaXQgZXhwZWN0KHRhYihwYWdlLCBzZWNvbmQuaW5zdGFuY2VJZCkpLnRvSGF2ZUNvdW50KDApO1xuICAgIGF3YWl0IGV4cGVjdCh0YWIocGFnZSwgZmlyc3QuaW5zdGFuY2VJZCkpLnRvQmVWaXNpYmxlKCk7XG4gICAgZXhwZWN0KGNvbW1hbmRzLCBcIuS7heWFs+mXreagh+etviBtdXN0IG5vdCBzdG9wIHRoZSBzZXNzaW9uXCIpLnRvRXF1YWwoW10pO1xuICAgIGV4cGVjdCgoYXdhaXQgKGF3YWl0IHBhZ2UucmVxdWVzdC5nZXQoYC92MS9pbnN0YW5jZXMvJHtzZWNvbmQuaW5zdGFuY2VJZH1gKSkuanNvbigpKS5saWZlY3ljbGUpXG4gICAgICAubm90LnRvQmUoXCJleGl0ZWRcIik7XG5cbiAgICAvLyBDbGlja2luZyB0aGUgZGlzbWlzc2VkIHNlc3Npb24gaW4gdGhlIC9zZXNzaW9ucyBpbmRleCByZS1vcGVucyBpdHMgdGFiLlxuICAgIGF3YWl0IHRvTGlzdChwYWdlKTtcbiAgICBhd2FpdCBzaWRlYmFyU2Vzc2lvbihwYWdlLCBzZWNvbmQuaW5zdGFuY2VJZCkuY2xpY2soKTtcbiAgICBhd2FpdCBleHBlY3QodGFiKHBhZ2UsIHNlY29uZC5pbnN0YW5jZUlkKSkudG9IYXZlQXR0cmlidXRlKFwiYXJpYS1zZWxlY3RlZFwiLCBcInRydWVcIik7XG5cbiAgICAvLyBTdG9wcGluZyB0aHJvdWdoIHRoZSBzYW1lIHNoZWV0IGlzIGEgc2VwYXJhdGUsIGV4cGxpY2l0IGNob2ljZS5cbiAgICBhd2FpdCBjbG9zZUJ1dHRvbihwYWdlLCBzZWNvbmQuaW5zdGFuY2VJZCkuY2xpY2soKTtcbiAgICBjb25zdCBzdG9wcGVkID0gcGFnZS53YWl0Rm9yUmVzcG9uc2UoKHJlc3BvbnNlKSA9PiByZXNwb25zZS5yZXF1ZXN0KCkubWV0aG9kKCkgPT09IFwiUE9TVFwiXG4gICAgICAmJiBuZXcgVVJMKHJlc3BvbnNlLnVybCgpKS5wYXRobmFtZSA9PT0gYC92MS9pbnN0YW5jZXMvJHtzZWNvbmQuaW5zdGFuY2VJZH0vY29tbWFuZHNgXG4gICAgICAmJiByZXNwb25zZS5yZXF1ZXN0KCkucG9zdERhdGFKU09OKCkub3BlcmF0aW9uID09PSBcImluc3RhbmNlLmNsb3NlXCIpO1xuICAgIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJ0YWItY2xvc2Utc3RvcFwiKS5jbGljaygpO1xuICAgIGV4cGVjdCgoYXdhaXQgc3RvcHBlZCkub2soKSkudG9CZSh0cnVlKTtcbiAgICBwYWdlLm9mZihcInJlcXVlc3RcIiwgd2F0Y2hDb21tYW5kcyk7XG4gICAgYXdhaXQgZXhwZWN0KHRhYihwYWdlLCBzZWNvbmQuaW5zdGFuY2VJZCkpLnRvSGF2ZUNvdW50KDApO1xuICAgIGV4cGVjdChjb21tYW5kcykudG9FcXVhbChbXCJpbnN0YW5jZS5jbG9zZVwiXSk7XG5cbiAgICAvLyBBbiBleGl0ZWQgdGFiIGNsb3NlcyB3aXRoIG9uZSBjbGljayBhbmQgbm8gc2hlZXQgYXQgYWxsLlxuICAgIGNvbnN0IGV4aXRlZCA9IGF3YWl0IGV4aXRlZEluc3RhbmNlKHBhZ2UsIGZpcnN0Lmluc3RhbmNlSWQpO1xuICAgIGlmIChleGl0ZWQpIHtcbiAgICAgIGF3YWl0IGV4cGVjdCh0YWIocGFnZSwgZXhpdGVkKSkudG9CZVZpc2libGUoKTtcbiAgICAgIGF3YWl0IGNsb3NlQnV0dG9uKHBhZ2UsIGV4aXRlZCkuY2xpY2soKTtcbiAgICAgIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwidGFiLWNsb3NlLXNoZWV0XCIpKS50b0hhdmVDb3VudCgwKTtcbiAgICAgIGF3YWl0IGV4cGVjdCh0YWIocGFnZSwgZXhpdGVkKSkudG9IYXZlQ291bnQoMCk7XG5cbiAgICAgIC8vIFRoZSAvc2Vzc2lvbnMgaW5kZXgga2VlcHMgaXQgaW4gaXRzIG93biBjb2xsYXBzZWQg5bey6YCA5Ye6IGdyb3VwLlxuICAgICAgYXdhaXQgdG9MaXN0KHBhZ2UpO1xuICAgICAgY29uc3QgZ3JvdXAgPSBwYW5lbC5nZXRCeVRlc3RJZChcImV4aXRlZC10b2dnbGVcIikuZmlyc3QoKTtcbiAgICAgIGF3YWl0IGV4cGVjdChncm91cCkudG9Db250YWluVGV4dChcIuW3sumAgOWHulwiKTtcbiAgICAgIGF3YWl0IGV4cGVjdChncm91cCkudG9IYXZlQXR0cmlidXRlKFwiYXJpYS1leHBhbmRlZFwiLCBcImZhbHNlXCIpO1xuICAgICAgYXdhaXQgZ3JvdXAuY2xpY2soKTtcbiAgICAgIGF3YWl0IGV4cGVjdChwYW5lbC5nZXRCeVRlc3RJZChcImV4aXRlZC1zZXNzaW9uXCIpLmFuZChwYWdlLmxvY2F0b3IoYFtkYXRhLWluc3RhbmNlLWlkPVwiJHtleGl0ZWR9XCJdYCkpKS50b0JlVmlzaWJsZSgpO1xuICAgICAgYXdhaXQgZXhwZWN0KHBhbmVsLmdldEJ5VGVzdElkKFwiZXhpdGVkLXJlc3VtZVwiKS5maXJzdCgpKS50b0JlVmlzaWJsZSgpO1xuICAgICAgYXdhaXQgcGFuZWwuZ2V0QnlUZXN0SWQoXCJleGl0ZWQtZGVsZXRlXCIpLmZpcnN0KCkuY2xpY2soKTtcbiAgICAgIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwiZGVsZXRlLXNlc3Npb24tc2hlZXRcIikpLnRvQ29udGFpblRleHQoXCLliKDpmaTkvJror53lj4rlhbborrDlvZXvvJ9cIik7XG4gICAgICBhd2FpdCBwYWdlLmdldEJ5VGVzdElkKFwiZGVsZXRlLXNlc3Npb24tc2hlZXQtY2FuY2VsXCIpLmNsaWNrKCk7XG4gICAgICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcImRlbGV0ZS1zZXNzaW9uLXNoZWV0XCIpKS50b0hhdmVDb3VudCgwKTtcbiAgICAgIC8vIENhbmNlbGxpbmcgbGVhdmVzIHRoZSByZWNvcmQgdW50b3VjaGVkLlxuICAgICAgZXhwZWN0KChhd2FpdCBwYWdlLnJlcXVlc3QuZ2V0KGAvdjEvaW5zdGFuY2VzLyR7ZXhpdGVkfWApKS5vaygpKS50b0JlKHRydWUpO1xuICAgIH1cblxuICAgIGF3YWl0IHRvTGlzdChwYWdlKTtcbiAgICBhd2FpdCBzcGFjZShwYWdlLCBzZWNvbmRhcnkuaWQpLmNsaWNrKCk7XG4gICAgYXdhaXQgc2lkZWJhclNlc3Npb24ocGFnZSwgb3RoZXIuaW5zdGFuY2VJZCkuY2xpY2soKTtcbiAgICBhd2FpdCBleHBlY3QodGFiKHBhZ2UsIG90aGVyLmluc3RhbmNlSWQpKS50b0hhdmVBdHRyaWJ1dGUoXCJhcmlhLXNlbGVjdGVkXCIsIFwidHJ1ZVwiKTtcbiAgICBleHBlY3QocGFnZUVycm9ycykudG9FcXVhbChbXSk7XG4gIH0gZmluYWxseSB7XG4gICAgaWYgKHJlYWxOb2RlKSBhd2FpdCBjbG9zZUNyZWF0ZWRTZXNzaW9ucyhwYWdlLCBjcmVhdGVkKTtcbiAgfVxufSk7XG4iXSwibWFwcGluZ3MiOiJBQUFBLFNBQVNBLE1BQU0sRUFBRUMsSUFBSSxRQUFtQixrQkFBa0I7QUFDMUQsU0FBU0MsS0FBSyxRQUFRLGtCQUFrQjtBQUN4QyxPQUFPQyxJQUFJLE1BQU0sV0FBVztBQUM1QixTQUFTQyxhQUFhLFFBQVEsVUFBVTtBQUN4QyxTQUFTQyxPQUFPLFFBQVEsb0JBQW9CO0FBQzVDLFNBQVNDLEtBQUssUUFBUSxZQUFZOztBQUVsQztBQUNBO0FBQ0E7QUFDQSxNQUFNQyxRQUFRLEdBQUdDLE9BQU8sQ0FBQ0MsR0FBRyxDQUFDQyxlQUFlLEtBQUssR0FBRyxHQUNoRFAsSUFBSSxDQUFDUSxJQUFJLENBQUNSLElBQUksQ0FBQ1MsT0FBTyxDQUFDUixhQUFhLENBQUNTLE1BQU0sQ0FBQ0MsSUFBSSxDQUFDQyxHQUFHLENBQUMsQ0FBQyxFQUFF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