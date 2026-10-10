// b68ce755da3083ee47b9bcd73428e31110dac751
import { expect, test } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { expectCookieSession, login } from "./hub-auth";
const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence ? path.join(here, "../../../docs/design/evidence") : path.join(here, "../../test-results/native-pty-web");
async function shot(page, name) {
  await mkdir(shotDir, {
    recursive: true
  });
  await page.screenshot({
    path: path.join(shotDir, name),
    animations: "disabled"
  });
}

/**
 * Answer every pending approval/question this instance currently has.
 *
 * Gate flake: a launch approval is journaled concurrently with the create
 * response, so an interactions poll taken immediately after navigation can
 * read 0 pending — the old helper passed as soon as pending read 0, before the
 * approval existed, and the launch stayed blocked. We therefore wait until an
 * interaction for the instance EXISTS in any state (proving the launch
 * approval was created — it may already be answered, e.g. the hook-approval
 * case resolves its card on /approvals before calling this helper), answer
 * whatever is still pending, then wait for the pending list to clear.
 */
async function answerPendingApprovals(page, instanceId) {
  const items = () => page.evaluate(async id => {
    var _body$items;
    const list = await fetch("/v1/interactions", {
      credentials: "include"
    });
    const body = await list.json();
    return ((_body$items = body.items) !== null && _body$items !== void 0 ? _body$items : []).filter(item => item.instanceId === id);
  }, instanceId);
  await expect.poll(async () => (await items()).length, {
    timeout: 20000,
    message: "launch approval exists"
  }).toBeGreaterThan(0);

  // Clear inside the poll: every attempt re-lists the pending set and answers
  // whatever is still open, so a late-populated options array, a rejected
  // answer, or a second approval cannot strand the launch until the timeout.
  // An id is remembered only after its answer POST succeeded.
  const answered = new Set();
  await expect.poll(async () => {
    const pending = (await items()).filter(item => item.state === "pending");
    for (const item of pending) {
      var _item$request, _item$request2;
      if (answered.has(item.id)) continue;
      const optionId = (_item$request = item.request) === null || _item$request === void 0 || (_item$request = _item$request.options) === null || _item$request === void 0 || (_item$request = _item$request[0]) === null || _item$request === void 0 ? void 0 : _item$request.id;
      if (!optionId) continue;
      const ok = await page.evaluate(async ({
        iid,
        optionId,
        digest
      }) => {
        const res = await fetch(`/v1/interactions/${iid}/answer`, {
          method: "POST",
          credentials: "include",
          headers: {
            "content-type": "application/json"
          },
          body: JSON.stringify({
            answer: {
              kind: "approval",
              optionId,
              inputDigest: digest !== null && digest !== void 0 ? digest : ""
            }
          })
        });
        return res.ok;
      }, {
        iid: item.id,
        optionId,
        digest: (_item$request2 = item.request) === null || _item$request2 === void 0 ? void 0 : _item$request2.inputDigest
      });
      if (ok) answered.add(item.id);
    }
    return pending.length;
  }, {
    timeout: 20000,
    message: "approvals clear"
  }).toBe(0);
}
test.describe.configure({
  mode: "serial"
});

/**
 * Gate hygiene: this file creates instances and only two of its eight tests
 * delete theirs, so by the second half the shared in-process fake Node is
 * carrying this file's leftovers, and earlier gate files leave more. The fake
 * host advertises a fixed `maxInstances: 8` (a product ceiling other specs
 * deliberately exercise), and a POST refused with 422 PLACEMENT_UNSATISFIABLE
 * leaves NewSessionPage on /sessions/new — the gate flake attributed to lines
 * 447/481 (it accumulates: in a 10-repeat loop against one live server the
 * 9th and 10th runs failed deterministically). Sweep the shared host before
 * the first test and after every test so each run starts with real headroom. The external-Node flow
 * owns its own host and must never be swept.
 */
async function sweepFakeNodeInstances(page) {
  const hostId = await page.evaluate(async () => {
    var _body$items$find$id, _body$items2;
    const res = await fetch("/v1/hosts", {
      credentials: "include"
    });
    if (!res.ok) return null;
    const body = await res.json();
    return (_body$items$find$id = (_body$items2 = body.items) === null || _body$items2 === void 0 || (_body$items2 = _body$items2.find(host => host.label === "e2e-fake-node")) === null || _body$items2 === void 0 ? void 0 : _body$items2.id) !== null && _body$items$find$id !== void 0 ? _body$items$find$id : null;
  });
  if (!hostId) return;
  const items = await page.evaluate(async id => {
    var _body$items3;
    const res = await fetch("/v1/instances", {
      credentials: "include"
    });
    if (!res.ok) return [];
    const body = await res.json();
    return ((_body$items3 = body.items) !== null && _body$items3 !== void 0 ? _body$items3 : []).filter(item => item.hostId === id && item.instanceId);
  }, hostId);
  for (const item of items) {
    await page.request.delete(`/v1/instances/${item.instanceId}?force=1`).catch(() => undefined);
  }
}
test.beforeAll(async ({
  browser
}) => {
  if (process.env.HUB_E2E_EXTERNAL === "1") return;
  const page = await browser.newPage();
  try {
    await login(page);
    await sweepFakeNodeInstances(page);
  } finally {
    await page.close();
  }
});
test.afterEach(async ({
  page
}) => {
  if (process.env.HUB_E2E_EXTERNAL === "1") return;
  // An unauthenticated page (a failure before login) simply sweeps nothing.
  await sweepFakeNodeInstances(page).catch(() => undefined);
});
test("device login, hosts, create/send/close, follow, approvals", async ({
  page
}) => {
  test.skip(process.env.HUB_E2E_EXTERNAL === "1", "External Node is covered by the real shell workspace flow");
  const followUrls = [];
  page.on("websocket", socket => {
    // Vite authenticates HMR with its own token; restrict this check to Hub.
    if (new URL(socket.url()).pathname === "/v1/follow") followUrls.push(socket.url());
  });
  await login(page);
  await page.getByRole("button", {
    name: "管理"
  }).click();
  await page.getByRole("menuitem", {
    name: "主机",
    exact: true
  }).click();
  await expect(page.getByTestId("hosts-page")).toBeVisible();
  await expect(page.getByTestId("host-row").filter({
    hasText: "e2e-fake-node"
  })).toBeVisible({
    timeout: 20000
  });
  await page.getByTitle("新建", {
    exact: true
  }).click();
  await expect(page.getByTestId("new-session-sheet")).toBeVisible();
  await expect(page.getByTestId("new-session-host")).toContainText("e2e-fake-node", {
    timeout: 20000
  });
  const host = await page.getByTestId("new-session-host").locator("option").filter({
    hasText: "e2e-fake-node"
  }).getAttribute("value");
  await page.getByTestId("new-session-host").selectOption(host);
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0);
  await page.getByTestId("new-session-advanced").click();
  await page.getByTestId("new-session-tui").selectOption("default");
  await page.getByTestId("new-session-prompt").fill("hello from web hub");
  await expect(page.getByTestId("new-session-start")).toBeEnabled();
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, {
    timeout: 20000
  });
  const sessionPath = new URL(page.url()).pathname;
  const createdId = sessionPath.split("/")[2];
  const stored = await (await page.request.get(`/v1/instances/${createdId}`)).json();
  expect(stored.tui).toBe("default");
  await expect(page.getByTestId("session-page")).toBeVisible();
  await expect(page.getByTestId("message").filter({
    hasText: /^You/
  })).toContainText("hello from web hub", {
    timeout: 20000
  });
  await expect(page.getByTestId("message").filter({
    hasText: "echo: hello from web hub"
  })).toHaveCount(1, {
    timeout: 20000
  });
  await expect.poll(() => followUrls.length).toBeGreaterThan(0);
  expect(followUrls.every(url => !new URL(url).searchParams.has("token"))).toBe(true);
  await page.reload();
  await expectCookieSession(page);
  // Reload waits for cookie-backed bootstrap and the durable journal read again.
  await expect(page.getByTestId("message").filter({
    hasText: "echo: hello from web hub"
  })).toHaveCount(1, {
    timeout: 20000
  });
  await expect(page.getByTestId("composer-bar")).toBeVisible();
  await page.goto("/approvals");
  await expect(page.getByTestId("approvals-page")).toBeVisible();
  // Gate flake: every create on the shared fake Node raises an identically
  // labelled "echo e2e" card, so a leftover card from a retried attempt (the
  // CI config retries once against the SAME long-lived fake Node) or from a
  // neighbouring spec whose cleanup raced the gate can sit in this queue and
  // make the text-only filter strict (3 rows). Own the row: scope it to THIS
  // test's instance through the row's session link.
  const ownApproval = () => page.getByTestId("approval-row").filter({
    has: page.locator(`a[href="/s/${createdId}"]`)
  });
  const approval = ownApproval().filter({
    hasText: "echo e2e"
  });
  await expect(approval).toBeVisible({
    timeout: 20000
  });
  await approval.getByRole("button", {
    name: "允许一次"
  }).click();
  await expect(ownApproval()).toHaveCount(0, {
    timeout: 20000
  });
  await page.goto(sessionPath);
  await expect(page.getByTestId("session-page")).toBeVisible();
  await expect(page.getByTestId("composer-bar")).toBeVisible();
  await expect(page.getByTestId("composer-input")).toBeEnabled();
  await page.getByTestId("composer-input").fill("second turn");
  await page.getByTestId("composer-send").click();
  await expect(page.getByTestId("message").filter({
    hasText: "echo: second turn"
  })).toBeVisible({
    timeout: 20000
  });
  await page.getByRole("button", {
    name: "Stop"
  }).click();
  await expect(page.getByTestId("session-page")).toBeVisible();
  expect(followUrls.every(url => !new URL(url).searchParams.has("token"))).toBe(true);
});

/**
 * D-027: an image pasted into the composer is staged on the Hub and its
 * metadata reaches the Node, with the bytes never entering a command frame.
 */
test("paste an image: it stages on the Hub and its metadata reaches the Node", async ({
  page
}) => {
  test.skip(process.env.HUB_E2E_EXTERNAL === "1", "Needs the in-process fake Node");
  await login(page);
  await page.getByTitle("新建", {
    exact: true
  }).click();
  await expect(page.getByTestId("new-session-sheet")).toBeVisible();
  await expect(page.getByTestId("new-session-host")).toContainText("e2e-fake-node", {
    timeout: 20000
  });
  const host = await page.getByTestId("new-session-host").locator("option").filter({
    hasText: "e2e-fake-node"
  }).getAttribute("value");
  await page.getByTestId("new-session-host").selectOption(host);
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0);
  await page.getByTestId("new-session-prompt").fill("attachment session");
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, {
    timeout: 20000
  });
  await expect(page.getByTestId("session-page")).toBeVisible();

  // The fake Node raises an approval on every create, and a pending one holds
  // this instance's composer disabled. Answer exactly this instance's
  // approvals through the API: the UI rows are all labelled alike, so picking
  // the right one by text is not reliable here.
  const instanceId = new URL(page.url()).pathname.split("/").pop();
  await expect.poll(async () => await page.evaluate(async id => {
    var _body$items4;
    const list = await fetch("/v1/interactions", {
      credentials: "include"
    });
    const body = await list.json();
    const mine = ((_body$items4 = body.items) !== null && _body$items4 !== void 0 ? _body$items4 : []).filter(item => item.instanceId === id && item.state === "pending");
    for (const item of mine) {
      var _item$request3, _item$request$inputDi, _item$request4;
      const optionId = (_item$request3 = item.request) === null || _item$request3 === void 0 || (_item$request3 = _item$request3.options) === null || _item$request3 === void 0 || (_item$request3 = _item$request3[0]) === null || _item$request3 === void 0 ? void 0 : _item$request3.id;
      if (!optionId) continue;
      await fetch(`/v1/interactions/${item.id}/answer`, {
        method: "POST",
        credentials: "include",
        headers: {
          "content-type": "application/json"
        },
        body: JSON.stringify({
          answer: {
            kind: "approval",
            optionId,
            inputDigest: (_item$request$inputDi = (_item$request4 = item.request) === null || _item$request4 === void 0 ? void 0 : _item$request4.inputDigest) !== null && _item$request$inputDi !== void 0 ? _item$request$inputDi : ""
          }
        })
      });
    }
    return mine.length;
  }, instanceId), {
    timeout: 20000
  }).toBe(0);
  await expect(page.getByTestId("composer-input")).toBeEnabled({
    timeout: 20000
  });

  // A real 1x1 red PNG, pasted the way a browser delivers one.
  const uploads = [];
  page.on("response", response => {
    if (new URL(response.url()).pathname === "/v1/objects") uploads.push(response.status());
  });
  await page.getByTestId("composer-input").fill("what colour is the image?");
  await page.evaluate(async () => {
    const base64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
    const bytes = Uint8Array.from(atob(base64), char => char.charCodeAt(0));
    const file = new File([bytes], "red.png", {
      type: "image/png"
    });
    const data = new DataTransfer();
    data.items.add(file);
    const area = document.querySelector("[data-testid='composer-input']");
    area === null || area === void 0 || area.dispatchEvent(new ClipboardEvent("paste", {
      clipboardData: data,
      bubbles: true
    }));
  });

  // The chip appears, the upload succeeds, and only then can the send go.
  await expect(page.getByTestId("attachment-chip")).toBeVisible({
    timeout: 20000
  });
  await expect.poll(() => uploads, {
    timeout: 20000
  }).toContain(200);
  await expect(page.getByTestId("composer-send")).toBeEnabled({
    timeout: 20000
  });
  await page.getByTestId("composer-send").click();

  // The Node echoes the resolved media type, proving the metadata arrived.
  await expect(page.getByTestId("message").filter({
    hasText: "[attachments: image/png]"
  })).toBeVisible({
    timeout: 20000
  });
  // The chip is consumed by the send and the bubble keeps the thumbnail.
  await expect(page.getByTestId("attachment-chip")).toHaveCount(0);
  await expect(page.getByTestId("sent-attachments").first()).toBeVisible();
});
test("real Node: register a project, create a shell in it, close and unregister", async ({
  page
}) => {
  test.skip(process.env.HUB_E2E_EXTERNAL !== "1", "Requires the operator's remuda dev");
  const path = process.env.HUB_E2E_WORKSPACE;
  expect(path, "Set HUB_E2E_WORKSPACE to an existing allowed directory on the Node").toBeTruthy();
  const followUrls = [];
  page.on("websocket", socket => {
    if (new URL(socket.url()).pathname === "/v1/follow") followUrls.push(socket.url());
  });
  await login(page);
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  await expect(hostPicker.locator("option")).not.toHaveCount(0);
  if (process.env.HUB_E2E_HOST_ID) await hostPicker.selectOption(process.env.HUB_E2E_HOST_ID);
  const hostId = await hostPicker.inputValue();
  const observer = await page.context().newPage();
  await observer.goto("/sessions/new");
  await observer.getByTestId("new-session-host").selectOption(hostId);
  const frozenHosts = await (await observer.request.get("/v1/hosts")).json();
  // Freeze the observer's HTTP snapshots: additions must arrive through follow,
  // and a subsequent stale poll must not undo the newer journal revision.
  await observer.route("**/v1/hosts", route => route.fulfill({
    json: frozenHosts
  }));
  await page.getByTestId("workspace-add").click();
  await page.getByTestId("workspace-register-path").fill(path);
  const registered = page.waitForResponse(response => response.request().method() === "POST" && new URL(response.url()).pathname === `/v1/hosts/${hostId}/workspaces`);
  await page.getByTestId("workspace-register-submit").click();
  const registration = await registered;
  expect(registration.ok()).toBe(true);
  const snapshot = await registration.json();
  const workspace = snapshot.workspaces.find(row => row.workspaceId === snapshot.workspaceId);
  expect(workspace).toBeTruthy();
  await expect(page.getByTestId("new-session-workspace")).toHaveValue(workspace.workspaceId);
  const observerOption = observer.getByTestId("new-session-workspace").locator(`option[value="${workspace.workspaceId}"]`);
  await expect(observerOption).toHaveCount(1);
  await observer.waitForRequest(request => new URL(request.url()).pathname === "/v1/hosts");
  await expect(observerOption).toHaveCount(1);
  await expect(page.getByTestId("new-session-cwd")).toHaveValue("");
  await page.getByTestId("new-session-kind-terminal").click();
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, {
    timeout: 30000
  });
  const sessionPath = new URL(page.url()).pathname;
  const instanceId = sessionPath.split("/").at(-1);
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-view", "tty");
  await expect(page.locator("[data-tty-lab='1']")).toHaveAttribute("data-tty-status", "live", {
    timeout: 30000
  });
  const terminalInput = page.getByRole("textbox", {
    name: "Terminal input",
    exact: true
  });
  await terminalInput.focus();
  await expect(terminalInput).toBeFocused();
  await page.keyboard.type("printf 'WORKSPACE_REG_CWD=%s\\n' \"$PWD\"");
  await page.keyboard.press("Enter");
  await expect(page.getByTestId("tty-ansi-preview")).toContainText(`WORKSPACE_REG_CWD=${workspace.root}`, {
    timeout: 20000
  });
  await expect.poll(() => followUrls.length).toBeGreaterThan(0);
  expect(followUrls.every(url => !new URL(url).searchParams.has("token"))).toBe(true);
  await page.reload();
  await expectCookieSession(page);
  await expect(page.getByTestId("session-page")).toBeVisible();
  await page.getByRole("button", {
    name: "Stop",
    exact: true
  }).click();
  await expect.poll(async () => {
    const response = await page.request.get(`/v1/instances/${instanceId}`);
    return (await response.json()).lifecycle;
  }, {
    timeout: 30000
  }).toBe("exited");
  await page.goto("/hosts");
  const row = page.getByTestId("host-workspace").filter({
    hasText: workspace.root
  });
  await expect(row).toBeVisible();
  const unregistered = page.waitForResponse(response => response.request().method() === "DELETE" && new URL(response.url()).pathname === `/v1/hosts/${hostId}/workspaces`);
  await row.getByRole("button", {
    name: `移除目录 ${workspace.root}`,
    exact: true
  }).click();
  expect((await unregistered).ok()).toBe(true);
  await expect(row).toHaveCount(0);
  await expect(observerOption).toHaveCount(0);
  await observer.close();
  await page.goto("/sessions/new");
  await hostPicker.selectOption(hostId);
  await expect(page.getByTestId("new-session-workspace").locator(`option[value="${workspace.workspaceId}"]`)).toHaveCount(0);
});
test("effort slider drag and keyboard send instance.configure", async ({
  page
}) => {
  test.skip(process.env.HUB_E2E_EXTERNAL === "1", "External Node is covered by the real shell workspace flow");
  await login(page);
  await page.getByTitle("新建", {
    exact: true
  }).click();
  await expect(page.getByTestId("new-session-sheet")).toBeVisible();
  await expect(page.getByTestId("new-session-host")).toContainText("e2e-fake-node", {
    timeout: 20000
  });
  const host = await page.getByTestId("new-session-host").locator("option").filter({
    hasText: "e2e-fake-node"
  }).getAttribute("value");
  await page.getByTestId("new-session-host").selectOption(host);
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0);
  await page.getByTestId("new-session-prompt").fill("effort slider e2e");
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, {
    timeout: 20000
  });
  await expect(page.getByTestId("composer-bar")).toBeVisible();
  await expect(page.getByTestId("model-effort-chip")).toBeVisible();
  const configureBodies = [];
  page.on("request", request => {
    if (request.method() !== "POST") return;
    if (!new URL(request.url()).pathname.endsWith("/commands")) return;
    const body = request.postDataJSON();
    if (body) configureBodies.push(body);
  });
  await page.getByTestId("model-effort-chip").click();
  const slider = page.getByTestId("effort-slider");
  await expect(slider).toBeVisible();
  await expect(slider).toHaveAttribute("data-tiers", "low,medium,high,xhigh,max,ultracode");
  const box = await slider.boundingBox();
  expect(box).toBeTruthy();
  await page.mouse.move(box.x + box.width - 3, box.y + box.height / 2);
  await page.mouse.down();
  await page.mouse.move(box.x + box.width - 3, box.y + box.height / 2, {
    steps: 3
  });
  await page.mouse.up();
  // The far-right stop is ultracode: the xhigh tier plus the workflow flag.
  // The chip only re-renders once instance.configure round-trips through the
  // Hub, so these wait on the wire like every other live assertion here.
  await expect(page.getByTestId("composer")).toHaveAttribute("data-effort", "ultracode", {
    timeout: 20000
  });
  await expect(page.getByTestId("composer")).toHaveAttribute("data-ultracode", "1", {
    timeout: 20000
  });
  await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-ember", "1", {
    timeout: 20000
  });
  await expect.poll(() => configureBodies.some(body => {
    var _body$payload;
    return body.operation === "instance.configure" && ((_body$payload = body.payload) === null || _body$payload === void 0 || (_body$payload = _body$payload.effort) === null || _body$payload === void 0 ? void 0 : _body$payload.name) === "ultracode";
  })).toBeTruthy();
  await slider.focus();
  await page.keyboard.press("Home");
  await expect(page.getByTestId("composer")).toHaveAttribute("data-effort", "low", {
    timeout: 20000
  });
  await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-ember", "0", {
    timeout: 20000
  });
  await expect.poll(() => configureBodies.some(body => {
    var _body$payload2;
    return body.operation === "instance.configure" && ((_body$payload2 = body.payload) === null || _body$payload2 === void 0 || (_body$payload2 = _body$payload2.effort) === null || _body$payload2 === void 0 ? void 0 : _body$payload2.name) === "low";
  })).toBeTruthy();
});

