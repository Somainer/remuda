import { expect, test } from "@playwright/test";
import { expectCookieSession, login } from "./hub-auth";

test.describe.configure({ mode: "serial" });

test("device login, hosts, create/send/close, follow, approvals", async ({ page }) => {
  test.skip(process.env.HUB_E2E_EXTERNAL === "1", "External Node is covered by the real shell workspace flow");
  const followUrls: string[] = [];
  page.on("websocket", (socket) => {
    // Vite authenticates HMR with its own token; restrict this check to Hub.
    if (new URL(socket.url()).pathname === "/v1/follow") followUrls.push(socket.url());
  });
  await login(page);

  await page.getByTitle("更多", { exact: true }).click();
  await page.getByRole("menuitem", { name: "主机", exact: true }).click();
  await expect(page.getByTestId("hosts-page")).toBeVisible();
  await expect(page.getByTestId("host-row").filter({ hasText: "e2e-fake-node" })).toBeVisible({
    timeout: 20_000,
  });

  await page.locator('a[href="/sessions/new"]').first().click();
  await expect(page.getByTestId("new-session-sheet")).toBeVisible();
  await expect(page.getByTestId("new-session-host")).toContainText("e2e-fake-node", { timeout: 20_000 });
  const host = await page.getByTestId("new-session-host").locator("option").filter({ hasText: "e2e-fake-node" }).getAttribute("value");
  await page.getByTestId("new-session-host").selectOption(host!);
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0);
  await page.getByTestId("new-session-prompt").fill("hello from web hub");
  await expect(page.getByTestId("new-session-start")).toBeEnabled();
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
  const sessionPath = new URL(page.url()).pathname;
  await expect(page.getByTestId("session-page")).toBeVisible();
  await expect(page.getByTestId("message").filter({ hasText: /^You/ })).toContainText("hello from web hub", {
    timeout: 20_000,
  });
  await expect(page.getByTestId("message").filter({ hasText: "echo: hello from web hub" })).toHaveCount(1, {
    timeout: 20_000,
  });
  await expect.poll(() => followUrls.length).toBeGreaterThan(0);
  expect(followUrls.every((url) => !new URL(url).searchParams.has("token"))).toBe(true);
  await page.reload();
  await expectCookieSession(page);
  await expect(page.getByTestId("message").filter({ hasText: "echo: hello from web hub" })).toHaveCount(1);
  await expect(page.getByTestId("composer-bar")).toBeVisible();

  await page.goto("/approvals");
  await expect(page.getByTestId("approvals-page")).toBeVisible();
  const approval = page.getByTestId("approval-row").filter({ hasText: "echo e2e" });
  await expect(approval).toBeVisible({ timeout: 20_000 });
  await approval.getByRole("button", { name: "允许一次" }).click();
  await expect(page.getByTestId("approval-row").filter({ hasText: "echo e2e" })).toHaveCount(0, {
    timeout: 20_000,
  });

  await page.goto(sessionPath);
  await expect(page.getByTestId("session-page")).toBeVisible();
  await expect(page.getByTestId("composer-bar")).toBeVisible();
  await expect(page.getByTestId("composer-input")).toBeEnabled();
  await page.getByTestId("composer-input").fill("second turn");
  await page.getByTestId("composer-send").click();
  await expect(page.getByTestId("message").filter({ hasText: "echo: second turn" })).toBeVisible({
    timeout: 20_000,
  });

  await page.getByRole("button", { name: "Stop" }).click();
  await expect(page.getByTestId("session-page")).toBeVisible();
  expect(followUrls.every((url) => !new URL(url).searchParams.has("token"))).toBe(true);
});

test("real Node: register a project, create a shell in it, close and unregister", async ({ page }) => {
  test.skip(process.env.HUB_E2E_EXTERNAL !== "1", "Requires the operator's remuda dev");
  const path = process.env.HUB_E2E_WORKSPACE;
  expect(path, "Set HUB_E2E_WORKSPACE to an existing allowed directory on the Node").toBeTruthy();
  const followUrls: string[] = [];
  page.on("websocket", (socket) => {
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
  await observer.route("**/v1/hosts", (route) => route.fulfill({ json: frozenHosts }));
  await page.getByTestId("workspace-add").click();
  await page.getByTestId("workspace-register-path").fill(path!);
  const registered = page.waitForResponse((response) => response.request().method() === "POST"
    && new URL(response.url()).pathname === `/v1/hosts/${hostId}/workspaces`);
  await page.getByTestId("workspace-register-submit").click();
  const registration = await registered;
  expect(registration.ok()).toBe(true);
  const snapshot = await registration.json();
  const workspace = snapshot.workspaces.find((row: { workspaceId: string }) => row.workspaceId === snapshot.workspaceId);
  expect(workspace).toBeTruthy();
  await expect(page.getByTestId("new-session-workspace")).toHaveValue(workspace.workspaceId);
  const observerOption = observer.getByTestId("new-session-workspace").locator(`option[value="${workspace.workspaceId}"]`);
  await expect(observerOption).toHaveCount(1);
  await observer.waitForRequest((request) => new URL(request.url()).pathname === "/v1/hosts");
  await expect(observerOption).toHaveCount(1);
  await expect(page.getByTestId("new-session-cwd")).toHaveValue("");
  await page.getByTestId("new-session-kind-terminal").click();
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, { timeout: 30_000 });
  const sessionPath = new URL(page.url()).pathname;
  const instanceId = sessionPath.split("/").at(-1)!;
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-view", "tty");
  await expect(page.locator("[data-tty-lab='1']")).toHaveAttribute("data-tty-status", "live", { timeout: 30_000 });
  const terminalInput = page.getByRole("textbox", { name: "Terminal input", exact: true });
  await terminalInput.focus();
  await expect(terminalInput).toBeFocused();
  await page.keyboard.type("printf 'WORKSPACE_REG_CWD=%s\\n' \"$PWD\"");
  await page.keyboard.press("Enter");
  await expect(page.getByTestId("tty-ansi-preview")).toContainText(`WORKSPACE_REG_CWD=${workspace.root}`, { timeout: 20_000 });
  await expect.poll(() => followUrls.length).toBeGreaterThan(0);
  expect(followUrls.every((url) => !new URL(url).searchParams.has("token"))).toBe(true);
  await page.reload();
  await expectCookieSession(page);
  await expect(page.getByTestId("session-page")).toBeVisible();
  await page.getByRole("button", { name: "Stop", exact: true }).click();
  await expect.poll(async () => {
    const response = await page.request.get(`/v1/instances/${instanceId}`);
    return (await response.json()).lifecycle;
  }, { timeout: 30_000 }).toBe("exited");
  await page.goto("/hosts");
  const row = page.getByTestId("host-workspace").filter({ hasText: workspace.root });
  await expect(row).toBeVisible();
  const unregistered = page.waitForResponse((response) => response.request().method() === "DELETE"
    && new URL(response.url()).pathname === `/v1/hosts/${hostId}/workspaces`);
  await row.getByRole("button", { name: `移除目录 ${workspace.root}`, exact: true }).click();
  expect((await unregistered).ok()).toBe(true);
  await expect(row).toHaveCount(0);
  await expect(observerOption).toHaveCount(0);
  await observer.close();
  await page.goto("/sessions/new");
  await hostPicker.selectOption(hostId);
  await expect(page.getByTestId("new-session-workspace").locator(`option[value="${workspace.workspaceId}"]`)).toHaveCount(0);
});

test("effort slider drag and keyboard send instance.configure", async ({ page }) => {
  test.skip(process.env.HUB_E2E_EXTERNAL === "1", "External Node is covered by the real shell workspace flow");
  await login(page);
  await page.locator('a[href="/sessions/new"]').first().click();
  await expect(page.getByTestId("new-session-sheet")).toBeVisible();
  await expect(page.getByTestId("new-session-host")).toContainText("e2e-fake-node", { timeout: 20_000 });
  const host = await page
    .getByTestId("new-session-host")
    .locator("option")
    .filter({ hasText: "e2e-fake-node" })
    .getAttribute("value");
  await page.getByTestId("new-session-host").selectOption(host!);
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0);
  await page.getByTestId("new-session-prompt").fill("effort slider e2e");
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
  await expect(page.getByTestId("composer-bar")).toBeVisible();
  await expect(page.getByTestId("model-effort-chip")).toBeVisible();

  const configureBodies: { operation?: string; payload?: { effort?: { name?: string; index?: number } } }[] = [];
  page.on("request", (request) => {
    if (request.method() !== "POST") return;
    if (!new URL(request.url()).pathname.endsWith("/commands")) return;
    const body = request.postDataJSON() as (typeof configureBodies)[number] | null;
    if (body) configureBodies.push(body);
  });

  await page.getByTestId("model-effort-chip").click();
  const slider = page.getByTestId("effort-slider");
  await expect(slider).toBeVisible();
  await expect(slider).toHaveAttribute("data-tiers", "default,think,think-hard,ultracode");
  const box = await slider.boundingBox();
  expect(box).toBeTruthy();
  await page.mouse.move(box!.x + box!.width - 3, box!.y + box!.height / 2);
  await page.mouse.down();
  await page.mouse.move(box!.x + box!.width - 3, box!.y + box!.height / 2, { steps: 3 });
  await page.mouse.up();
  await expect(page.getByTestId("composer")).toHaveAttribute("data-effort", "ultracode");
  await expect
    .poll(() =>
      configureBodies.some(
        (body) => body.operation === "instance.configure" && body.payload?.effort?.name === "ultracode",
      ),
    )
    .toBeTruthy();

  await slider.focus();
  await page.keyboard.press("Home");
  await expect(page.getByTestId("composer")).toHaveAttribute("data-effort", "default");
  await expect
    .poll(() =>
      configureBodies.some(
        (body) => body.operation === "instance.configure" && body.payload?.effort?.name === "default",
      ),
    )
    .toBeTruthy();
});