/**
 * D-028 §5.1/§1.0: New Session defaults to the native shell-pty carrier (the
 * choice comes from the fake Node's driverInventory), and the resulting
 * session carries BOTH projections — terminal and 结构.
 */
test("native PTY default from the host matrix, with both projections", async ({
  page
}) => {
  test.skip(process.env.HUB_E2E_EXTERNAL === "1", "Needs the in-process fake Node's matrix");
  await login(page);
  await page.goto("/sessions/new");
  await expect(page.getByTestId("new-session-host")).toContainText("e2e-fake-node", {
    timeout: 20000
  });

  // Batch B moved the carrier matrix and launch preview behind 高级设置 so
  // the first layer stays in user vocabulary; the matrix itself is unchanged.
  await page.getByTestId("new-session-advanced").click();
  const shell = page.getByTestId("new-session-driver-shell-pty");
  await expect(shell).toBeVisible();
  await expect(shell).toHaveAttribute("data-default", "1");
  await expect(page.getByTestId("new-session-launch-preview")).toContainText("claude");
  // Legacy carriers remain selectable but are secondary.
  await expect(page.getByTestId("new-session-driver-claude-print")).toHaveAttribute("data-default", "0");
  await shot(page, "native-pty-web-1-new-session-1440.png");
  await page.setViewportSize({
    width: 400,
    height: 840
  });
  await shot(page, "native-pty-web-1-new-session-400.png");
  await page.setViewportSize({
    width: 1440,
    height: 900
  });
  await page.getByTestId("new-session-prompt").fill("native pty session");
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, {
    timeout: 20000
  });

  // Both projections available: the 终端/结构 switch is rendered, and the
  // structured conversation opens by default.
  await expect(page.getByTestId("view-switch")).toBeVisible();
  await expect(page.getByTestId("view-switch-tty")).toBeVisible();
  await expect(page.getByTestId("view-switch-structured")).toBeVisible();
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-view", "structured");
});

/**
 * c-steer composer on a working native session: Enter QUEUES (Remuda-held,
 * no POST), ⌘/Ctrl+Enter or the 插队 button interrupts and jumps with
 * mode=steer, Esc plain-interrupts, and held rows flush when the turn ends.
 */
test("composer queue / steer / interrupt states on a working native session", async ({
  page
}) => {
  var _sends$, _sends$2, _sends$at;
  test.skip(process.env.HUB_E2E_EXTERNAL === "1", "Needs the in-process fake Node");
  const commands = [];
  page.on("request", request => {
    if (request.method() !== "POST") return;
    if (!new URL(request.url()).pathname.endsWith("/commands")) return;
    const body = request.postDataJSON();
    if (body) commands.push(body);
  });
  await login(page);
  await page.goto("/sessions/new");
  await expect(page.getByTestId("new-session-host")).toContainText("e2e-fake-node", {
    timeout: 20000
  });
  await page.getByTestId("new-session-prompt").fill("working session");
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, {
    timeout: 20000
  });
  const instanceId = new URL(page.url()).pathname.split("/").pop();
  await answerPendingApprovals(page, instanceId);

  // Answering the create approval completes the fake turn: the composer is
  // idle. Start a fresh "hold-working" turn to exercise the working controls.
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-status", "idle", {
    timeout: 10000
  });
  await page.getByTestId("composer-input").fill("hold-working long tool");
  await page.getByTestId("composer-send").click();
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-status", "working", {
    timeout: 10000
  });
  // While `sending` the dock is disabled; type the held text first so the
  // queue button's enablement marks the settled POST flight (it is also
  // disabled on empty text, so it can only be enabled with text present).
  await page.getByTestId("composer-input").fill("after this turn");
  await expect(page.getByTestId("composer-queue")).toBeEnabled({
    timeout: 10000
  });

  // Working composer: Enter queues (Remuda-held), 插队 and 打断 available.
  const queue = page.getByTestId("composer-queue");
  await expect(queue).toHaveAttribute("data-mode", "queue");
  await expect(queue).toHaveAttribute("data-holder", "remuda");
  const steer = page.getByTestId("composer-steer");
  await expect(steer).toBeVisible();
  await expect(steer).toHaveAttribute("data-provision", "native");
  const interrupt = page.getByTestId("composer-interrupt");
  await expect(interrupt).toBeVisible();
  await expect(interrupt).toHaveAttribute("data-provision", "native");
  await shot(page, "native-pty-web-1-composer-working-1440.png");

  // Enter holds a removable chip and sends nothing.
  const before = commands.length;
  await page.getByTestId("composer-input").press("Enter");
  await expect(page.getByTestId("composer-queued-chip")).toBeVisible();
  await expect(page.getByTestId("composer-queued-chip")).toHaveAttribute("data-ordinal", "1");
  await expect(page.getByTestId("composer-queue-status")).toContainText("1");
  expect(commands.length).toBe(before);
  await page.getByTestId("composer-queued-remove").click();
  await expect(page.getByTestId("composer-queued-chip")).toHaveCount(0);

  // Re-queue, then 插队: the steer POSTs mode=steer first; the held row flushes
  // with a plain new-turn POST only after the turn-end (idle) lands.
  await page.getByTestId("composer-input").fill("later");
  await page.getByTestId("composer-input").press("Enter");
  await expect(page.getByTestId("composer-queued-chip")).toBeVisible();
  await page.setViewportSize({
    width: 400,
    height: 840
  });
  await shot(page, "native-pty-web-1-composer-working-400.png");
  await page.setViewportSize({
    width: 1440,
    height: 900
  });
  await page.getByTestId("composer-input").fill("jump now");
  await steer.click();
  // D-042: the steer gesture confirms through the in-app Sheet, not
  // window.confirm.
  await expect(page.getByTestId("composer-confirm-title")).toHaveText("插队发送");
  await page.getByTestId("composer-confirm-ok").click();
  await expect.poll(() => commands.some(c => {
    var _c$payload;
    return c.operation === "instance.send" && ((_c$payload = c.payload) === null || _c$payload === void 0 ? void 0 : _c$payload.mode) === "steer";
  })).toBeTruthy();
  // The steer interrupted the turn (badge) and the fake Node reported idle,
  // which flushes the held row as an ordinary new turn.
  await expect(page.getByTestId("composer-interrupted-chip")).toBeVisible();
  await expect.poll(() => commands.filter(c => c.operation === "instance.send").length).toBeGreaterThanOrEqual(3);
  const sends = commands.filter(c => c.operation === "instance.send");
  expect((_sends$ = sends[0]) === null || _sends$ === void 0 || (_sends$ = _sends$.payload) === null || _sends$ === void 0 ? void 0 : _sends$.prompt).toBe("hold-working long tool");
  expect((_sends$2 = sends[1]) === null || _sends$2 === void 0 ? void 0 : _sends$2.payload).toMatchObject({
    prompt: "jump now",
    mode: "steer"
  });
  expect((_sends$at = sends.at(-1)) === null || _sends$at === void 0 || (_sends$at = _sends$at.payload) === null || _sends$at === void 0 ? void 0 : _sends$at.prompt).toBe("later");

  // Plain Esc interrupts: start another hold-working turn, then cancel it.
  await page.getByTestId("composer-input").fill("hold-working stop me");
  await page.getByTestId("composer-send").click();
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-status", "working", {
    timeout: 10000
  });
  await expect(page.getByTestId("composer-interrupt")).toBeEnabled({
    timeout: 10000
  });
  await page.getByTestId("composer-input").focus();
  await page.keyboard.press("Escape");
  // D-042: Esc opens the in-app Sheet confirm; Esc inside it cancels, so
  // press the destructive action explicitly.
  await expect(page.getByTestId("composer-confirm-title")).toHaveText("打断当前 turn");
  await page.getByTestId("composer-confirm-ok").click();
  await expect.poll(() => commands.some(c => c.operation === "instance.cancel")).toBeTruthy();
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-status", "idle", {
    timeout: 10000
  });
  await expect(page.getByTestId("composer-interrupt")).toHaveCount(0);
  await expect(page.getByTestId("composer-steer")).toHaveCount(0);
  await expect(page.getByTestId("composer-send")).toHaveAttribute("data-mode", "new-turn");
});
test("a hook-carried approval shows the real tool input and an always-allow option", async ({
  page
}) => {
  // D-028 §4.4 tier A end to end through the Hub: the card the Node builds
  // from a real PermissionRequest carries the harness-hook carrier, the tool's
  // actual input rather than a screen scrape, and an always-allow button that
  // exists only because the harness offered a permission_suggestion.
  test.skip(process.env.HUB_E2E_EXTERNAL === "1", "External Node is covered by the real shell workspace flow");
  await login(page);
  await page.getByTitle("新建", {
    exact: true
  }).click();
  await expect(page.getByTestId("new-session-sheet")).toBeVisible();
  await expect(page.getByTestId("new-session-host")).toContainText("e2e-fake-node", {
    timeout: 20000
  });
  const host = await page.getByTestId("new-session-host").locator("option").filter({
    hasText: "e2e-fake-node"
  }).getAttribute("value");
  await page.getByTestId("new-session-host").selectOption(host);
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0);
  // The fake Node raises the tier A card for a prompt naming the hook path.
  await page.getByTestId("new-session-prompt").fill("hook-approval please");
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, {
    timeout: 20000
  });
  const instanceId = new URL(page.url()).pathname.split("/").pop();
  await page.goto("/approvals");
  await expect(page.getByTestId("approvals-page")).toBeVisible();
  const row = page.getByTestId("approval-row").filter({
    hasText: "/tmp/hook-approval.txt"
  });
  // The real tool input is what the human reads before deciding (§2.2).
  await expect(row).toBeVisible({
    timeout: 20000
  });
  await expect(row).toContainText("Write");
  // All three options the card carried, including the suggested grant.
  await expect(row.getByRole("button", {
    name: "允许一次"
  })).toBeVisible();
  await expect(row.getByRole("button", {
    name: "拒绝"
  })).toBeVisible();
  const always = row.getByRole("button", {
    name: "始终允许 (acceptEdits)"
  });
  await expect(always).toBeVisible();
  await always.click();
  await expect(page.getByTestId("approval-row").filter({
    hasText: "/tmp/hook-approval.txt"
  })).toHaveCount(0, {
    timeout: 20000
  });

  // The card WAS this instance's blocking launch approval; the click above has
  // already answered it. Do NOT call answerPendingApprovals here: once the
  // fake node's card is answered it leaves interaction.list (it serves only
  // its pending map), so the helper's "an interaction exists" wait would time
  // out on the already-resolved card. The enabled composer below is the
  // release signal.
  await page.goto(`/s/${instanceId}`);
  await expect(page.getByTestId("session-page")).toBeVisible();
  await expect(page.getByTestId("composer-input")).toBeEnabled({
    timeout: 20000
  });

  // Release the placement slot. The fake host advertises maxInstances 8 and
  // the suite is serial, so a session left live here makes a later spec fail
  // placement (PLACEMENT_UNSATISFIABLE) far from the spec that leaked it. The
  // fake Node never exits, so only a forced DELETE settles the row.
  const deleted = await page.request.delete(`/v1/instances/${instanceId}?force=1`);
  expect(deleted.ok()).toBe(true);
  await expect.poll(async () => (await page.request.get(`/v1/instances/${instanceId}`)).status()).toBe(404);
});

/**
 * D-028 §7 in the browser: assistant text must grow in place as deltas land,
 * and records the human did not write must not render as their bubble.
 *
 * «现在 Structural 的界面不是按文本流式出现的…我觉得它的实时性不够» and
 * «结构化界面会把追加的 prompt 信息也额外展示了 … 容易让人误解是我发了这些信息».
 */
test("structured view streams assistant text and separates injected records", async ({
  page
}) => {
  test.skip(process.env.HUB_E2E_EXTERNAL === "1", "Needs the in-process fake Node");
  await login(page);
  await page.goto("/sessions/new");
  await expect(page.getByTestId("new-session-host")).toContainText("e2e-fake-node", {
    timeout: 20000
  });
  await page.getByTestId("new-session-prompt").fill("stream please");
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, {
    timeout: 20000
  });
  const instanceId = new URL(page.url()).pathname.split("/").pop();
  await answerPendingApprovals(page, instanceId);

  // The fake Node replies to a `stream ` prompt as an open/append chain. The
  // assembler must merge it into ONE bubble carrying the whole text — a bubble
  // per chunk is exactly the "not streaming, just re-rendering" failure.
  await page.getByTestId("composer-input").fill("stream the reply");
  await page.getByTestId("composer-send").click();
  const streamed = page.getByTestId("message").filter({
    hasText: "echo: stream the reply"
  });
  await expect(streamed).toHaveCount(1, {
    timeout: 20000
  });

  // Injected records are collapsed, not drawn as "You", and the toggle hides
  // them entirely. The fake Node emits none, so assert the invariant that
  // holds either way: nothing claiming to be the user that the user never sent.
  const bubbles = await page.getByTestId("message").filter({
    hasText: /^You/
  }).allTextContents();
  expect(bubbles.every(text => !text.includes("<task-notification>"))).toBe(true);
  expect(bubbles.every(text => !text.includes("<command-name>"))).toBe(true);
  await shot(page, "native-pty-web-3-structured-stream-1440.png");
  await page.setViewportSize({
    width: 400,
    height: 840
  });
  await shot(page, "native-pty-web-3-structured-stream-400.png");
  await page.setViewportSize({
    width: 1440,
    height: 900
  });

  // Release the placement slot. The fake host advertises `maxInstances: 8`
  // and the suite is serial, so a spec that leaves its instance live spends
  // one of those slots for the rest of the run — a later spec creating
  // several sessions then fails placement with a non-OK POST /v1/instances,
  // far from the spec that actually leaked.
  //
  // The fake Node never emits an exit lifecycle (there is no real process to
  // lose), so `instance.close` alone leaves the row `running` and the slot
  // counted. DELETE settles to `exited` best-effort regardless, which is the
  // cleanup path the rest of the suite relies on.
  const deleted = await page.request.delete(`/v1/instances/${instanceId}?force=1`);
  expect(deleted.ok()).toBe(true);
  await expect.poll(async () => (await page.request.get(`/v1/instances/${instanceId}`)).status()).toBe(404);
});
//# sourceMappingURL=data:application/json;charset=utf-8;base64,eyJ2ZXJzaW9uIjozLCJuYW1lcyI6WyJleHBlY3QiLCJ0ZXN0IiwibWtkaXIiLCJwYXRoIiwiZmlsZVVSTFRvUGF0aCIsImV4cGVjdENvb2tpZVNlc3Npb24iLCJsb2dpbiIsImhlcmUiLCJkaXJuYW1lIiwiaW1wb3J0IiwibWV0YSIsInVybCIsImV2aWRlbmNlIiwicHJvY2VzcyIsImVudiIsIlJFTVVEQV9FVklERU5DRSIsInNob3REaXIiLCJqb2luIiwic2hvdCIsInBhZ2UiLCJuYW1lIiwicmVjdXJzaXZlIiwic2NyZWVuc2hvdCIsImFuaW1hdGlvbnMiLCJhbnN3ZXJQZW5kaW5nQXBwcm92YWxzIiwiaW5zdGFuY2VJZCIsIml0ZW1zIiwiZXZhbHVhdGUiLCJpZCIsIl9ib2R5JGl0ZW1zIiwibGlzdCIsImZldGNoIiwiY3JlZGVudGlhbHMiLCJib2R5IiwianNvbiIsImZpbHRlciIsIml0ZW0iLCJwb2xsIiwibGVuZ3RoIiwidGltZW91dCIsIm1lc3NhZ2UiLCJ0b0JlR3JlYXRlclRoYW4iLCJhbnN3ZXJlZCIsIlNldCIsInBlbmRpbmciLCJzdGF0ZSIsIl9pdGVtJHJlcXVlc3QiLCJfaXRlbSRyZXF1ZXN0MiIsImhhcyIsIm9wdGlvbklkIiwicmVxdWVzdCIsIm9wdGlvbnMiLCJvayIsImlpZCIsImRpZ2VzdCIsInJlcyIsIm1ldGhvZCIsImhlYWRlcnMiLCJKU09OIiwic3RyaW5naWZ5IiwiYW5zd2VyIiwia2luZCIsImlucHV0RGlnZXN0IiwiYWRkIiwidG9CZSIsImRlc2NyaWJlIiwiY29uZmlndXJlIiwibW9kZSIsInN3ZWVwRmFrZU5vZGVJbnN0YW5jZXMiLCJob3N0SWQiLCJfYm9keSRpdGVtcyRmaW5kJGlkIiwiX2JvZHkkaXRlbXMyIiwiZmluZCIsImhvc3QiLCJsYWJlbCIsIl9ib2R5JGl0ZW1zMyIsImRlbGV0ZSIsImNhdGNoIiwidW5kZWZpbmVkIiwiYmVmb3JlQWxsIiwiYnJvd3NlciIsIkhVQl9FMkVfRVhURVJOQUwiLCJuZXdQYWdlIiwiY2xvc2UiLCJhZnRlckVhY2giLCJza2lwIiwiZm9sbG93VXJscyIsIm9uIiwic29ja2V0IiwiVVJMIiwicGF0aG5hbWUiLCJwdXNoIiwiZ2V0QnlSb2xlIiwiY2xpY2siLCJleGFjdCIsImdldEJ5VGVzdElkIiwidG9CZVZpc2libGUiLCJoYXNUZXh0IiwiZ2V0QnlUaXRsZSIsInRvQ29udGFpblRleHQiLCJsb2NhdG9yIiwiZ2V0QXR0cmlidXRlIiwic2VsZWN0T3B0aW9uIiwibm90IiwidG9IYXZlQ291bnQiLCJmaWxsIiwidG9CZUVuYWJsZWQiLCJ0b0hhdmVVUkwiLCJzZXNzaW9uUGF0aCIsImNyZWF0ZWRJZCIsInNwbGl0Iiwic3RvcmVkIiwiZ2V0IiwidHVpIiwiZXZlcnkiLCJzZWFyY2hQYXJhbXMiLCJyZWxvYWQiLCJnb3RvIiwib3duQXBwcm92YWwiLCJhcHByb3ZhbCIsInBvcCIsIl9ib2R5JGl0ZW1zNCIsIm1pbmUiLCJfaXRlbSRyZXF1ZXN0MyIsIl9pdGVtJHJlcXVlc3QkaW5wdXREaSIsIl9pdGVtJHJlcXVlc3Q0IiwidXBsb2FkcyIsInJlc3BvbnNlIiwic3RhdHVzIiwiYmFzZTY0IiwiYnl0ZXMiLCJVaW50OEFycmF5IiwiZnJvbSIsImF0b2IiLCJjaGFyIiwiY2hhckNvZGVBdCIsImZpbGUiLCJGaWxlIiwidHlwZSIsImRhdGEiLCJEYXRhVHJhbnNmZXIiLCJhcmVhIiwiZG9jdW1lbnQiLCJxdWVyeVNlbGVjdG9yIiwiZGlzcGF0Y2hFdmVudCIsIkNsaXBib2FyZEV2ZW50IiwiY2xpcGJvYXJkRGF0YSIsImJ1YmJsZXMiLCJ0b0NvbnRhaW4iLCJmaXJzdCIsIkhVQl9FMkVfV09SS1NQQUNFIiwidG9CZVRydXRoeSIsImhvc3RQaWNrZXIiLCJIVUJfRTJFX0hPU1RfSUQiLCJpbnB1dFZhbHVlIiwib2JzZXJ2ZXIiLCJjb250ZXh0IiwiZnJvemVuSG9zdHMiLCJyb3V0ZSIsImZ1bGZpbGwiLCJyZWdpc3RlcmVkIiwid2FpdEZvclJlc3BvbnNlIiwicmVnaXN0cmF0aW9uIiwic25hcHNob3QiLCJ3b3Jrc3BhY2UiLCJ3b3Jrc3BhY2VzIiwicm93Iiwid29ya3NwYWNlSWQiLCJ0b0hhdmVWYWx1ZSIsIm9ic2VydmVyT3B0aW9uIiwid2FpdEZvclJlcXVlc3QiLCJhdCIsInRvSGF2ZUF0dHJpYnV0ZSIsInRlcm1pbmFsSW5wdXQiLCJmb2N1cyIsInRvQmVGb2N1c2VkIiwia2V5Ym9hcmQiLCJwcmVzcyIsInJvb3QiLCJsaWZlY3ljbGUiLCJ1bnJlZ2lzdGVyZWQiLCJjb25maWd1cmVCb2RpZXMiLCJlbmRzV2l0aCIsInBvc3REYXRhSlNPTiIsInNsaWRlciIsImJveCIsImJvdW5kaW5nQm94IiwibW91c2UiLCJtb3ZlIiwieCIsIndpZHRoIiwieSIsImhlaWdodCIsImRvd24iLCJzdGVwcyIsInVwIiwic29tZSIsIl9ib2R5JHBheWxvYWQiLCJvcGVyYXRpb24iLCJwYXlsb2FkIiwiZWZmb3J0IiwiX2JvZHkkcGF5bG9hZDIiLCJzaGVsbCIsInNldFZpZXdwb3J0U2l6ZSIsIl9zZW5kcyQiLCJfc2VuZHMkMiIsIl9zZW5kcyRhdCIsImNvbW1hbmRzIiwicXVldWUiLCJzdGVlciIsImludGVycnVwdCIsImJlZm9yZSIsInRvSGF2ZVRleHQiLCJjIiwiX2MkcGF5bG9hZCIsInRvQmVHcmVhdGVyVGhhbk9yRXF1YWwiLCJzZW5kcyIsInByb21wdCIsInRvTWF0Y2hPYmplY3QiLCJhbHdheXMiLCJkZWxldGVkIiwic3RyZWFtZWQiLCJhbGxUZXh0Q29udGVudHMiLCJ0ZXh0IiwiaW5jbHVkZXMiXSwic291cmNlcyI6WyJodWItbGl2ZS5zcGVjLnRzIl0sInNvdXJjZXNDb250ZW50IjpbImltcG9ydCB7IGV4cGVjdCwgdGVzdCwgdHlwZSBQYWdlIH0gZnJvbSBcIkBwbGF5d3JpZ2h0L3Rlc3RcIjtcbmltcG9ydCB7IG1rZGlyIH0gZnJvbSBcIm5vZGU6ZnMvcHJvbWlzZXNcIjtcbmltcG9ydCBwYXRoIGZyb20gXCJub2RlOnBhdGhcIjtcbmltcG9ydCB7IGZpbGVVUkxUb1BhdGggfSBmcm9tIFwibm9kZTp1cmxcIjtcbmltcG9ydCB7IGV4cGVjdENvb2tpZVNlc3Npb24sIGxvZ2luIH0gZnJvbSBcIi4vaHViLWF1dGhcIjtcblxuY29uc3QgaGVyZSA9IHBhdGguZGlybmFtZShmaWxlVVJMVG9QYXRoKGltcG9ydC5tZXRhLnVybCkpO1xuY29uc3QgZXZpZGVuY2UgPSBwcm9jZXNzLmVudi5SRU1VREFfRVZJREVOQ0UgPT09IFwiMVwiO1xuY29uc3Qgc2hvdERpciA9IGV2aWRlbmNlXG4gID8gcGF0aC5qb2luKGhlcmUsIFwiLi4vLi4vLi4vZG9jcy9kZXNpZ24vZXZpZGVuY2VcIilcbiAgOiBwYXRoLmpvaW4oaGVyZSwgXCIuLi8uLi90ZXN0LXJlc3VsdHMvbmF0aXZlLXB0eS13ZWJcIik7XG5cbmFzeW5jIGZ1bmN0aW9uIHNob3QocGFnZTogUGFnZSwgbmFtZTogc3RyaW5nKSB7XG4gIGF3YWl0IG1rZGlyKHNob3REaXIsIHsgcmVjdXJzaXZlOiB0cnVlIH0pO1xuICBhd2FpdCBwYWdlLnNjcmVlbnNob3QoeyBwYXRoOiBwYXRoLmpvaW4oc2hvdERpciwgbmFtZSksIGFuaW1hdGlvbnM6IFwiZGlzYWJsZWRcIiB9KTtcbn1cblxuLyoqXG4gKiBBbnN3ZXIgZXZlcnkgcGVuZGluZyBhcHByb3ZhbC9xdWVzdGlvbiB0aGlzIGluc3RhbmNlIGN1cnJlbnRseSBoYXMuXG4gKlxuICogR2F0ZSBmbGFrZTogYSBsYXVuY2ggYXBwcm92YWwgaXMgam91cm5hbGVkIGNvbmN1cnJlbnRseSB3aXRoIHRoZSBjcmVhdGVcbiAqIHJlc3BvbnNlLCBzbyBhbiBpbnRlcmFjdGlvbnMgcG9sbCB0YWtlbiBpbW1lZGlhdGVseSBhZnRlciBuYXZpZ2F0aW9uIGNhblxuICogcmVhZCAwIHBlbmRpbmcg4oCUIHRoZSBvbGQgaGVscGVyIHBhc3NlZCBhcyBzb29uIGFzIHBlbmRpbmcgcmVhZCAwLCBiZWZvcmUgdGhlXG4gKiBhcHByb3ZhbCBleGlzdGVkLCBhbmQgdGhlIGxhdW5jaCBzdGF5ZWQgYmxvY2tlZC4gV2UgdGhlcmVmb3JlIHdhaXQgdW50aWwgYW5cbiAqIGludGVyYWN0aW9uIGZvciB0aGUgaW5zdGFuY2UgRVhJU1RTIGluIGFueSBzdGF0ZSAocHJvdmluZyB0aGUgbGF1bmNoXG4gKiBhcHByb3ZhbCB3YXMgY3JlYXRlZCDigJQgaXQgbWF5IGFscmVhZHkgYmUgYW5zd2VyZWQsIGUuZy4gdGhlIGhvb2stYXBwcm92YWxcbiAqIGNhc2UgcmVzb2x2ZXMgaXRzIGNhcmQgb24gL2FwcHJvdmFscyBiZWZvcmUgY2FsbGluZyB0aGlzIGhlbHBlciksIGFuc3dlclxuICogd2hhdGV2ZXIgaXMgc3RpbGwgcGVuZGluZywgdGhlbiB3YWl0IGZvciB0aGUgcGVuZGluZyBsaXN0IHRvIGNsZWFyLlxuICovXG5hc3luYyBmdW5jdGlvbiBhbnN3ZXJQZW5kaW5nQXBwcm92YWxzKHBhZ2U6IFBhZ2UsIGluc3RhbmNlSWQ6IHN0cmluZykge1xuICBjb25zdCBpdGVtcyA9ICgpID0+XG4gICAgcGFnZS5ldmFsdWF0ZShhc3luYyAoaWQpID0+IHtcbiAgICAgIGNvbnN0IGxpc3QgPSBhd2FpdCBmZXRjaChcIi92MS9pbnRlcmFjdGlvbnNcIiwgeyBjcmVkZW50aWFsczogXCJpbmNsdWRlXCIgfSk7XG4gICAgICBjb25zdCBib2R5ID0gKGF3YWl0IGxpc3QuanNvbigpKSBhcyB7XG4gICAgICAgIGl0ZW1zPzoge1xuICAgICAgICAgIGlkOiBzdHJpbmc7XG4gICAgICAgICAgaW5zdGFuY2VJZD86IHN0cmluZztcbiAgICAgICAgICBzdGF0ZT86IHN0cmluZztcbiAgICAgICAgICByZXF1ZXN0PzogeyBraW5kPzogc3RyaW5nOyBpbnB1dERpZ2VzdD86IHN0cmluZzsgb3B0aW9ucz86IHsgaWQ6IHN0cmluZyB9W10gfTtcbiAgICAgICAgfVtdO1xuICAgICAgfTtcbiAgICAgIHJldHVybiAoYm9keS5pdGVtcyA/PyBbXSkuZmlsdGVyKChpdGVtKSA9PiBpdGVtLmluc3RhbmNlSWQgPT09IGlkKTtcbiAgICB9LCBpbnN0YW5jZUlkKTtcblxuICBhd2FpdCBleHBlY3RcbiAgICAucG9sbChhc3luYyAoKSA9PiAoYXdhaXQgaXRlbXMoKSkubGVuZ3RoLCB7IHRpbWVvdXQ6IDIwXzAwMCwgbWVzc2FnZTogXCJsYXVuY2ggYXBwcm92YWwgZXhpc3RzXCIgfSlcbiAgICAudG9CZUdyZWF0ZXJUaGFuKDApO1xuXG4gIC8vIENsZWFyIGluc2lkZSB0aGUgcG9sbDogZXZlcnkgYXR0ZW1wdCByZS1saXN0cyB0aGUgcGVuZGluZyBzZXQgYW5kIGFuc3dlcnNcbiAgLy8gd2hhdGV2ZXIgaXMgc3RpbGwgb3Blbiwgc28gYSBsYXRlLXBvcHVsYXRlZCBvcHRpb25zIGFycmF5LCBhIHJlamVjdGVkXG4gIC8vIGFuc3dlciwgb3IgYSBzZWNvbmQgYXBwcm92YWwgY2Fubm90IHN0cmFuZCB0aGUgbGF1bmNoIHVudGlsIHRoZSB0aW1lb3V0LlxuICAvLyBBbiBpZCBpcyByZW1lbWJlcmVkIG9ubHkgYWZ0ZXIgaXRzIGFuc3dlciBQT1NUIHN1Y2NlZWRlZC5cbiAgY29uc3QgYW5zd2VyZWQgPSBuZXcgU2V0PHN0cmluZz4oKTtcbiAgYXdhaXQgZXhwZWN0XG4gICAgLnBvbGwoXG4gICAgICBhc3luYyAoKSA9PiB7XG4gICAgICAgIGNvbnN0IHBlbmRpbmcgPSAoYXdhaXQgaXRlbXMoKSkuZmlsdGVyKChpdGVtKSA9PiBpdGVtLnN0YXRlID09PSBcInBlbmRpbmdcIik7XG4gICAgICAgIGZvciAoY29uc3QgaXRlbSBvZiBwZW5kaW5nKSB7XG4gICAgICAgICAgaWYgKGFuc3dlcmVkLmhhcyhpdGVtLmlkKSkgY29udGludWU7XG4gICAgICAgICAgY29uc3Qgb3B0aW9uSWQgPSBpdGVtLnJlcXVlc3Q/Lm9wdGlvbnM/LlswXT8uaWQ7XG4gICAgICAgICAgaWYgKCFvcHRpb25JZCkgY29udGludWU7XG4gICAgICAgICAgY29uc3Qgb2sgPSBhd2FpdCBwYWdlLmV2YWx1YXRlKFxuICAgICAgICAgICAgYXN5bmMgKHsgaWlkLCBvcHRpb25JZCwgZGlnZXN0IH0pID0+IHtcbiAgICAgICAgICAgICAgY29uc3QgcmVzID0gYXdhaXQgZmV0Y2goYC92MS9pbnRlcmFjdGlvbnMvJHtpaWR9L2Fuc3dlcmAsIHtcbiAgICAgICAgICAgICAgICBtZXRob2Q6IFwiUE9TVFwiLFxuICAgICAgICAgICAgICAgIGNyZWRlbnRpYWxzOiBcImluY2x1ZGVcIixcbiAgICAgICAgICAgICAgICBoZWFkZXJzOiB7IFwiY29udGVudC10eXBlXCI6IFwiYXBwbGljYXRpb24vanNvblwiIH0sXG4gICAgICAgICAgICAgICAgYm9keTogSlNPTi5zdHJpbmdpZnkoe1xuICAgICAgICAgICAgICAgICAgYW5zd2VyOiB7IGtpbmQ6IFwiYXBwcm92YWxcIiwgb3B0aW9uSWQsIGlucHV0RGlnZXN0OiBkaWdlc3QgPz8gXCJcIiB9LFxuICAgICAgICAgICAgICAgIH0pLFxuICAgICAgICAgICAgICB9KTtcbiAgICAgICAgICAgICAgcmV0dXJuIHJlcy5vaztcbiAgICAgICAgICAgIH0sXG4gICAgICAgICAgICB7IGlpZDogaXRlbS5pZCwgb3B0aW9uSWQsIGRpZ2VzdDogaXRlbS5yZXF1ZXN0Py5pbnB1dERpZ2VzdCB9LFxuICAgICAgICAgICk7XG4gICAgICAgICAgaWYgKG9rKSBhbnN3ZXJlZC5hZGQoaXRlbS5pZCk7XG4gICAgICAgIH1cbiAgICAgICAgcmV0dXJuIHBlbmRpbmcubGVuZ3RoO1xuICAgICAgfSxcbiAgICAgIHsgdGltZW91dDogMjBfMDAwLCBtZXNzYWdlOiBcImFwcHJvdmFscyBjbGVhclwiIH0sXG4gICAgKVxuICAgIC50b0JlKDApO1xufVxuXG5cbnRlc3QuZGVzY3JpYmUuY29uZmlndXJlKHsgbW9kZTogXCJzZXJpYWxcIiB9KTtcblxuLyoqXG4gKiBHYXRlIGh5Z2llbmU6IHRoaXMgZmlsZSBjcmVhdGVzIGluc3RhbmNlcyBhbmQgb25seSB0d28gb2YgaXRzIGVpZ2h0IHRlc3RzXG4gKiBkZWxldGUgdGhlaXJzLCBzbyBieSB0aGUgc2Vjb25kIGhhbGYgdGhlIHNoYXJlZCBpbi1wcm9jZXNzIGZha2UgTm9kZSBpc1xuICogY2FycnlpbmcgdGhpcyBmaWxlJ3MgbGVmdG92ZXJzLCBhbmQgZWFybGllciBnYXRlIGZpbGVzIGxlYXZlIG1vcmUuIFRoZSBmYWtlXG4gKiBob3N0IGFkdmVydGlzZXMgYSBmaXhlZCBgbWF4SW5zdGFuY2VzOiA4YCAoYSBwcm9kdWN0IGNlaWxpbmcgb3RoZXIgc3BlY3NcbiAqIGRlbGliZXJhdGVseSBleGVyY2lzZSksIGFuZCBhIFBPU1QgcmVmdXNlZCB3aXRoIDQyMiBQTEFDRU1FTlRfVU5TQVRJU0ZJQUJMRVxuICogbGVhdmVzIE5ld1Nlc3Npb25QYWdlIG9uIC9zZXNzaW9ucy9uZXcg4oCUIHRoZSBnYXRlIGZsYWtlIGF0dHJpYnV0ZWQgdG8gbGluZXNcbiAqIDQ0Ny80ODEgKGl0IGFjY3VtdWxhdGVzOiBpbiBhIDEwLXJlcGVhdCBsb29wIGFnYWluc3Qgb25lIGxpdmUgc2VydmVyIHRoZVxuICogOXRoIGFuZCAxMHRoIHJ1bnMgZmFpbGVkIGRldGVybWluaXN0aWNhbGx5KS4gU3dlZXAgdGhlIHNoYXJlZCBob3N0IGJlZm9yZVxuICogdGhlIGZpcnN0IHRlc3QgYW5kIGFmdGVyIGV2ZXJ5IHRlc3Qgc28gZWFjaCBydW4gc3RhcnRzIHdpdGggcmVhbCBoZWFkcm9vbS4gVGhlIGV4dGVybmFsLU5vZGUgZmxvd1xuICogb3ducyBpdHMgb3duIGhvc3QgYW5kIG11c3QgbmV2ZXIgYmUgc3dlcHQuXG4gKi9cbmFzeW5jIGZ1bmN0aW9uIHN3ZWVwRmFrZU5vZGVJbnN0YW5jZXMocGFnZTogUGFnZSkge1xuICBjb25zdCBob3N0SWQgPSBhd2FpdCBwYWdlLmV2YWx1YXRlKGFzeW5jICgpID0+IHtcbiAgICBjb25zdCByZXMgPSBhd2FpdCBmZXRjaChcIi92MS9ob3N0c1wiLCB7IGNyZWRlbnRpYWxzOiBcImluY2x1ZGVcIiB9KTtcbiAgICBpZiAoIXJlcy5vaykgcmV0dXJuIG51bGw7XG4gICAgY29uc3QgYm9keSA9IChhd2FpdCByZXMuanNvbigpKSBhcyB7XG4gICAgICBpdGVtcz86IHsgaWQ/OiBzdHJpbmc7IGxhYmVsPzogc3RyaW5nIH1bXTtcbiAgICB9O1xuICAgIHJldHVybiBib2R5Lml0ZW1zPy5maW5kKChob3N0KSA9PiBob3N0LmxhYmVsID09PSBcImUyZS1mYWtlLW5vZGVcIik/LmlkID8/IG51bGw7XG4gIH0pO1xuICBpZiAoIWhvc3RJZCkgcmV0dXJuO1xuICBjb25zdCBpdGVtcyA9IGF3YWl0IHBhZ2UuZXZhbHVhdGUoYXN5bmMgKGlkKSA9PiB7XG4gICAgY29uc3QgcmVzID0gYXdhaXQgZmV0Y2goXCIvdjEvaW5zdGFuY2VzXCIsIHsgY3JlZGVudGlhbHM6IFwiaW5jbHVkZVwiIH0pO1xuICAgIGlmICghcmVzLm9rKSByZXR1cm4gW107XG4gICAgY29uc3QgYm9keSA9IChhd2FpdCByZXMuanNvbigpKSBhcyB7XG4gICAgICBpdGVtcz86IHsgaW5zdGFuY2VJZD86IHN0cmluZzsgaG9zdElkPzogc3RyaW5nIH1bXTtcbiAgICB9O1xuICAgIHJldHVybiAoYm9keS5pdGVtcyA/PyBbXSkuZmlsdGVyKChpdGVtKSA9PiBpdGVtLmhvc3RJZCA9PT0gaWQgJiYgaXRlbS5pbnN0YW5jZUlkKTtcbiAgfSwgaG9zdElkKTtcbiAgZm9yIChjb25zdCBpdGVtIG9mIGl0ZW1zKSB7XG4gICAgYXdhaXQgcGFnZS5yZXF1ZXN0LmRlbGV0ZShgL3YxL2luc3RhbmNlcy8ke2l0ZW0uaW5zdGFuY2VJZH0/Zm9yY2U9MWApLmNhdGNoKCgpID0+IHVuZGVmaW5lZCk7XG4gIH1cbn1cblxudGVzdC5iZWZvcmVBbGwoYXN5bmMgKHsgYnJvd3NlciB9KSA9PiB7XG4gIGlmIChwcm9jZXNzLmVudi5IVUJfRTJFX0VYVEVSTkFMID09PSBcIjFcIikgcmV0dXJuO1xuICBjb25zdCBwYWdlID0gYXdhaXQgYnJvd3Nlci5uZXdQYWdlKCk7XG4gIHRyeSB7XG4gICAgYXdhaXQgbG9naW4ocGFnZSk7XG4gICAgYXdhaXQgc3dlZXBGYWtlTm9kZUluc3RhbmNlcyhwYWdlKTtcbiAgfSBmaW5hbGx5IHtcbiAgICBhd2FpdCBwYWdlLmNsb3NlKCk7XG4gIH1cbn0pO1xuXG50ZXN0LmFmdGVyRWFjaChhc3luYyAoeyBwYWdlIH0pID0+IHtcbiAgaWYgKHByb2Nlc3MuZW52LkhVQl9FMkVfRVhURVJOQUwgPT09IFwiMVwiKSByZXR1cm47XG4gIC8vIEFuIHVuYXV0aGVudGljYXRlZCBwYWdlIChhIGZhaWx1cmUgYmVmb3JlIGxvZ2luKSBzaW1wbHkgc3dlZXBzIG5vdGhpbmcuXG4gIGF3YWl0IHN3ZWVwRmFrZU5vZGVJbnN0YW5jZXMocGFnZSkuY2F0Y2goKCkgPT4gdW5kZWZpbmVkKTtcbn0pO1xuXG50ZXN0KFwiZGV2aWNlIGxvZ2luLCBob3N0cywgY3JlYXRlL3NlbmQvY2xvc2UsIGZvbGxvdywgYXBwcm92YWxzXCIsIGFzeW5jICh7IHBhZ2UgfSkgPT4ge1xuICB0ZXN0LnNraXAocHJvY2Vzcy5lbnYuSFVCX0UyRV9FWFRFUk5BTCA9PT0gXCIxXCIsIFwiRXh0ZXJuYWwgTm9kZSBpcyBjb3ZlcmVkIGJ5IHRoZSByZWFsIHNoZWxsIHdvcmtzcGFjZSBmbG93XCIpO1xuICBjb25zdCBmb2xsb3dVcmxzOiBzdHJpbmdbXSA9IFtdO1xuICBwYWdlLm9uKFwid2Vic29ja2V0XCIsIChzb2NrZXQpID0+IHtcbiAgICAvLyBWaXRlIGF1dGhlbnRpY2F0ZXMgSE1SIHdpdGggaXRzIG93biB0b2tlbjsgcmVzdHJpY3QgdGhpcyBjaGVjayB0byBIdWIuXG4gICAgaWYgKG5ldyBVUkwoc29ja2V0LnVybCgpKS5wYXRobmFtZSA9PT0gXCIvdjEvZm9sbG93XCIpIGZvbGxvd1VybHMucHVzaChzb2NrZXQudXJsKCkpO1xuICB9KTtcbiAgYXdhaXQgbG9naW4ocGFnZSk7XG5cbiAgYXdhaXQgcGFnZS5nZXRCeVJvbGUoXCJidXR0b25cIiwgeyBuYW1lOiBcIueuoeeQhlwiIH0pLmNsaWNrKCk7XG4gIGF3YWl0IHBhZ2UuZ2V0QnlSb2xlKFwibWVudWl0ZW1cIiwgeyBuYW1lOiBcIuS4u+aculwiLCBleGFjdDogdHJ1ZSB9KS5jbGljaygpO1xuICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcImhvc3RzLXBhZ2VcIikpLnRvQmVWaXNpYmxlKCk7XG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwiaG9zdC1yb3dcIikuZmlsdGVyKHsgaGFzVGV4dDogXCJlMmUtZmFrZS1ub2RlXCIgfSkpLnRvQmVWaXNpYmxlKHtcbiAgICB0aW1lb3V0OiAyMF8wMDAsXG4gIH0pO1xuXG4gIGF3YWl0IHBhZ2UuZ2V0QnlUaXRsZShcIuaWsOW7ulwiLCB7IGV4YWN0OiB0cnVlIH0pLmNsaWNrKCk7XG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwibmV3LXNlc3Npb24tc2hlZXRcIikpLnRvQmVWaXNpYmxlKCk7XG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwibmV3LXNlc3Npb24taG9zdFwiKSkudG9Db250YWluVGV4dChcImUyZS1mYWtlLW5vZGVcIiwgeyB0aW1lb3V0OiAyMF8wMDAgfSk7XG4gIGNvbnN0IGhvc3QgPSBhd2FpdCBwYWdlLmdldEJ5VGVzdElkKFwibmV3LXNlc3Npb24taG9zdFwiKS5sb2NhdG9yKFwib3B0aW9uXCIpLmZpbHRlcih7IGhhc1RleHQ6IFwiZTJlLWZha2Utbm9kZVwiIH0pLmdldEF0dHJpYnV0ZShcInZhbHVlXCIpO1xuICBhd2FpdCBwYWdlLmdldEJ5VGVzdElkKFwibmV3LXNlc3Npb24taG9zdFwiKS5zZWxlY3RPcHRpb24oaG9zdCEpO1xuICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcIm5ldy1zZXNzaW9uLXdvcmtzcGFjZVwiKS5sb2NhdG9yKFwib3B0aW9uXCIpKS5ub3QudG9IYXZlQ291bnQoMCk7XG4gIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1hZHZhbmNlZFwiKS5jbGljaygpO1xuICBhd2FpdCBwYWdlLmdldEJ5VGVzdElkKFwibmV3LXNlc3Npb24tdHVpXCIpLnNlbGVjdE9wdGlvbihcImRlZmF1bHRcIik7XG4gIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1wcm9tcHRcIikuZmlsbChcImhlbGxvIGZyb20gd2ViIGh1YlwiKTtcbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1zdGFydFwiKSkudG9CZUVuYWJsZWQoKTtcbiAgYXdhaXQgcGFnZS5nZXRCeVRlc3RJZChcIm5ldy1zZXNzaW9uLXN0YXJ0XCIpLmNsaWNrKCk7XG4gIGF3YWl0IGV4cGVjdChwYWdlKS50b0hhdmVVUkwoL1xcL3NcXC8vLCB7IHRpbWVvdXQ6IDIwXzAwMCB9KTtcbiAgY29uc3Qgc2Vzc2lvblBhdGggPSBuZXcgVVJMKHBhZ2UudXJsKCkpLnBhdGhuYW1lO1xuICBjb25zdCBjcmVhdGVkSWQgPSBzZXNzaW9uUGF0aC5zcGxpdChcIi9cIilbMl07XG4gIGNvbnN0IHN0b3JlZCA9IGF3YWl0IChhd2FpdCBwYWdlLnJlcXVlc3QuZ2V0KGAvdjEvaW5zdGFuY2VzLyR7Y3JlYXRlZElkfWApKS5qc29uKCk7XG4gIGV4cGVjdChzdG9yZWQudHVpKS50b0JlKFwiZGVmYXVsdFwiKTtcblxuICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcInNlc3Npb24tcGFnZVwiKSkudG9CZVZpc2libGUoKTtcbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJtZXNzYWdlXCIpLmZpbHRlcih7IGhhc1RleHQ6IC9eWW91LyB9KSkudG9Db250YWluVGV4dChcImhlbGxvIGZyb20gd2ViIGh1YlwiLCB7XG4gICAgdGltZW91dDogMjBfMDAwLFxuICB9KTtcbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJtZXNzYWdlXCIpLmZpbHRlcih7IGhhc1RleHQ6IFwiZWNobzogaGVsbG8gZnJvbSB3ZWIgaHViXCIgfSkpLnRvSGF2ZUNvdW50KDEsIHtcbiAgICB0aW1lb3V0OiAyMF8wMDAsXG4gIH0pO1xuICBhd2FpdCBleHBlY3QucG9sbCgoKSA9PiBmb2xsb3dVcmxzLmxlbmd0aCkudG9CZUdyZWF0ZXJUaGFuKDApO1xuICBleHBlY3QoZm9sbG93VXJscy5ldmVyeSgodXJsKSA9PiAhbmV3IFVSTCh1cmwpLnNlYXJjaFBhcmFtcy5oYXMoXCJ0b2tlblwiKSkpLnRvQmUodHJ1ZSk7XG4gIGF3YWl0IHBhZ2UucmVsb2FkKCk7XG4gIGF3YWl0IGV4cGVjdENvb2tpZVNlc3Npb24ocGFnZSk7XG4gIC8vIFJlbG9hZCB3YWl0cyBmb3IgY29va2llLWJhY2tlZCBib290c3RyYXAgYW5kIHRoZSBkdXJhYmxlIGpvdXJuYWwgcmVhZCBhZ2Fpbi5cbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJtZXNzYWdlXCIpLmZpbHRlcih7IGhhc1RleHQ6IFwiZWNobzogaGVsbG8gZnJvbSB3ZWIgaHViXCIgfSkpLnRvSGF2ZUNvdW50KDEsIHtcbiAgICB0aW1lb3V0OiAyMF8wMDAsXG4gIH0pO1xuICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcImNvbXBvc2VyLWJhclwiKSkudG9CZVZpc2libGUoKTtcblxuICBhd2FpdCBwYWdlLmdvdG8oXCIvYXBwcm92YWxzXCIpO1xuICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcImFwcHJvdmFscy1wYWdlXCIpKS50b0JlVmlzaWJsZSgpO1xuICAvLyBHYXRlIGZsYWtlOiBldmVyeSBjcmVhdGUgb24gdGhlIHNoYXJlZCBmYWtlIE5vZGUgcmFpc2VzIGFuIGlkZW50aWNhbGx5XG4gIC8vIGxhYmVsbGVkIFwiZWNobyBlMmVcIiBjYXJkLCBzbyBhIGxlZnRvdmVyIGNhcmQgZnJvbSBhIHJldHJpZWQgYXR0ZW1wdCAodGhlXG4gIC8vIENJIGNvbmZpZyByZXRyaWVzIG9uY2UgYWdhaW5zdCB0aGUgU0FNRSBsb25nLWxpdmVkIGZha2UgTm9kZSkgb3IgZnJvbSBhXG4gIC8vIG5laWdoYm91cmluZyBzcGVjIHdob3NlIGNsZWFudXAgcmFjZWQgdGhlIGdhdGUgY2FuIHNpdCBpbiB0aGlzIHF1ZXVlIGFuZFxuICAvLyBtYWtlIHRoZSB0ZXh0LW9ubHkgZmlsdGVyIHN0cmljdCAoMyByb3dzKS4gT3duIHRoZSByb3c6IHNjb3BlIGl0IHRvIFRISVNcbiAgLy8gdGVzdCdzIGluc3RhbmNlIHRocm91Z2ggdGhlIHJvdydzIHNlc3Npb24gbGluay5cbiAgY29uc3Qgb3duQXBwcm92YWwgPSAoKSA9PlxuICAgIHBhZ2VcbiAgICAgIC5nZXRCeVRlc3RJZChcImFwcHJvdmFsLXJvd1wiKVxuICAgICAgLmZpbHRlcih7IGhhczogcGFnZS5sb2NhdG9yKGBhW2hyZWY9XCIvcy8ke2NyZWF0ZWRJZH1cIl1gKSB9KTtcbiAgY29uc3QgYXBwcm92YWwgPSBvd25BcHByb3ZhbCgpLmZpbHRlcih7IGhhc1RleHQ6IFwiZWNobyBlMmVcIiB9KTtcbiAgYXdhaXQgZXhwZWN0KGFwcHJvdmFsKS50b0JlVmlzaWJsZSh7IHRpbWVvdXQ6IDIwXzAwMCB9KTtcbiAgYXdhaXQgYXBwcm92YWwuZ2V0QnlSb2xlKFwiYnV0dG9uXCIsIHsgbmFtZTogXCLlhYHorrjkuIDmrKFcIiB9KS5jbGljaygpO1xuICBhd2FpdCBleHBlY3Qob3duQXBwcm92YWwoKSkudG9IYXZlQ291bnQoMCwge1xuICAgIHRpbWVvdXQ6IDIwXzAwMCxcbiAgfSk7XG5cbiAgYXdhaXQgcGFnZS5nb3RvKHNlc3Npb25QYXRoKTtcbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJzZXNzaW9uLXBhZ2VcIikpLnRvQmVWaXNpYmxlKCk7XG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwiY29tcG9zZXItYmFyXCIpKS50b0JlVmlzaWJsZSgpO1xuICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcImNvbXBvc2VyLWlucHV0XCIpKS50b0JlRW5hYmxlZCgpO1xuICBhd2FpdCBwYWdlLmdldEJ5VGVzdElkKFwiY29tcG9zZXItaW5wdXRcIikuZmlsbChcInNlY29uZCB0dXJuXCIpO1xuICBhd2FpdCBwYWdlLmdldEJ5VGVzdElkKFwiY29tcG9zZXItc2VuZFwiKS5jbGljaygpO1xuICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcIm1lc3NhZ2VcIikuZmlsdGVyKHsgaGFzVGV4dDogXCJlY2hvOiBzZWNvbmQgdHVyblwiIH0pKS50b0JlVmlzaWJsZSh7XG4gICAgdGltZW91dDogMjBfMDAwLFxuICB9KTtcblxuICBhd2FpdCBwYWdlLmdldEJ5Um9sZShcImJ1dHRvblwiLCB7IG5hbWU6IFwiU3RvcFwiIH0pLmNsaWNrKCk7XG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwic2Vzc2lvbi1wYWdlXCIpKS50b0JlVmlzaWJsZSgpO1xuICBleHBlY3QoZm9sbG93VXJscy5ldmVyeSgodXJsKSA9PiAhbmV3IFVSTCh1cmwpLnNlYXJjaFBhcmFtcy5oYXMoXCJ0b2tlblwiKSkpLnRvQmUodHJ1ZSk7XG59KTtcblxuLyoqXG4gKiBELTAyNzogYW4gaW1hZ2UgcGFzdGVkIGludG8gdGhlIGNvbXBvc2VyIGlzIHN0YWdlZCBvbiB0aGUgSHViIGFuZCBpdHNcbiAqIG1ldGFkYXRhIHJlYWNoZXMgdGhlIE5vZGUsIHdpdGggdGhlIGJ5dGVzIG5ldmVyIGVudGVyaW5nIGEgY29tbWFuZCBmcmFtZS5cbiAqL1xudGVzdChcInBhc3RlIGFuIGltYWdlOiBpdCBzdGFnZXMgb24gdGhlIEh1YiBhbmQgaXRzIG1ldGFkYXRhIHJlYWNoZXMgdGhlIE5vZGVcIiwgYXN5bmMgKHsgcGFnZSB9KSA9PiB7XG4gIHRlc3Quc2tpcChwcm9jZXNzLmVudi5IVUJfRTJFX0VYVEVSTkFMID09PSBcIjFcIiwgXCJOZWVkcyB0aGUgaW4tcHJvY2VzcyBmYWtlIE5vZGVcIik7XG4gIGF3YWl0IGxvZ2luKHBhZ2UpO1xuXG4gIGF3YWl0IHBhZ2UuZ2V0QnlUaXRsZShcIuaWsOW7ulwiLCB7IGV4YWN0OiB0cnVlIH0pLmNsaWNrKCk7XG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwibmV3LXNlc3Npb24tc2hlZXRcIikpLnRvQmVWaXNpYmxlKCk7XG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwibmV3LXNlc3Npb24taG9zdFwiKSkudG9Db250YWluVGV4dChcImUyZS1mYWtlLW5vZGVcIiwgeyB0aW1lb3V0OiAyMF8wMDAgfSk7XG4gIGNvbnN0IGhvc3QgPSBhd2FpdCBwYWdlXG4gICAgLmdldEJ5VGVzdElkKFwibmV3LXNlc3Npb24taG9zdFwiKVxuICAgIC5sb2NhdG9yKFwib3B0aW9uXCIpXG4gICAgLmZpbHRlcih7IGhhc1RleHQ6IFwiZTJlLWZha2Utbm9kZVwiIH0pXG4gICAgLmdldEF0dHJpYnV0ZShcInZhbHVlXCIpO1xuICBhd2FpdCBwYWdlLmdldEJ5VGVzdElkKFwibmV3LXNlc3Npb24taG9zdFwiKS5zZWxlY3RPcHRpb24oaG9zdCEpO1xuICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcIm5ldy1zZXNzaW9uLXdvcmtzcGFjZVwiKS5sb2NhdG9yKFwib3B0aW9uXCIpKS5ub3QudG9IYXZlQ291bnQoMCk7XG4gIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1wcm9tcHRcIikuZmlsbChcImF0dGFjaG1lbnQgc2Vzc2lvblwiKTtcbiAgYXdhaXQgcGFnZS5nZXRCeVRlc3RJZChcIm5ldy1zZXNzaW9uLXN0YXJ0XCIpLmNsaWNrKCk7XG4gIGF3YWl0IGV4cGVjdChwYWdlKS50b0hhdmVVUkwoL1xcL3NcXC8vLCB7IHRpbWVvdXQ6IDIwXzAwMCB9KTtcbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJzZXNzaW9uLXBhZ2VcIikpLnRvQmVWaXNpYmxlKCk7XG5cbiAgLy8gVGhlIGZha2UgTm9kZSByYWlzZXMgYW4gYXBwcm92YWwgb24gZXZlcnkgY3JlYXRlLCBhbmQgYSBwZW5kaW5nIG9uZSBob2xkc1xuICAvLyB0aGlzIGluc3RhbmNlJ3MgY29tcG9zZXIgZGlzYWJsZWQuIEFuc3dlciBleGFjdGx5IHRoaXMgaW5zdGFuY2Unc1xuICAvLyBhcHByb3ZhbHMgdGhyb3VnaCB0aGUgQVBJOiB0aGUgVUkgcm93cyBhcmUgYWxsIGxhYmVsbGVkIGFsaWtlLCBzbyBwaWNraW5nXG4gIC8vIHRoZSByaWdodCBvbmUgYnkgdGV4dCBpcyBub3QgcmVsaWFibGUgaGVyZS5cbiAgY29uc3QgaW5zdGFuY2VJZCA9IG5ldyBVUkwocGFnZS51cmwoKSkucGF0aG5hbWUuc3BsaXQoXCIvXCIpLnBvcCgpIGFzIHN0cmluZztcbiAgYXdhaXQgZXhwZWN0XG4gICAgLnBvbGwoXG4gICAgICBhc3luYyAoKSA9PlxuICAgICAgICBhd2FpdCBwYWdlLmV2YWx1YXRlKGFzeW5jIChpZCkgPT4ge1xuICAgICAgICAgIGNvbnN0IGxpc3QgPSBhd2FpdCBmZXRjaChcIi92MS9pbnRlcmFjdGlvbnNcIiwgeyBjcmVkZW50aWFsczogXCJpbmNsdWRlXCIgfSk7XG4gICAgICAgICAgY29uc3QgYm9keSA9IChhd2FpdCBsaXN0Lmpzb24oKSkgYXMge1xuICAgICAgICAgICAgaXRlbXM/OiB7XG4gICAgICAgICAgICAgIGlkOiBzdHJpbmc7XG4gICAgICAgICAgICAgIGluc3RhbmNlSWQ/OiBzdHJpbmc7XG4gICAgICAgICAgICAgIHN0YXRlPzogc3RyaW5nO1xuICAgICAgICAgICAgICByZXF1ZXN0PzogeyBraW5kPzogc3RyaW5nOyBpbnB1dERpZ2VzdD86IHN0cmluZzsgb3B0aW9ucz86IHsgaWQ6IHN0cmluZyB9W10gfTtcbiAgICAgICAgICAgIH1bXTtcbiAgICAgICAgICB9O1xuICAgICAgICAgIGNvbnN0IG1pbmUgPSAoYm9keS5pdGVtcyA/PyBbXSkuZmlsdGVyKFxuICAgICAgICAgICAgKGl0ZW0pID0+IGl0ZW0uaW5zdGFuY2VJZCA9PT0gaWQgJiYgaXRlbS5zdGF0ZSA9PT0gXCJwZW5kaW5nXCIsXG4gICAgICAgICAgKTtcbiAgICAgICAgICBmb3IgKGNvbnN0IGl0ZW0gb2YgbWluZSkge1xuICAgICAgICAgICAgY29uc3Qgb3B0aW9uSWQgPSBpdGVtLnJlcXVlc3Q/Lm9wdGlvbnM/LlswXT8uaWQ7XG4gICAgICAgICAgICBpZiAoIW9wdGlvbklkKSBjb250aW51ZTtcbiAgICAgICAgICAgIGF3YWl0IGZldGNoKGAvdjEvaW50ZXJhY3Rpb25zLyR7aXRlbS5pZH0vYW5zd2VyYCwge1xuICAgICAgICAgICAgICBtZXRob2Q6IFwiUE9TVFwiLFxuICAgICAgICAgICAgICBjcmVkZW50aWFsczogXCJpbmNsdWRlXCIsXG4gICAgICAgICAgICAgIGhlYWRlcnM6IHsgXCJjb250ZW50LXR5cGVcIjogXCJhcHBsaWNhdGlvbi9qc29uXCIgfSxcbiAgICAgICAgICAgICAgYm9keTogSlNPTi5zdHJpbmdpZnkoe1xuICAgICAgICAgICAgICAgIGFuc3dlcjoge1xuICAgICAgICAgICAgICAgICAga2luZDogXCJhcHByb3ZhbFwiLFxuICAgICAgICAgICAgICAgICAgb3B0aW9uSWQsXG4gICAgICAgICAgICAgICAgICBpbnB1dERpZ2VzdDogaXRlbS5yZXF1ZXN0Py5pbnB1dERpZ2VzdCA/PyBcIlwiLFxuICAgICAgICAgICAgICAgIH0sXG4gICAgICAgICAgICAgIH0pLFxuICAgICAgICAgICAgfSk7XG4gICAgICAgICAgfVxuICAgICAgICAgIHJldHVybiBtaW5lLmxlbmd0aDtcbiAgICAgICAgfSwgaW5zdGFuY2VJZCksXG4gICAgICB7IHRpbWVvdXQ6IDIwXzAwMCB9LFxuICAgIClcbiAgICAudG9CZSgwKTtcbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJjb21wb3Nlci1pbnB1dFwiKSkudG9CZUVuYWJsZWQoeyB0aW1lb3V0OiAyMF8wMDAgfSk7XG5cbiAgLy8gQSByZWFsIDF4MSByZWQgUE5HLCBwYXN0ZWQgdGhlIHdheSBhIGJyb3dzZXIgZGVsaXZlcnMgb25lLlxuICBjb25zdCB1cGxvYWRzOiBudW1iZXJbXSA9IFtdO1xuICBwYWdlLm9uKFwicmVzcG9uc2VcIiwgKHJlc3BvbnNlKSA9PiB7XG4gICAgaWYgKG5ldyBVUkwocmVzcG9uc2UudXJsKCkpLnBhdGhuYW1lID09PSBcIi92MS9vYmplY3RzXCIpIHVwbG9hZHMucHVzaChyZXNwb25zZS5zdGF0dXMoKSk7XG4gIH0pO1xuICBhd2FpdCBwYWdlLmdldEJ5VGVzdElkKFwiY29tcG9zZXItaW5wdXRcIikuZmlsbChcIndoYXQgY29sb3VyIGlzIHRoZSBpbWFnZT9cIik7XG4gIGF3YWl0IHBhZ2UuZXZhbHVhdGUoYXN5bmMgKCkgPT4ge1xuICAgIGNvbnN0IGJhc2U2NCA9XG4gICAgICBcImlWQk9SdzBLR2dvQUFBQU5TVWhFVWdBQUFBRUFBQUFCQ0FJQUFBQ1FkMVBlQUFBQURVbEVRVlI0Mm1QOHo4QlFEd0FFaFFHQWhLbU1JUUFBQUFCSlJVNUVya0pnZ2c9PVwiO1xuICAgIGNvbnN0IGJ5dGVzID0gVWludDhBcnJheS5mcm9tKGF0b2IoYmFzZTY0KSwgKGNoYXIpID0+IGNoYXIuY2hhckNvZGVBdCgwKSk7XG4gICAgY29uc3QgZmlsZSA9IG5ldyBGaWxlKFtieXRlc10sIFwicmVkLnBuZ1wiLCB7IHR5cGU6IFwiaW1hZ2UvcG5nXCIgfSk7XG4gICAgY29uc3QgZGF0YSA9IG5ldyBEYXRhVHJhbnNmZXIoKTtcbiAgICBkYXRhLml0ZW1zLmFkZChmaWxlKTtcbiAgICBjb25zdCBhcmVhID0gZG9jdW1lbnQucXVlcnlTZWxlY3RvcihcIltkYXRhLXRlc3RpZD0nY29tcG9zZXItaW5wdXQnXVwiKTtcbiAgICBhcmVhPy5kaXNwYXRjaEV2ZW50KG5ldyBDbGlwYm9hcmRFdmVudChcInBhc3RlXCIsIHsgY2xpcGJvYXJkRGF0YTogZGF0YSwgYnViYmxlczogdHJ1ZSB9KSk7XG4gIH0pO1xuXG4gIC8vIFRoZSBjaGlwIGFwcGVhcnMsIHRoZSB1cGxvYWQgc3VjY2VlZHMsIGFuZCBvbmx5IHRoZW4gY2FuIHRoZSBzZW5kIGdvLlxuICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcImF0dGFjaG1lbnQtY2hpcFwiKSkudG9CZVZpc2libGUoeyB0aW1lb3V0OiAyMF8wMDAgfSk7XG4gIGF3YWl0IGV4cGVjdC5wb2xsKCgpID0+IHVwbG9hZHMsIHsgdGltZW91dDogMjBfMDAwIH0pLnRvQ29udGFpbigyMDApO1xuICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcImNvbXBvc2VyLXNlbmRcIikpLnRvQmVFbmFibGVkKHsgdGltZW91dDogMjBfMDAwIH0pO1xuICBhd2FpdCBwYWdlLmdldEJ5VGVzdElkKFwiY29tcG9zZXItc2VuZFwiKS5jbGljaygpO1xuXG4gIC8vIFRoZSBOb2RlIGVjaG9lcyB0aGUgcmVzb2x2ZWQgbWVkaWEgdHlwZSwgcHJvdmluZyB0aGUgbWV0YWRhdGEgYXJyaXZlZC5cbiAgYXdhaXQgZXhwZWN0KFxuICAgIHBhZ2UuZ2V0QnlUZXN0SWQoXCJtZXNzYWdlXCIpLmZpbHRlcih7IGhhc1RleHQ6IFwiW2F0dGFjaG1lbnRzOiBpbWFnZS9wbmddXCIgfSksXG4gICkudG9CZVZpc2libGUoeyB0aW1lb3V0OiAyMF8wMDAgfSk7XG4gIC8vIFRoZSBjaGlwIGlzIGNvbnN1bWVkIGJ5IHRoZSBzZW5kIGFuZCB0aGUgYnViYmxlIGtlZXBzIHRoZSB0aHVtYm5haWwuXG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwiYXR0YWNobWVudC1jaGlwXCIpKS50b0hhdmVDb3VudCgwKTtcbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJzZW50LWF0dGFjaG1lbnRzXCIpLmZpcnN0KCkpLnRvQmVWaXNpYmxlKCk7XG59KTtcblxudGVzdChcInJlYWwgTm9kZTogcmVnaXN0ZXIgYSBwcm9qZWN0LCBjcmVhdGUgYSBzaGVsbCBpbiBpdCwgY2xvc2UgYW5kIHVucmVnaXN0ZXJcIiwgYXN5bmMgKHsgcGFnZSB9KSA9PiB7XG4gIHRlc3Quc2tpcChwcm9jZXNzLmVudi5IVUJfRTJFX0VYVEVSTkFMICE9PSBcIjFcIiwgXCJSZXF1aXJlcyB0aGUgb3BlcmF0b3IncyByZW11ZGEgZGV2XCIpO1xuICBjb25zdCBwYXRoID0gcHJvY2Vzcy5lbnYuSFVCX0UyRV9XT1JLU1BBQ0U7XG4gIGV4cGVjdChwYXRoLCBcIlNldCBIVUJfRTJFX1dPUktTUEFDRSB0byBhbiBleGlzdGluZyBhbGxvd2VkIGRpcmVjdG9yeSBvbiB0aGUgTm9kZVwiKS50b0JlVHJ1dGh5KCk7XG4gIGNvbnN0IGZvbGxvd1VybHM6IHN0cmluZ1tdID0gW107XG4gIHBhZ2Uub24oXCJ3ZWJzb2NrZXRcIiwgKHNvY2tldCkgPT4ge1xuICAgIGlmIChuZXcgVVJMKHNvY2tldC51cmwoKSkucGF0aG5hbWUgPT09IFwiL3YxL2ZvbGxvd1wiKSBmb2xsb3dVcmxzLnB1c2goc29ja2V0LnVybCgpKTtcbiAgfSk7XG4gIGF3YWl0IGxvZ2luKHBhZ2UpO1xuICBhd2FpdCBwYWdlLmdvdG8oXCIvc2Vzc2lvbnMvbmV3XCIpO1xuICBjb25zdCBob3N0UGlja2VyID0gcGFnZS5nZXRCeVRlc3RJZChcIm5ldy1zZXNzaW9uLWhvc3RcIik7XG4gIGF3YWl0IGV4cGVjdChob3N0UGlja2VyLmxvY2F0b3IoXCJvcHRpb25cIikpLm5vdC50b0hhdmVDb3VudCgwKTtcbiAgaWYgKHByb2Nlc3MuZW52LkhVQl9FMkVfSE9TVF9JRCkgYXdhaXQgaG9zdFBpY2tlci5zZWxlY3RPcHRpb24ocHJvY2Vzcy5lbnYuSFVCX0UyRV9IT1NUX0lEKTtcbiAgY29uc3QgaG9zdElkID0gYXdhaXQgaG9zdFBpY2tlci5pbnB1dFZhbHVlKCk7XG4gIGNvbnN0IG9ic2VydmVyID0gYXdhaXQgcGFnZS5jb250ZXh0KCkubmV3UGFnZSgpO1xuICBhd2FpdCBvYnNlcnZlci5nb3RvKFwiL3Nlc3Npb25zL25ld1wiKTtcbiAgYXdhaXQgb2JzZXJ2ZXIuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1ob3N0XCIpLnNlbGVjdE9wdGlvbihob3N0SWQpO1xuICBjb25zdCBmcm96ZW5Ib3N0cyA9IGF3YWl0IChhd2FpdCBvYnNlcnZlci5yZXF1ZXN0LmdldChcIi92MS9ob3N0c1wiKSkuanNvbigpO1xuICAvLyBGcmVlemUgdGhlIG9ic2VydmVyJ3MgSFRUUCBzbmFwc2hvdHM6IGFkZGl0aW9ucyBtdXN0IGFycml2ZSB0aHJvdWdoIGZvbGxvdyxcbiAgLy8gYW5kIGEgc3Vic2VxdWVudCBzdGFsZSBwb2xsIG11c3Qgbm90IHVuZG8gdGhlIG5ld2VyIGpvdXJuYWwgcmV2aXNpb24uXG4gIGF3YWl0IG9ic2VydmVyLnJvdXRlKFwiKiovdjEvaG9zdHNcIiwgKHJvdXRlKSA9PiByb3V0ZS5mdWxmaWxsKHsganNvbjogZnJvemVuSG9zdHMgfSkpO1xuICBhd2FpdCBwYWdlLmdldEJ5VGVzdElkKFwid29ya3NwYWNlLWFkZFwiKS5jbGljaygpO1xuICBhd2FpdCBwYWdlLmdldEJ5VGVzdElkKFwid29ya3NwYWNlLXJlZ2lzdGVyLXBhdGhcIikuZmlsbChwYXRoISk7XG4gIGNvbnN0IHJlZ2lzdGVyZWQgPSBwYWdlLndhaXRGb3JSZXNwb25zZSgocmVzcG9uc2UpID0+IHJlc3BvbnNlLnJlcXVlc3QoKS5tZXRob2QoKSA9PT0gXCJQT1NUXCJcbiAgICAmJiBuZXcgVVJMKHJlc3BvbnNlLnVybCgpKS5wYXRobmFtZSA9PT0gYC92MS9ob3N0cy8ke2hvc3RJZH0vd29ya3NwYWNlc2ApO1xuICBhd2FpdCBwYWdlLmdldEJ5VGVzdElkKFwid29ya3NwYWNlLXJlZ2lzdGVyLXN1Ym1pdFwiKS5jbGljaygpO1xuICBjb25zdCByZWdpc3RyYXRpb24gPSBhd2FpdCByZWdpc3RlcmVkO1xuICBleHBlY3QocmVnaXN0cmF0aW9uLm9rKCkpLnRvQmUodHJ1ZSk7XG4gIGNvbnN0IHNuYXBzaG90ID0gYXdhaXQgcmVnaXN0cmF0aW9uLmpzb24oKTtcbiAgY29uc3Qgd29ya3NwYWNlID0gc25hcHNob3Qud29ya3NwYWNlcy5maW5kKChyb3c6IHsgd29ya3NwYWNlSWQ6IHN0cmluZyB9KSA9PiByb3cud29ya3NwYWNlSWQgPT09IHNuYXBzaG90LndvcmtzcGFjZUlkKTtcbiAgZXhwZWN0KHdvcmtzcGFjZSkudG9CZVRydXRoeSgpO1xuICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcIm5ldy1zZXNzaW9uLXdvcmtzcGFjZVwiKSkudG9IYXZlVmFsdWUod29ya3NwYWNlLndvcmtzcGFjZUlkKTtcbiAgY29uc3Qgb2JzZXJ2ZXJPcHRpb24gPSBvYnNlcnZlci5nZXRCeVRlc3RJZChcIm5ldy1zZXNzaW9uLXdvcmtzcGFjZVwiKS5sb2NhdG9yKGBvcHRpb25bdmFsdWU9XCIke3dvcmtzcGFjZS53b3Jrc3BhY2VJZH1cIl1gKTtcbiAgYXdhaXQgZXhwZWN0KG9ic2VydmVyT3B0aW9uKS50b0hhdmVDb3VudCgxKTtcbiAgYXdhaXQgb2JzZXJ2ZXIud2FpdEZvclJlcXVlc3QoKHJlcXVlc3QpID0+IG5ldyBVUkwocmVxdWVzdC51cmwoKSkucGF0aG5hbWUgPT09IFwiL3YxL2hvc3RzXCIpO1xuICBhd2FpdCBleHBlY3Qob2JzZXJ2ZXJPcHRpb24pLnRvSGF2ZUNvdW50KDEpO1xuICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcIm5ldy1zZXNzaW9uLWN3ZFwiKSkudG9IYXZlVmFsdWUoXCJcIik7XG4gIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1raW5kLXRlcm1pbmFsXCIpLmNsaWNrKCk7XG4gIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1zdGFydFwiKS5jbGljaygpO1xuICBhd2FpdCBleHBlY3QocGFnZSkudG9IYXZlVVJMKC9cXC9zXFwvLywgeyB0aW1lb3V0OiAzMF8wMDAgfSk7XG4gIGNvbnN0IHNlc3Npb25QYXRoID0gbmV3IFVSTChwYWdlLnVybCgpKS5wYXRobmFtZTtcbiAgY29uc3QgaW5zdGFuY2VJZCA9IHNlc3Npb25QYXRoLnNwbGl0KFwiL1wiKS5hdCgtMSkhO1xuICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcInNlc3Npb24tcGFnZVwiKSkudG9IYXZlQXR0cmlidXRlKFwiZGF0YS12aWV3XCIsIFwidHR5XCIpO1xuICBhd2FpdCBleHBlY3QocGFnZS5sb2NhdG9yKFwiW2RhdGEtdHR5LWxhYj0nMSddXCIpKS50b0hhdmVBdHRyaWJ1dGUoXCJkYXRhLXR0eS1zdGF0dXNcIiwgXCJsaXZlXCIsIHsgdGltZW91dDogMzBfMDAwIH0pO1xuICBjb25zdCB0ZXJtaW5hbElucHV0ID0gcGFnZS5nZXRCeVJvbGUoXCJ0ZXh0Ym94XCIsIHsgbmFtZTogXCJUZXJtaW5hbCBpbnB1dFwiLCBleGFjdDogdHJ1ZSB9KTtcbiAgYXdhaXQgdGVybWluYWxJbnB1dC5mb2N1cygpO1xuICBhd2FpdCBleHBlY3QodGVybWluYWxJbnB1dCkudG9CZUZvY3VzZWQoKTtcbiAgYXdhaXQgcGFnZS5rZXlib2FyZC50eXBlKFwicHJpbnRmICdXT1JLU1BBQ0VfUkVHX0NXRD0lc1xcXFxuJyBcXFwiJFBXRFxcXCJcIik7XG4gIGF3YWl0IHBhZ2Uua2V5Ym9hcmQucHJlc3MoXCJFbnRlclwiKTtcbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJ0dHktYW5zaS1wcmV2aWV3XCIpKS50b0NvbnRhaW5UZXh0KGBXT1JLU1BBQ0VfUkVHX0NXRD0ke3dvcmtzcGFjZS5yb290fWAsIHsgdGltZW91dDogMjBfMDAwIH0pO1xuICBhd2FpdCBleHBlY3QucG9sbCgoKSA9PiBmb2xsb3dVcmxzLmxlbmd0aCkudG9CZUdyZWF0ZXJUaGFuKDApO1xuICBleHBlY3QoZm9sbG93VXJscy5ldmVyeSgodXJsKSA9PiAhbmV3IFVSTCh1cmwpLnNlYXJjaFBhcmFtcy5oYXMoXCJ0b2tlblwiKSkpLnRvQmUodHJ1ZSk7XG4gIGF3YWl0IHBhZ2UucmVsb2FkKCk7XG4gIGF3YWl0IGV4cGVjdENvb2tpZVNlc3Npb24ocGFnZSk7XG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwic2Vzc2lvbi1wYWdlXCIpKS50b0JlVmlzaWJsZSgpO1xuICBhd2FpdCBwYWdlLmdldEJ5Um9sZShcImJ1dHRvblwiLCB7IG5hbWU6IFwiU3RvcFwiLCBleGFjdDogdHJ1ZSB9KS5jbGljaygpO1xuICBhd2FpdCBleHBlY3QucG9sbChhc3luYyAoKSA9PiB7XG4gICAgY29uc3QgcmVzcG9uc2UgPSBhd2FpdCBwYWdlLnJlcXVlc3QuZ2V0KGAvdjEvaW5zdGFuY2VzLyR7aW5zdGFuY2VJZH1gKTtcbiAgICByZXR1cm4gKGF3YWl0IHJlc3BvbnNlLmpzb24oKSkubGlmZWN5Y2xlO1xuICB9LCB7IHRpbWVvdXQ6IDMwXzAwMCB9KS50b0JlKFwiZXhpdGVkXCIpO1xuICBhd2FpdCBwYWdlLmdvdG8oXCIvaG9zdHNcIik7XG4gIGNvbnN0IHJvdyA9IHBhZ2UuZ2V0QnlUZXN0SWQoXCJob3N0LXdvcmtzcGFjZVwiKS5maWx0ZXIoeyBoYXNUZXh0OiB3b3Jrc3BhY2Uucm9vdCB9KTtcbiAgYXdhaXQgZXhwZWN0KHJvdykudG9CZVZpc2libGUoKTtcbiAgY29uc3QgdW5yZWdpc3RlcmVkID0gcGFnZS53YWl0Rm9yUmVzcG9uc2UoKHJlc3BvbnNlKSA9PiByZXNwb25zZS5yZXF1ZXN0KCkubWV0aG9kKCkgPT09IFwiREVMRVRFXCJcbiAgICAmJiBuZXcgVVJMKHJlc3BvbnNlLnVybCgpKS5wYXRobmFtZSA9PT0gYC92MS9ob3N0cy8ke2hvc3RJZH0vd29ya3NwYWNlc2ApO1xuICBhd2FpdCByb3cuZ2V0QnlSb2xlKFwiYnV0dG9uXCIsIHsgbmFtZTogYOenu+mZpOebruW9lSAke3dvcmtzcGFjZS5yb290fWAsIGV4YWN0OiB0cnVlIH0pLmNsaWNrKCk7XG4gIGV4cGVjdCgoYXdhaXQgdW5yZWdpc3RlcmVkKS5vaygpKS50b0JlKHRydWUpO1xuICBhd2FpdCBleHBlY3Qocm93KS50b0hhdmVDb3VudCgwKTtcbiAgYXdhaXQgZXhwZWN0KG9ic2VydmVyT3B0aW9uKS50b0hhdmVDb3VudCgwKTtcbiAgYXdhaXQgb2JzZXJ2ZXIuY2xvc2UoKTtcbiAgYXdhaXQgcGFnZS5nb3RvKFwiL3Nlc3Npb25zL25ld1wiKTtcbiAgYXdhaXQgaG9zdFBpY2tlci5zZWxlY3RPcHRpb24oaG9zdElkKTtcbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi13b3Jrc3BhY2VcIikubG9jYXRvcihgb3B0aW9uW3ZhbHVlPVwiJHt3b3Jrc3BhY2Uud29ya3NwYWNlSWR9XCJdYCkpLnRvSGF2ZUNvdW50KDApO1xufSk7XG5cbnRlc3QoXCJlZmZvcnQgc2xpZGVyIGRyYWcgYW5kIGtleWJvYXJkIHNlbmQgaW5zdGFuY2UuY29uZmlndXJlXCIsIGFzeW5jICh7IHBhZ2UgfSkgPT4ge1xuICB0ZXN0LnNraXAocHJvY2Vzcy5lbnYuSFVCX0UyRV9FWFRFUk5BTCA9PT0gXCIxXCIsIFwiRXh0ZXJuYWwgTm9kZSBpcyBjb3ZlcmVkIGJ5IHRoZSByZWFsIHNoZWxsIHdvcmtzcGFjZSBmbG93XCIpO1xuICBhd2FpdCBsb2dpbihwYWdlKTtcbiAgYXdhaXQgcGFnZS5nZXRCeVRpdGxlKFwi5paw5bu6XCIsIHsgZXhhY3Q6IHRydWUgfSkuY2xpY2soKTtcbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1zaGVldFwiKSkudG9CZVZpc2libGUoKTtcbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1ob3N0XCIpKS50b0NvbnRhaW5UZXh0KFwiZTJlLWZha2Utbm9kZVwiLCB7IHRpbWVvdXQ6IDIwXzAwMCB9KTtcbiAgY29uc3QgaG9zdCA9IGF3YWl0IHBhZ2VcbiAgICAuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1ob3N0XCIpXG4gICAgLmxvY2F0b3IoXCJvcHRpb25cIilcbiAgICAuZmlsdGVyKHsgaGFzVGV4dDogXCJlMmUtZmFrZS1ub2RlXCIgfSlcbiAgICAuZ2V0QXR0cmlidXRlKFwidmFsdWVcIik7XG4gIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1ob3N0XCIpLnNlbGVjdE9wdGlvbihob3N0ISk7XG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwibmV3LXNlc3Npb24td29ya3NwYWNlXCIpLmxvY2F0b3IoXCJvcHRpb25cIikpLm5vdC50b0hhdmVDb3VudCgwKTtcbiAgYXdhaXQgcGFnZS5nZXRCeVRlc3RJZChcIm5ldy1zZXNzaW9uLXByb21wdFwiKS5maWxsKFwiZWZmb3J0IHNsaWRlciBlMmVcIik7XG4gIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1zdGFydFwiKS5jbGljaygpO1xuICBhd2FpdCBleHBlY3QocGFnZSkudG9IYXZlVVJMKC9cXC9zXFwvLywgeyB0aW1lb3V0OiAyMF8wMDAgfSk7XG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwiY29tcG9zZXItYmFyXCIpKS50b0JlVmlzaWJsZSgpO1xuICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcIm1vZGVsLWVmZm9ydC1jaGlwXCIpKS50b0JlVmlzaWJsZSgpO1xuXG4gIGNvbnN0IGNvbmZpZ3VyZUJvZGllczogeyBvcGVyYXRpb24/OiBzdHJpbmc7IHBheWxvYWQ/OiB7IGVmZm9ydD86IHsgbmFtZT86IHN0cmluZzsgaW5kZXg/OiBudW1iZXIgfSB9IH1bXSA9IFtdO1xuICBwYWdlLm9uKFwicmVxdWVzdFwiLCAocmVxdWVzdCkgPT4ge1xuICAgIGlmIChyZXF1ZXN0Lm1ldGhvZCgpICE9PSBcIlBPU1RcIikgcmV0dXJuO1xuICAgIGlmICghbmV3IFVSTChyZXF1ZXN0LnVybCgpKS5wYXRobmFtZS5lbmRzV2l0aChcIi9jb21tYW5kc1wiKSkgcmV0dXJuO1xuICAgIGNvbnN0IGJvZHkgPSByZXF1ZXN0LnBvc3REYXRhSlNPTigpIGFzICh0eXBlb2YgY29uZmlndXJlQm9kaWVzKVtudW1iZXJdIHwgbnVsbDtcbiAgICBpZiAoYm9keSkgY29uZmlndXJlQm9kaWVzLnB1c2goYm9keSk7XG4gIH0pO1xuXG4gIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJtb2RlbC1lZmZvcnQtY2hpcFwiKS5jbGljaygpO1xuICBjb25zdCBzbGlkZXIgPSBwYWdlLmdldEJ5VGVzdElkKFwiZWZmb3J0LXNsaWRlclwiKTtcbiAgYXdhaXQgZXhwZWN0KHNsaWRlcikudG9CZVZpc2libGUoKTtcbiAgYXdhaXQgZXhwZWN0KHNsaWRlcikudG9IYXZlQXR0cmlidXRlKFwiZGF0YS10aWVyc1wiLCBcImxvdyxtZWRpdW0saGlnaCx4aGlnaCxtYXgsdWx0cmFjb2RlXCIpO1xuICBjb25zdCBib3ggPSBhd2FpdCBzbGlkZXIuYm91bmRpbmdCb3goKTtcbiAgZXhwZWN0KGJveCkudG9CZVRydXRoeSgpO1xuICBhd2FpdCBwYWdlLm1vdXNlLm1vdmUoYm94IS54ICsgYm94IS53aWR0aCAtIDMsIGJveCEueSArIGJveCEuaGVpZ2h0IC8gMik7XG4gIGF3YWl0IHBhZ2UubW91c2UuZG93bigpO1xuICBhd2FpdCBwYWdlLm1vdXNlLm1vdmUoYm94IS54ICsgYm94IS53aWR0aCAtIDMsIGJveCEueSArIGJveCEuaGVpZ2h0IC8gMiwgeyBzdGVwczogMyB9KTtcbiAgYXdhaXQgcGFnZS5tb3VzZS51cCgpO1xuICAvLyBUaGUgZmFyLXJpZ2h0IHN0b3AgaXMgdWx0cmFjb2RlOiB0aGUgeGhpZ2ggdGllciBwbHVzIHRoZSB3b3JrZmxvdyBmbGFnLlxuICAvLyBUaGUgY2hpcCBvbmx5IHJlLXJlbmRlcnMgb25jZSBpbnN0YW5jZS5jb25maWd1cmUgcm91bmQtdHJpcHMgdGhyb3VnaCB0aGVcbiAgLy8gSHViLCBzbyB0aGVzZSB3YWl0IG9uIHRoZSB3aXJlIGxpa2UgZXZlcnkgb3RoZXIgbGl2ZSBhc3NlcnRpb24gaGVyZS5cbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJjb21wb3NlclwiKSkudG9IYXZlQXR0cmlidXRlKFwiZGF0YS1lZmZvcnRcIiwgXCJ1bHRyYWNvZGVcIiwge1xuICAgIHRpbWVvdXQ6IDIwXzAwMCxcbiAgfSk7XG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwiY29tcG9zZXJcIikpLnRvSGF2ZUF0dHJpYnV0ZShcImRhdGEtdWx0cmFjb2RlXCIsIFwiMVwiLCB7XG4gICAgdGltZW91dDogMjBfMDAwLFxuICB9KTtcbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJtb2RlbC1lZmZvcnQtY2hpcFwiKSkudG9IYXZlQXR0cmlidXRlKFwiZGF0YS1lbWJlclwiLCBcIjFcIiwge1xuICAgIHRpbWVvdXQ6IDIwXzAwMCxcbiAgfSk7XG4gIGF3YWl0IGV4cGVjdFxuICAgIC5wb2xsKCgpID0+XG4gICAgICBjb25maWd1cmVCb2RpZXMuc29tZShcbiAgICAgICAgKGJvZHkpID0+IGJvZHkub3BlcmF0aW9uID09PSBcImluc3RhbmNlLmNvbmZpZ3VyZVwiICYmIGJvZHkucGF5bG9hZD8uZWZmb3J0Py5uYW1lID09PSBcInVsdHJhY29kZVwiLFxuICAgICAgKSxcbiAgICApXG4gICAgLnRvQmVUcnV0aHkoKTtcblxuICBhd2FpdCBzbGlkZXIuZm9jdXMoKTtcbiAgYXdhaXQgcGFnZS5rZXlib2FyZC5wcmVzcyhcIkhvbWVcIik7XG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwiY29tcG9zZXJcIikpLnRvSGF2ZUF0dHJpYnV0ZShcImRhdGEtZWZmb3J0XCIsIFwibG93XCIsIHtcbiAgICB0aW1lb3V0OiAyMF8wMDAsXG4gIH0pO1xuICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcIm1vZGVsLWVmZm9ydC1jaGlwXCIpKS50b0hhdmVBdHRyaWJ1dGUoXCJkYXRhLWVtYmVyXCIsIFwiMFwiLCB7XG4gICAgdGltZW91dDogMjBfMDAwLFxuICB9KTtcbiAgYXdhaXQgZXhwZWN0XG4gICAgLnBvbGwoKCkgPT5cbiAgICAgIGNvbmZpZ3VyZUJvZGllcy5zb21lKFxuICAgICAgICAoYm9keSkgPT4gYm9keS5vcGVyYXRpb24gPT09IFwiaW5zdGFuY2UuY29uZmlndXJlXCIgJiYgYm9keS5wYXlsb2FkPy5lZmZvcnQ/Lm5hbWUgPT09IFwibG93XCIsXG4gICAgICApLFxuICAgIClcbiAgICAudG9CZVRydXRoeSgpO1xufSk7XG5cbi8qKlxuICogRC0wMjggwqc1LjEvwqcxLjA6IE5ldyBTZXNzaW9uIGRlZmF1bHRzIHRvIHRoZSBuYXRpdmUgc2hlbGwtcHR5IGNhcnJpZXIgKHRoZVxuICogY2hvaWNlIGNvbWVzIGZyb20gdGhlIGZha2UgTm9kZSdzIGRyaXZlckludmVudG9yeSksIGFuZCB0aGUgcmVzdWx0aW5nXG4gKiBzZXNzaW9uIGNhcnJpZXMgQk9USCBwcm9qZWN0aW9ucyDigJQgdGVybWluYWwgYW5kIOe7k+aehC5cbiAqL1xudGVzdChcIm5hdGl2ZSBQVFkgZGVmYXVsdCBmcm9tIHRoZSBob3N0IG1hdHJpeCwgd2l0aCBib3RoIHByb2plY3Rpb25zXCIsIGFzeW5jICh7IHBhZ2UgfSkgPT4ge1xuICB0ZXN0LnNraXAocHJvY2Vzcy5lbnYuSFVCX0UyRV9FWFRFUk5BTCA9PT0gXCIxXCIsIFwiTmVlZHMgdGhlIGluLXByb2Nlc3MgZmFrZSBOb2RlJ3MgbWF0cml4XCIpO1xuICBhd2FpdCBsb2dpbihwYWdlKTtcbiAgYXdhaXQgcGFnZS5nb3RvKFwiL3Nlc3Npb25zL25ld1wiKTtcbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1ob3N0XCIpKS50b0NvbnRhaW5UZXh0KFwiZTJlLWZha2Utbm9kZVwiLCB7IHRpbWVvdXQ6IDIwXzAwMCB9KTtcblxuICAvLyBCYXRjaCBCIG1vdmVkIHRoZSBjYXJyaWVyIG1hdHJpeCBhbmQgbGF1bmNoIHByZXZpZXcgYmVoaW5kIOmrmOe6p+iuvue9riBzb1xuICAvLyB0aGUgZmlyc3QgbGF5ZXIgc3RheXMgaW4gdXNlciB2b2NhYnVsYXJ5OyB0aGUgbWF0cml4IGl0c2VsZiBpcyB1bmNoYW5nZWQuXG4gIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1hZHZhbmNlZFwiKS5jbGljaygpO1xuICBjb25zdCBzaGVsbCA9IHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1kcml2ZXItc2hlbGwtcHR5XCIpO1xuICBhd2FpdCBleHBlY3Qoc2hlbGwpLnRvQmVWaXNpYmxlKCk7XG4gIGF3YWl0IGV4cGVjdChzaGVsbCkudG9IYXZlQXR0cmlidXRlKFwiZGF0YS1kZWZhdWx0XCIsIFwiMVwiKTtcbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1sYXVuY2gtcHJldmlld1wiKSkudG9Db250YWluVGV4dChcImNsYXVkZVwiKTtcbiAgLy8gTGVnYWN5IGNhcnJpZXJzIHJlbWFpbiBzZWxlY3RhYmxlIGJ1dCBhcmUgc2Vjb25kYXJ5LlxuICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcIm5ldy1zZXNzaW9uLWRyaXZlci1jbGF1ZGUtcHJpbnRcIikpLnRvSGF2ZUF0dHJpYnV0ZShcImRhdGEtZGVmYXVsdFwiLCBcIjBcIik7XG4gIGF3YWl0IHNob3QocGFnZSwgXCJuYXRpdmUtcHR5LXdlYi0xLW5ldy1zZXNzaW9uLTE0NDAucG5nXCIpO1xuICBhd2FpdCBwYWdlLnNldFZpZXdwb3J0U2l6ZSh7IHdpZHRoOiA0MDAsIGhlaWdodDogODQwIH0pO1xuICBhd2FpdCBzaG90KHBhZ2UsIFwibmF0aXZlLXB0eS13ZWItMS1uZXctc2Vzc2lvbi00MDAucG5nXCIpO1xuICBhd2FpdCBwYWdlLnNldFZpZXdwb3J0U2l6ZSh7IHdpZHRoOiAxNDQwLCBoZWlnaHQ6IDkwMCB9KTtcblxuICBhd2FpdCBwYWdlLmdldEJ5VGVzdElkKFwibmV3LXNlc3Npb24tcHJvbXB0XCIpLmZpbGwoXCJuYXRpdmUgcHR5IHNlc3Npb25cIik7XG4gIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1zdGFydFwiKS5jbGljaygpO1xuICBhd2FpdCBleHBlY3QocGFnZSkudG9IYXZlVVJMKC9cXC9zXFwvLywgeyB0aW1lb3V0OiAyMF8wMDAgfSk7XG5cbiAgLy8gQm90aCBwcm9qZWN0aW9ucyBhdmFpbGFibGU6IHRoZSDnu4jnq68v57uT5p6EIHN3aXRjaCBpcyByZW5kZXJlZCwgYW5kIHRoZVxuICAvLyBzdHJ1Y3R1cmVkIGNvbnZlcnNhdGlvbiBvcGVucyBieSBkZWZhdWx0LlxuICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcInZpZXctc3dpdGNoXCIpKS50b0JlVmlzaWJsZSgpO1xuICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcInZpZXctc3dpdGNoLXR0eVwiKSkudG9CZVZpc2libGUoKTtcbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJ2aWV3LXN3aXRjaC1zdHJ1Y3R1cmVkXCIpKS50b0JlVmlzaWJsZSgpO1xuICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcInNlc3Npb24tcGFnZVwiKSkudG9IYXZlQXR0cmlidXRlKFwiZGF0YS12aWV3XCIsIFwic3RydWN0dXJlZFwiKTtcbn0pO1xuXG4vKipcbiAqIGMtc3RlZXIgY29tcG9zZXIgb24gYSB3b3JraW5nIG5hdGl2ZSBzZXNzaW9uOiBFbnRlciBRVUVVRVMgKFJlbXVkYS1oZWxkLFxuICogbm8gUE9TVCksIOKMmC9DdHJsK0VudGVyIG9yIHRoZSDmj5LpmJ8gYnV0dG9uIGludGVycnVwdHMgYW5kIGp1bXBzIHdpdGhcbiAqIG1vZGU9c3RlZXIsIEVzYyBwbGFpbi1pbnRlcnJ1cHRzLCBhbmQgaGVsZCByb3dzIGZsdXNoIHdoZW4gdGhlIHR1cm4gZW5kcy5cbiAqL1xudGVzdChcImNvbXBvc2VyIHF1ZXVlIC8gc3RlZXIgLyBpbnRlcnJ1cHQgc3RhdGVzIG9uIGEgd29ya2luZyBuYXRpdmUgc2Vzc2lvblwiLCBhc3luYyAoeyBwYWdlIH0pID0+IHtcbiAgdGVzdC5za2lwKHByb2Nlc3MuZW52LkhVQl9FMkVfRVhURVJOQUwgPT09IFwiMVwiLCBcIk5lZWRzIHRoZSBpbi1wcm9jZXNzIGZha2UgTm9kZVwiKTtcbiAgY29uc3QgY29tbWFuZHM6IHsgb3BlcmF0aW9uPzogc3RyaW5nOyBwYXlsb2FkPzogeyBtb2RlPzogc3RyaW5nOyBwcm9tcHQ/OiBzdHJpbmcgfSB9W10gPSBbXTtcbiAgcGFnZS5vbihcInJlcXVlc3RcIiwgKHJlcXVlc3QpID0+IHtcbiAgICBpZiAocmVxdWVzdC5tZXRob2QoKSAhPT0gXCJQT1NUXCIpIHJldHVybjtcbiAgICBpZiAoIW5ldyBVUkwocmVxdWVzdC51cmwoKSkucGF0aG5hbWUuZW5kc1dpdGgoXCIvY29tbWFuZHNcIikpIHJldHVybjtcbiAgICBjb25zdCBib2R5ID0gcmVxdWVzdC5wb3N0RGF0YUpTT04oKSBhcyAodHlwZW9mIGNvbW1hbmRzKVtudW1iZXJdIHwgbnVsbDtcbiAgICBpZiAoYm9keSkgY29tbWFuZHMucHVzaChib2R5KTtcbiAgfSk7XG5cbiAgYXdhaXQgbG9naW4ocGFnZSk7XG4gIGF3YWl0IHBhZ2UuZ290byhcIi9zZXNzaW9ucy9uZXdcIik7XG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwibmV3LXNlc3Npb24taG9zdFwiKSkudG9Db250YWluVGV4dChcImUyZS1mYWtlLW5vZGVcIiwgeyB0aW1lb3V0OiAyMF8wMDAgfSk7XG4gIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1wcm9tcHRcIikuZmlsbChcIndvcmtpbmcgc2Vzc2lvblwiKTtcbiAgYXdhaXQgcGFnZS5nZXRCeVRlc3RJZChcIm5ldy1zZXNzaW9uLXN0YXJ0XCIpLmNsaWNrKCk7XG4gIGF3YWl0IGV4cGVjdChwYWdlKS50b0hhdmVVUkwoL1xcL3NcXC8vLCB7IHRpbWVvdXQ6IDIwXzAwMCB9KTtcbiAgY29uc3QgaW5zdGFuY2VJZCA9IG5ldyBVUkwocGFnZS51cmwoKSkucGF0aG5hbWUuc3BsaXQoXCIvXCIpLnBvcCgpIGFzIHN0cmluZztcbiAgYXdhaXQgYW5zd2VyUGVuZGluZ0FwcHJvdmFscyhwYWdlLCBpbnN0YW5jZUlkKTtcblxuICAvLyBBbnN3ZXJpbmcgdGhlIGNyZWF0ZSBhcHByb3ZhbCBjb21wbGV0ZXMgdGhlIGZha2UgdHVybjogdGhlIGNvbXBvc2VyIGlzXG4gIC8vIGlkbGUuIFN0YXJ0IGEgZnJlc2ggXCJob2xkLXdvcmtpbmdcIiB0dXJuIHRvIGV4ZXJjaXNlIHRoZSB3b3JraW5nIGNvbnRyb2xzLlxuICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcInNlc3Npb24tcGFnZVwiKSkudG9IYXZlQXR0cmlidXRlKFwiZGF0YS1zdGF0dXNcIiwgXCJpZGxlXCIsIHtcbiAgICB0aW1lb3V0OiAxMF8wMDAsXG4gIH0pO1xuICBhd2FpdCBwYWdlLmdldEJ5VGVzdElkKFwiY29tcG9zZXItaW5wdXRcIikuZmlsbChcImhvbGQtd29ya2luZyBsb25nIHRvb2xcIik7XG4gIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJjb21wb3Nlci1zZW5kXCIpLmNsaWNrKCk7XG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwic2Vzc2lvbi1wYWdlXCIpKS50b0hhdmVBdHRyaWJ1dGUoXCJkYXRhLXN0YXR1c1wiLCBcIndvcmtpbmdcIiwge1xuICAgIHRpbWVvdXQ6IDEwXzAwMCxcbiAgfSk7XG4gIC8vIFdoaWxlIGBzZW5kaW5nYCB0aGUgZG9jayBpcyBkaXNhYmxlZDsgdHlwZSB0aGUgaGVsZCB0ZXh0IGZpcnN0IHNvIHRoZVxuICAvLyBxdWV1ZSBidXR0b24ncyBlbmFibGVtZW50IG1hcmtzIHRoZSBzZXR0bGVkIFBPU1QgZmxpZ2h0IChpdCBpcyBhbHNvXG4gIC8vIGRpc2FibGVkIG9uIGVtcHR5IHRleHQsIHNvIGl0IGNhbiBvbmx5IGJlIGVuYWJsZWQgd2l0aCB0ZXh0IHByZXNlbnQpLlxuICBhd2FpdCBwYWdlLmdldEJ5VGVzdElkKFwiY29tcG9zZXItaW5wdXRcIikuZmlsbChcImFmdGVyIHRoaXMgdHVyblwiKTtcbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJjb21wb3Nlci1xdWV1ZVwiKSkudG9CZUVuYWJsZWQoeyB0aW1lb3V0OiAxMF8wMDAgfSk7XG5cbiAgLy8gV29ya2luZyBjb21wb3NlcjogRW50ZXIgcXVldWVzIChSZW11ZGEtaGVsZCksIOaPkumYnyBhbmQg5omT5patIGF2YWlsYWJsZS5cbiAgY29uc3QgcXVldWUgPSBwYWdlLmdldEJ5VGVzdElkKFwiY29tcG9zZXItcXVldWVcIik7XG4gIGF3YWl0IGV4cGVjdChxdWV1ZSkudG9IYXZlQXR0cmlidXRlKFwiZGF0YS1tb2RlXCIsIFwicXVldWVcIik7XG4gIGF3YWl0IGV4cGVjdChxdWV1ZSkudG9IYXZlQXR0cmlidXRlKFwiZGF0YS1ob2xkZXJcIiwgXCJyZW11ZGFcIik7XG4gIGNvbnN0IHN0ZWVyID0gcGFnZS5nZXRCeVRlc3RJZChcImNvbXBvc2VyLXN0ZWVyXCIpO1xuICBhd2FpdCBleHBlY3Qoc3RlZXIpLnRvQmVWaXNpYmxlKCk7XG4gIGF3YWl0IGV4cGVjdChzdGVlcikudG9IYXZlQXR0cmlidXRlKFwiZGF0YS1wcm92aXNpb25cIiwgXCJuYXRpdmVcIik7XG4gIGNvbnN0IGludGVycnVwdCA9IHBhZ2UuZ2V0QnlUZXN0SWQoXCJjb21wb3Nlci1pbnRlcnJ1cHRcIik7XG4gIGF3YWl0IGV4cGVjdChpbnRlcnJ1cHQpLnRvQmVWaXNpYmxlKCk7XG4gIGF3YWl0IGV4cGVjdChpbnRlcnJ1cHQpLnRvSGF2ZUF0dHJpYnV0ZShcImRhdGEtcHJvdmlzaW9uXCIsIFwibmF0aXZlXCIpO1xuICBhd2FpdCBzaG90KHBhZ2UsIFwibmF0aXZlLXB0eS13ZWItMS1jb21wb3Nlci13b3JraW5nLTE0NDAucG5nXCIpO1xuXG4gIC8vIEVudGVyIGhvbGRzIGEgcmVtb3ZhYmxlIGNoaXAgYW5kIHNlbmRzIG5vdGhpbmcuXG4gIGNvbnN0IGJlZm9yZSA9IGNvbW1hbmRzLmxlbmd0aDtcbiAgYXdhaXQgcGFnZS5nZXRCeVRlc3RJZChcImNvbXBvc2VyLWlucHV0XCIpLnByZXNzKFwiRW50ZXJcIik7XG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwiY29tcG9zZXItcXVldWVkLWNoaXBcIikpLnRvQmVWaXNpYmxlKCk7XG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwiY29tcG9zZXItcXVldWVkLWNoaXBcIikpLnRvSGF2ZUF0dHJpYnV0ZShcImRhdGEtb3JkaW5hbFwiLCBcIjFcIik7XG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwiY29tcG9zZXItcXVldWUtc3RhdHVzXCIpKS50b0NvbnRhaW5UZXh0KFwiMVwiKTtcbiAgZXhwZWN0KGNvbW1hbmRzLmxlbmd0aCkudG9CZShiZWZvcmUpO1xuICBhd2FpdCBwYWdlLmdldEJ5VGVzdElkKFwiY29tcG9zZXItcXVldWVkLXJlbW92ZVwiKS5jbGljaygpO1xuICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcImNvbXBvc2VyLXF1ZXVlZC1jaGlwXCIpKS50b0hhdmVDb3VudCgwKTtcblxuICAvLyBSZS1xdWV1ZSwgdGhlbiDmj5LpmJ86IHRoZSBzdGVlciBQT1NUcyBtb2RlPXN0ZWVyIGZpcnN0OyB0aGUgaGVsZCByb3cgZmx1c2hlc1xuICAvLyB3aXRoIGEgcGxhaW4gbmV3LXR1cm4gUE9TVCBvbmx5IGFmdGVyIHRoZSB0dXJuLWVuZCAoaWRsZSkgbGFuZHMuXG4gIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJjb21wb3Nlci1pbnB1dFwiKS5maWxsKFwibGF0ZXJcIik7XG4gIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJjb21wb3Nlci1pbnB1dFwiKS5wcmVzcyhcIkVudGVyXCIpO1xuICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcImNvbXBvc2VyLXF1ZXVlZC1jaGlwXCIpKS50b0JlVmlzaWJsZSgpO1xuICBhd2FpdCBwYWdlLnNldFZpZXdwb3J0U2l6ZSh7IHdpZHRoOiA0MDAsIGhlaWdodDogODQwIH0pO1xuICBhd2FpdCBzaG90KHBhZ2UsIFwibmF0aXZlLXB0eS13ZWItMS1jb21wb3Nlci13b3JraW5nLTQwMC5wbmdcIik7XG4gIGF3YWl0IHBhZ2Uuc2V0Vmlld3BvcnRTaXplKHsgd2lkdGg6IDE0NDAsIGhlaWdodDogOTAwIH0pO1xuICBhd2FpdCBwYWdlLmdldEJ5VGVzdElkKFwiY29tcG9zZXItaW5wdXRcIikuZmlsbChcImp1bXAgbm93XCIpO1xuICBhd2FpdCBzdGVlci5jbGljaygpO1xuICAvLyBELTA0MjogdGhlIHN0ZWVyIGdlc3R1cmUgY29uZmlybXMgdGhyb3VnaCB0aGUgaW4tYXBwIFNoZWV0LCBub3RcbiAgLy8gd2luZG93LmNvbmZpcm0uXG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwiY29tcG9zZXItY29uZmlybS10aXRsZVwiKSkudG9IYXZlVGV4dChcIuaPkumYn+WPkemAgVwiKTtcbiAgYXdhaXQgcGFnZS5nZXRCeVRlc3RJZChcImNvbXBvc2VyLWNvbmZpcm0tb2tcIikuY2xpY2soKTtcbiAgYXdhaXQgZXhwZWN0XG4gICAgLnBvbGwoKCkgPT4gY29tbWFuZHMuc29tZSgoYykgPT4gYy5vcGVyYXRpb24gPT09IFwiaW5zdGFuY2Uuc2VuZFwiICYmIGMucGF5bG9hZD8ubW9kZSA9PT0gXCJzdGVlclwiKSlcbiAgICAudG9CZVRydXRoeSgpO1xuICAvLyBUaGUgc3RlZXIgaW50ZXJydXB0ZWQgdGhlIHR1cm4gKGJhZGdlKSBhbmQgdGhlIGZha2UgTm9kZSByZXBvcnRlZCBpZGxlLFxuICAvLyB3aGljaCBmbHVzaGVzIHRoZSBoZWxkIHJvdyBhcyBhbiBvcmRpbmFyeSBuZXcgdHVybi5cbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJjb21wb3Nlci1pbnRlcnJ1cHRlZC1jaGlwXCIpKS50b0JlVmlzaWJsZSgpO1xuICBhd2FpdCBleHBlY3RcbiAgICAucG9sbCgoKSA9PiBjb21tYW5kcy5maWx0ZXIoKGMpID0+IGMub3BlcmF0aW9uID09PSBcImluc3RhbmNlLnNlbmRcIikubGVuZ3RoKVxuICAgIC50b0JlR3JlYXRlclRoYW5PckVxdWFsKDMpO1xuICBjb25zdCBzZW5kcyA9IGNvbW1hbmRzLmZpbHRlcigoYykgPT4gYy5vcGVyYXRpb24gPT09IFwiaW5zdGFuY2Uuc2VuZFwiKTtcbiAgZXhwZWN0KHNlbmRzWzBdPy5wYXlsb2FkPy5wcm9tcHQpLnRvQmUoXCJob2xkLXdvcmtpbmcgbG9uZyB0b29sXCIpO1xuICBleHBlY3Qoc2VuZHNbMV0/LnBheWxvYWQpLnRvTWF0Y2hPYmplY3QoeyBwcm9tcHQ6IFwianVtcCBub3dcIiwgbW9kZTogXCJzdGVlclwiIH0pO1xuICBleHBlY3Qoc2VuZHMuYXQoLTEpPy5wYXlsb2FkPy5wcm9tcHQpLnRvQmUoXCJsYXRlclwiKTtcblxuICAvLyBQbGFpbiBFc2MgaW50ZXJydXB0czogc3RhcnQgYW5vdGhlciBob2xkLXdvcmtpbmcgdHVybiwgdGhlbiBjYW5jZWwgaXQuXG4gIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJjb21wb3Nlci1pbnB1dFwiKS5maWxsKFwiaG9sZC13b3JraW5nIHN0b3AgbWVcIik7XG4gIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJjb21wb3Nlci1zZW5kXCIpLmNsaWNrKCk7XG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwic2Vzc2lvbi1wYWdlXCIpKS50b0hhdmVBdHRyaWJ1dGUoXCJkYXRhLXN0YXR1c1wiLCBcIndvcmtpbmdcIiwge1xuICAgIHRpbWVvdXQ6IDEwXzAwMCxcbiAgfSk7XG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwiY29tcG9zZXItaW50ZXJydXB0XCIpKS50b0JlRW5hYmxlZCh7IHRpbWVvdXQ6IDEwXzAwMCB9KTtcbiAgYXdhaXQgcGFnZS5nZXRCeVRlc3RJZChcImNvbXBvc2VyLWlucHV0XCIpLmZvY3VzKCk7XG4gIGF3YWl0IHBhZ2Uua2V5Ym9hcmQucHJlc3MoXCJFc2NhcGVcIik7XG4gIC8vIEQtMDQyOiBFc2Mgb3BlbnMgdGhlIGluLWFwcCBTaGVldCBjb25maXJtOyBFc2MgaW5zaWRlIGl0IGNhbmNlbHMsIHNvXG4gIC8vIHByZXNzIHRoZSBkZXN0cnVjdGl2ZSBhY3Rpb24gZXhwbGljaXRseS5cbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJjb21wb3Nlci1jb25maXJtLXRpdGxlXCIpKS50b0hhdmVUZXh0KFwi5omT5pat5b2T5YmNIHR1cm5cIik7XG4gIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJjb21wb3Nlci1jb25maXJtLW9rXCIpLmNsaWNrKCk7XG4gIGF3YWl0IGV4cGVjdFxuICAgIC5wb2xsKCgpID0+IGNvbW1hbmRzLnNvbWUoKGMpID0+IGMub3BlcmF0aW9uID09PSBcImluc3RhbmNlLmNhbmNlbFwiKSlcbiAgICAudG9CZVRydXRoeSgpO1xuICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcInNlc3Npb24tcGFnZVwiKSkudG9IYXZlQXR0cmlidXRlKFwiZGF0YS1zdGF0dXNcIiwgXCJpZGxlXCIsIHtcbiAgICB0aW1lb3V0OiAxMF8wMDAsXG4gIH0pO1xuICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcImNvbXBvc2VyLWludGVycnVwdFwiKSkudG9IYXZlQ291bnQoMCk7XG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwiY29tcG9zZXItc3RlZXJcIikpLnRvSGF2ZUNvdW50KDApO1xuICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcImNvbXBvc2VyLXNlbmRcIikpLnRvSGF2ZUF0dHJpYnV0ZShcImRhdGEtbW9kZVwiLCBcIm5ldy10dXJuXCIpO1xufSk7XG5cbnRlc3QoXCJhIGhvb2stY2FycmllZCBhcHByb3ZhbCBzaG93cyB0aGUgcmVhbCB0b29sIGlucHV0IGFuZCBhbiBhbHdheXMtYWxsb3cgb3B0aW9uXCIsIGFzeW5jICh7XG4gIHBhZ2UsXG59KSA9PiB7XG4gIC8vIEQtMDI4IMKnNC40IHRpZXIgQSBlbmQgdG8gZW5kIHRocm91Z2ggdGhlIEh1YjogdGhlIGNhcmQgdGhlIE5vZGUgYnVpbGRzXG4gIC8vIGZyb20gYSByZWFsIFBlcm1pc3Npb25SZXF1ZXN0IGNhcnJpZXMgdGhlIGhhcm5lc3MtaG9vayBjYXJyaWVyLCB0aGUgdG9vbCdzXG4gIC8vIGFjdHVhbCBpbnB1dCByYXRoZXIgdGhhbiBhIHNjcmVlbiBzY3JhcGUsIGFuZCBhbiBhbHdheXMtYWxsb3cgYnV0dG9uIHRoYXRcbiAgLy8gZXhpc3RzIG9ubHkgYmVjYXVzZSB0aGUgaGFybmVzcyBvZmZlcmVkIGEgcGVybWlzc2lvbl9zdWdnZXN0aW9uLlxuICB0ZXN0LnNraXAoXG4gICAgcHJvY2Vzcy5lbnYuSFVCX0UyRV9FWFRFUk5BTCA9PT0gXCIxXCIsXG4gICAgXCJFeHRlcm5hbCBOb2RlIGlzIGNvdmVyZWQgYnkgdGhlIHJlYWwgc2hlbGwgd29ya3NwYWNlIGZsb3dcIixcbiAgKTtcbiAgYXdhaXQgbG9naW4ocGFnZSk7XG5cbiAgYXdhaXQgcGFnZS5nZXRCeVRpdGxlKFwi5paw5bu6XCIsIHsgZXhhY3Q6IHRydWUgfSkuY2xpY2soKTtcbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1zaGVldFwiKSkudG9CZVZpc2libGUoKTtcbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1ob3N0XCIpKS50b0NvbnRhaW5UZXh0KFwiZTJlLWZha2Utbm9kZVwiLCB7XG4gICAgdGltZW91dDogMjBfMDAwLFxuICB9KTtcbiAgY29uc3QgaG9zdCA9IGF3YWl0IHBhZ2VcbiAgICAuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1ob3N0XCIpXG4gICAgLmxvY2F0b3IoXCJvcHRpb25cIilcbiAgICAuZmlsdGVyKHsgaGFzVGV4dDogXCJlMmUtZmFrZS1ub2RlXCIgfSlcbiAgICAuZ2V0QXR0cmlidXRlKFwidmFsdWVcIik7XG4gIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1ob3N0XCIpLnNlbGVjdE9wdGlvbihob3N0ISk7XG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwibmV3LXNlc3Npb24td29ya3NwYWNlXCIpLmxvY2F0b3IoXCJvcHRpb25cIikpLm5vdC50b0hhdmVDb3VudCgwKTtcbiAgLy8gVGhlIGZha2UgTm9kZSByYWlzZXMgdGhlIHRpZXIgQSBjYXJkIGZvciBhIHByb21wdCBuYW1pbmcgdGhlIGhvb2sgcGF0aC5cbiAgYXdhaXQgcGFnZS5nZXRCeVRlc3RJZChcIm5ldy1zZXNzaW9uLXByb21wdFwiKS5maWxsKFwiaG9vay1hcHByb3ZhbCBwbGVhc2VcIik7XG4gIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1zdGFydFwiKS5jbGljaygpO1xuICBhd2FpdCBleHBlY3QocGFnZSkudG9IYXZlVVJMKC9cXC9zXFwvLywgeyB0aW1lb3V0OiAyMF8wMDAgfSk7XG4gIGNvbnN0IGluc3RhbmNlSWQgPSBuZXcgVVJMKHBhZ2UudXJsKCkpLnBhdGhuYW1lLnNwbGl0KFwiL1wiKS5wb3AoKSE7XG5cbiAgYXdhaXQgcGFnZS5nb3RvKFwiL2FwcHJvdmFsc1wiKTtcbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJhcHByb3ZhbHMtcGFnZVwiKSkudG9CZVZpc2libGUoKTtcbiAgY29uc3Qgcm93ID0gcGFnZS5nZXRCeVRlc3RJZChcImFwcHJvdmFsLXJvd1wiKS5maWx0ZXIoeyBoYXNUZXh0OiBcIi90bXAvaG9vay1hcHByb3ZhbC50eHRcIiB9KTtcbiAgLy8gVGhlIHJlYWwgdG9vbCBpbnB1dCBpcyB3aGF0IHRoZSBodW1hbiByZWFkcyBiZWZvcmUgZGVjaWRpbmcgKMKnMi4yKS5cbiAgYXdhaXQgZXhwZWN0KHJvdykudG9CZVZpc2libGUoeyB0aW1lb3V0OiAyMF8wMDAgfSk7XG4gIGF3YWl0IGV4cGVjdChyb3cpLnRvQ29udGFpblRleHQoXCJXcml0ZVwiKTtcbiAgLy8gQWxsIHRocmVlIG9wdGlvbnMgdGhlIGNhcmQgY2FycmllZCwgaW5jbHVkaW5nIHRoZSBzdWdnZXN0ZWQgZ3JhbnQuXG4gIGF3YWl0IGV4cGVjdChyb3cuZ2V0QnlSb2xlKFwiYnV0dG9uXCIsIHsgbmFtZTogXCLlhYHorrjkuIDmrKFcIiB9KSkudG9CZVZpc2libGUoKTtcbiAgYXdhaXQgZXhwZWN0KHJvdy5nZXRCeVJvbGUoXCJidXR0b25cIiwgeyBuYW1lOiBcIuaLkue7nVwiIH0pKS50b0JlVmlzaWJsZSgpO1xuICBjb25zdCBhbHdheXMgPSByb3cuZ2V0QnlSb2xlKFwiYnV0dG9uXCIsIHsgbmFtZTogXCLlp4vnu4jlhYHorrggKGFjY2VwdEVkaXRzKVwiIH0pO1xuICBhd2FpdCBleHBlY3QoYWx3YXlzKS50b0JlVmlzaWJsZSgpO1xuXG4gIGF3YWl0IGFsd2F5cy5jbGljaygpO1xuICBhd2FpdCBleHBlY3QoXG4gICAgcGFnZS5nZXRCeVRlc3RJZChcImFwcHJvdmFsLXJvd1wiKS5maWx0ZXIoeyBoYXNUZXh0OiBcIi90bXAvaG9vay1hcHByb3ZhbC50eHRcIiB9KSxcbiAgKS50b0hhdmVDb3VudCgwLCB7IHRpbWVvdXQ6IDIwXzAwMCB9KTtcblxuICAvLyBUaGUgY2FyZCBXQVMgdGhpcyBpbnN0YW5jZSdzIGJsb2NraW5nIGxhdW5jaCBhcHByb3ZhbDsgdGhlIGNsaWNrIGFib3ZlIGhhc1xuICAvLyBhbHJlYWR5IGFuc3dlcmVkIGl0LiBEbyBOT1QgY2FsbCBhbnN3ZXJQZW5kaW5nQXBwcm92YWxzIGhlcmU6IG9uY2UgdGhlXG4gIC8vIGZha2Ugbm9kZSdzIGNhcmQgaXMgYW5zd2VyZWQgaXQgbGVhdmVzIGludGVyYWN0aW9uLmxpc3QgKGl0IHNlcnZlcyBvbmx5XG4gIC8vIGl0cyBwZW5kaW5nIG1hcCksIHNvIHRoZSBoZWxwZXIncyBcImFuIGludGVyYWN0aW9uIGV4aXN0c1wiIHdhaXQgd291bGQgdGltZVxuICAvLyBvdXQgb24gdGhlIGFscmVhZHktcmVzb2x2ZWQgY2FyZC4gVGhlIGVuYWJsZWQgY29tcG9zZXIgYmVsb3cgaXMgdGhlXG4gIC8vIHJlbGVhc2Ugc2lnbmFsLlxuICBhd2FpdCBwYWdlLmdvdG8oYC9zLyR7aW5zdGFuY2VJZH1gKTtcbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJzZXNzaW9uLXBhZ2VcIikpLnRvQmVWaXNpYmxlKCk7XG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwiY29tcG9zZXItaW5wdXRcIikpLnRvQmVFbmFibGVkKHsgdGltZW91dDogMjBfMDAwIH0pO1xuXG4gIC8vIFJlbGVhc2UgdGhlIHBsYWNlbWVudCBzbG90LiBUaGUgZmFrZSBob3N0IGFkdmVydGlzZXMgbWF4SW5zdGFuY2VzIDggYW5kXG4gIC8vIHRoZSBzdWl0ZSBpcyBzZXJpYWwsIHNvIGEgc2Vzc2lvbiBsZWZ0IGxpdmUgaGVyZSBtYWtlcyBhIGxhdGVyIHNwZWMgZmFpbFxuICAvLyBwbGFjZW1lbnQgKFBMQUNFTUVOVF9VTlNBVElTRklBQkxFKSBmYXIgZnJvbSB0aGUgc3BlYyB0aGF0IGxlYWtlZCBpdC4gVGhlXG4gIC8vIGZha2UgTm9kZSBuZXZlciBleGl0cywgc28gb25seSBhIGZvcmNlZCBERUxFVEUgc2V0dGxlcyB0aGUgcm93LlxuICBjb25zdCBkZWxldGVkID0gYXdhaXQgcGFnZS5yZXF1ZXN0LmRlbGV0ZShgL3YxL2luc3RhbmNlcy8ke2luc3RhbmNlSWR9P2ZvcmNlPTFgKTtcbiAgZXhwZWN0KGRlbGV0ZWQub2soKSkudG9CZSh0cnVlKTtcbiAgYXdhaXQgZXhwZWN0XG4gICAgLnBvbGwoYXN5bmMgKCkgPT4gKGF3YWl0IHBhZ2UucmVxdWVzdC5nZXQoYC92MS9pbnN0YW5jZXMvJHtpbnN0YW5jZUlkfWApKS5zdGF0dXMoKSlcbiAgICAudG9CZSg0MDQpO1xufSk7XG5cbi8qKlxuICogRC0wMjggwqc3IGluIHRoZSBicm93c2VyOiBhc3Npc3RhbnQgdGV4dCBtdXN0IGdyb3cgaW4gcGxhY2UgYXMgZGVsdGFzIGxhbmQsXG4gKiBhbmQgcmVjb3JkcyB0aGUgaHVtYW4gZGlkIG5vdCB3cml0ZSBtdXN0IG5vdCByZW5kZXIgYXMgdGhlaXIgYnViYmxlLlxuICpcbiAqIMKr546w5ZyoIFN0cnVjdHVyYWwg55qE55WM6Z2i5LiN5piv5oyJ5paH5pys5rWB5byP5Ye6546w55qE4oCm5oiR6KeJ5b6X5a6D55qE5a6e5pe25oCn5LiN5aSfwrsgYW5kXG4gKiDCq+e7k+aehOWMlueVjOmdouS8muaKiui/veWKoOeahCBwcm9tcHQg5L+h5oGv5Lmf6aKd5aSW5bGV56S65LqGIOKApiDlrrnmmJPorqnkurror6/op6PmmK/miJHlj5Hkuobov5nkupvkv6Hmga/Cuy5cbiAqL1xudGVzdChcInN0cnVjdHVyZWQgdmlldyBzdHJlYW1zIGFzc2lzdGFudCB0ZXh0IGFuZCBzZXBhcmF0ZXMgaW5qZWN0ZWQgcmVjb3Jkc1wiLCBhc3luYyAoeyBwYWdlIH0pID0+IHtcbiAgdGVzdC5za2lwKHByb2Nlc3MuZW52LkhVQl9FMkVfRVhURVJOQUwgPT09IFwiMVwiLCBcIk5lZWRzIHRoZSBpbi1wcm9jZXNzIGZha2UgTm9kZVwiKTtcbiAgYXdhaXQgbG9naW4ocGFnZSk7XG4gIGF3YWl0IHBhZ2UuZ290byhcIi9zZXNzaW9ucy9uZXdcIik7XG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwibmV3LXNlc3Npb24taG9zdFwiKSkudG9Db250YWluVGV4dChcImUyZS1mYWtlLW5vZGVcIiwgeyB0aW1lb3V0OiAyMF8wMDAgfSk7XG4gIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1wcm9tcHRcIikuZmlsbChcInN0cmVhbSBwbGVhc2VcIik7XG4gIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1zdGFydFwiKS5jbGljaygpO1xuICBhd2FpdCBleHBlY3QocGFnZSkudG9IYXZlVVJMKC9cXC9zXFwvLywgeyB0aW1lb3V0OiAyMF8wMDAgfSk7XG4gIGNvbnN0IGluc3RhbmNlSWQgPSBuZXcgVVJMKHBhZ2UudXJsKCkpLnBhdGhuYW1lLnNwbGl0KFwiL1wiKS5wb3AoKSE7XG4gIGF3YWl0IGFuc3dlclBlbmRpbmdBcHByb3ZhbHMocGFnZSwgaW5zdGFuY2VJZCk7XG5cbiAgLy8gVGhlIGZha2UgTm9kZSByZXBsaWVzIHRvIGEgYHN0cmVhbSBgIHByb21wdCBhcyBhbiBvcGVuL2FwcGVuZCBjaGFpbi4gVGhlXG4gIC8vIGFzc2VtYmxlciBtdXN0IG1lcmdlIGl0IGludG8gT05FIGJ1YmJsZSBjYXJyeWluZyB0aGUgd2hvbGUgdGV4dCDigJQgYSBidWJibGVcbiAgLy8gcGVyIGNodW5rIGlzIGV4YWN0bHkgdGhlIFwibm90IHN0cmVhbWluZywganVzdCByZS1yZW5kZXJpbmdcIiBmYWlsdXJlLlxuICBhd2FpdCBwYWdlLmdldEJ5VGVzdElkKFwiY29tcG9zZXItaW5wdXRcIikuZmlsbChcInN0cmVhbSB0aGUgcmVwbHlcIik7XG4gIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJjb21wb3Nlci1zZW5kXCIpLmNsaWNrKCk7XG4gIGNvbnN0IHN0cmVhbWVkID0gcGFnZS5nZXRCeVRlc3RJZChcIm1lc3NhZ2VcIikuZmlsdGVyKHsgaGFzVGV4dDogXCJlY2hvOiBzdHJlYW0gdGhlIHJlcGx5XCIgfSk7XG4gIGF3YWl0IGV4cGVjdChzdHJlYW1lZCkudG9IYXZlQ291bnQoMSwgeyB0aW1lb3V0OiAyMF8wMDAgfSk7XG5cbiAgLy8gSW5qZWN0ZWQgcmVjb3JkcyBhcmUgY29sbGFwc2VkLCBub3QgZHJhd24gYXMgXCJZb3VcIiwgYW5kIHRoZSB0b2dnbGUgaGlkZXNcbiAgLy8gdGhlbSBlbnRpcmVseS4gVGhlIGZha2UgTm9kZSBlbWl0cyBub25lLCBzbyBhc3NlcnQgdGhlIGludmFyaWFudCB0aGF0XG4gIC8vIGhvbGRzIGVpdGhlciB3YXk6IG5vdGhpbmcgY2xhaW1pbmcgdG8gYmUgdGhlIHVzZXIgdGhhdCB0aGUgdXNlciBuZXZlciBzZW50LlxuICBjb25zdCBidWJibGVzID0gYXdhaXQgcGFnZS5nZXRCeVRlc3RJZChcIm1lc3NhZ2VcIikuZmlsdGVyKHsgaGFzVGV4dDogL15Zb3UvIH0pLmFsbFRleHRDb250ZW50cygpO1xuICBleHBlY3QoYnViYmxlcy5ldmVyeSgodGV4dCkgPT4gIXRleHQuaW5jbHVkZXMoXCI8dGFzay1ub3RpZmljYXRpb24+XCIpKSkudG9CZSh0cnVlKTtcbiAgZXhwZWN0KGJ1YmJsZXMuZXZlcnkoKHRleHQpID0+ICF0ZXh0LmluY2x1ZGVzKFwiPGNvbW1hbmQtbmFtZT5cIikpKS50b0JlKHRydWUpO1xuXG4gIGF3YWl0IHNob3QocGFnZSwgXCJuYXRpdmUtcHR5LXdlYi0zLXN0cnVjdHVyZWQtc3RyZWFtLTE0NDAucG5nXCIpO1xuICBhd2FpdCBwYWdlLnNldFZpZXdwb3J0U2l6ZSh7IHdpZHRoOiA0MDAsIGhlaWdodDogODQwIH0pO1xuICBhd2FpdCBzaG90KHBhZ2UsIFwibmF0aXZlLXB0eS13ZWItMy1zdHJ1Y3R1cmVkLXN0cmVhbS00MDAucG5nXCIpO1xuICBhd2FpdCBwYWdlLnNldFZpZXdwb3J0U2l6ZSh7IHdpZHRoOiAxNDQwLCBoZWlnaHQ6IDkwMCB9KTtcblxuICAvLyBSZWxlYXNlIHRoZSBwbGFjZW1lbnQgc2xvdC4gVGhlIGZha2UgaG9zdCBhZHZlcnRpc2VzIGBtYXhJbnN0YW5jZXM6IDhgXG4gIC8vIGFuZCB0aGUgc3VpdGUgaXMgc2VyaWFsLCBzbyBhIHNwZWMgdGhhdCBsZWF2ZXMgaXRzIGluc3RhbmNlIGxpdmUgc3BlbmRzXG4gIC8vIG9uZSBvZiB0aG9zZSBzbG90cyBmb3IgdGhlIHJlc3Qgb2YgdGhlIHJ1biDigJQgYSBsYXRlciBzcGVjIGNyZWF0aW5nXG4gIC8vIHNldmVyYWwgc2Vzc2lvbnMgdGhlbiBmYWlscyBwbGFjZW1lbnQgd2l0aCBhIG5vbi1PSyBQT1NUIC92MS9pbnN0YW5jZXMsXG4gIC8vIGZhciBmcm9tIHRoZSBzcGVjIHRoYXQgYWN0dWFsbHkgbGVha2VkLlxuICAvL1xuICAvLyBUaGUgZmFrZSBOb2RlIG5ldmVyIGVtaXRzIGFuIGV4aXQgbGlmZWN5Y2xlICh0aGVyZSBpcyBubyByZWFsIHByb2Nlc3MgdG9cbiAgLy8gbG9zZSksIHNvIGBpbnN0YW5jZS5jbG9zZWAgYWxvbmUgbGVhdmVzIHRoZSByb3cgYHJ1bm5pbmdgIGFuZCB0aGUgc2xvdFxuICAvLyBjb3VudGVkLiBERUxFVEUgc2V0dGxlcyB0byBgZXhpdGVkYCBiZXN0LWVmZm9ydCByZWdhcmRsZXNzLCB3aGljaCBpcyB0aGVcbiAgLy8gY2xlYW51cCBwYXRoIHRoZSByZXN0IG9mIHRoZSBzdWl0ZSByZWxpZXMgb24uXG4gIGNvbnN0IGRlbGV0ZWQgPSBhd2FpdCBwYWdlLnJlcXVlc3QuZGVsZXRlKGAvdjEvaW5zdGFuY2VzLyR7aW5zdGFuY2VJZH0/Zm9yY2U9MWApO1xuICBleHBlY3QoZGVsZXRlZC5vaygpKS50b0JlKHRydWUpO1xuICBhd2FpdCBleHBlY3RcbiAgICAucG9sbChhc3luYyAoKSA9PiAoYXdhaXQgcGFnZS5yZXF1ZXN0LmdldChgL3YxL2luc3RhbmNlcy8ke2luc3RhbmNlSWR9YCkpLnN0YXR1cygpKVxuICAgIC50b0JlKDQwNCk7XG59KTtcbiJdLCJtYXBwaW5ncyI6IkFBQUEsU0FBU0EsTUFBTSxFQUFFQyxJQUFJLFFBQW1CLGtCQUFrQjtBQUMxRCxTQUFTQyxLQUFLLFFBQVEsa0JBQWtCO0FBQ3hDLE9BQU9DLElBQUksTUFBTSxXQUFXO0FBQzVCLFNBQVNDLGFBQWEsUUFBUSxVQUFVO0FBQ3hDLFNBQVNDLG1CQUFtQixFQUFFQyxLQUFLLFFBQVEsWUFBWTtBQUV2RCxNQUFNQyxJQUFJLEdBQUdKLElBQUksQ0FBQ0ssT0FBTyxDQUFDSixhQUFhLENBQUNLLE1BQU0sQ0FBQ0MsSUFBSSxDQUFDQyxHQUFHLENBQUMsQ0FBQztBQUN6RCxNQUFNQyxRQUFRLEdBQUdDLE9BQU8sQ0FBQ0MsR0FBRyxDQUFDQyxlQUFlLEtBQUssR0FBRztBQUNwRCxNQUFNQyxPQUFPLEdBQUdKLFFBQVEsR0FDcEJULElBQUksQ0FBQ2MsSUFBSSxDQUFDVixJQUFJLEVBQUUsK0JBQStCLENBQUMsR0FDaERKLElBQUksQ0FBQ2MsSUFBSSxDQUFDVixJQUFJLEVBQUUsbUNBQW1DLENBQUM7QUFFeEQsZUFBZVcsSUFBSUEsQ0FBQ0MsSUFBVSxFQUFFQyxJQUFZLEVBQUU7RUFDNUMsTUFBTWxCLEtBQUssQ0FBQ2MsT0FBTyxFQUFFO0lBQUVLLFNBQVMsRUFBRTtFQUFLLENBQUMsQ0FBQztFQUN6QyxNQUFNRixJQUFJLENBQUNHLFVBQVUsQ0FBQztJQUFFbkIsSUFBSSxFQUFFQSxJQUFJLENBQUNjLElBQUksQ0FBQ0QsT0FBTyxFQUFFSSxJQUFJLENBQUM7SUFBRUcsVUFBVSxFQUFFO0VBQVcsQ0FBQyxDQUFDO0FBQ25GOztB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