import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { expectCookieSession, login } from "./hub-auth";

const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence
  ? path.join(here, "../../../docs/design/evidence")
  : path.join(here, "../../test-results/native-pty-web");

async function shot(page: Page, name: string) {
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
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
async function answerPendingApprovals(page: Page, instanceId: string) {
  const items = () =>
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
      return (body.items ?? []).filter((item) => item.instanceId === id);
    }, instanceId);

  await expect
    .poll(async () => (await items()).length, { timeout: 20_000, message: "launch approval exists" })
    .toBeGreaterThan(0);

  // Clear inside the poll: every attempt re-lists the pending set and answers
  // whatever is still open, so a late-populated options array, a rejected
  // answer, or a second approval cannot strand the launch until the timeout.
  // An id is remembered only after its answer POST succeeded.
  const answered = new Set<string>();
  await expect
    .poll(
      async () => {
        const pending = (await items()).filter((item) => item.state === "pending");
        for (const item of pending) {
          if (answered.has(item.id)) continue;
          const optionId = item.request?.options?.[0]?.id;
          if (!optionId) continue;
          const ok = await page.evaluate(
            async ({ iid, optionId, digest }) => {
              const res = await fetch(`/v1/interactions/${iid}/answer`, {
                method: "POST",
                credentials: "include",
                headers: { "content-type": "application/json" },
                body: JSON.stringify({
                  answer: { kind: "approval", optionId, inputDigest: digest ?? "" },
                }),
              });
              return res.ok;
            },
            { iid: item.id, optionId, digest: item.request?.inputDigest },
          );
          if (ok) answered.add(item.id);
        }
        return pending.length;
      },
      { timeout: 20_000, message: "approvals clear" },
    )
    .toBe(0);
}


test.describe.configure({ mode: "serial" });

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
async function sweepFakeNodeInstances(page: Page) {
  const hostId = await page.evaluate(async () => {
    const res = await fetch("/v1/hosts", { credentials: "include" });
    if (!res.ok) return null;
    const body = (await res.json()) as {
      items?: { id?: string; label?: string }[];
    };
    return body.items?.find((host) => host.label === "e2e-fake-node")?.id ?? null;
  });
  if (!hostId) return;
  const items = await page.evaluate(async (id) => {
    const res = await fetch("/v1/instances", { credentials: "include" });
    if (!res.ok) return [];
    const body = (await res.json()) as {
      items?: { instanceId?: string; hostId?: string }[];
    };
    return (body.items ?? []).filter((item) => item.hostId === id && item.instanceId);
  }, hostId);
  for (const item of items) {
    await page.request.delete(`/v1/instances/${item.instanceId}?force=1`).catch(() => undefined);
  }
}

test.beforeAll(async ({ browser }) => {
  if (process.env.HUB_E2E_EXTERNAL === "1") return;
  const page = await browser.newPage();
  try {
    await login(page);
    await sweepFakeNodeInstances(page);
  } finally {
    await page.close();
  }
});

test.afterEach(async ({ page }) => {
  if (process.env.HUB_E2E_EXTERNAL === "1") return;
  // An unauthenticated page (a failure before login) simply sweeps nothing.
  await sweepFakeNodeInstances(page).catch(() => undefined);
});

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

  await page.getByTitle("新建", { exact: true }).click();
  await expect(page.getByTestId("new-session-sheet")).toBeVisible();
  await expect(page.getByTestId("new-session-host")).toContainText("e2e-fake-node", { timeout: 20_000 });
  const host = await page.getByTestId("new-session-host").locator("option").filter({ hasText: "e2e-fake-node" }).getAttribute("value");
  await page.getByTestId("new-session-host").selectOption(host!);
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0);
  await page.getByTestId("new-session-advanced").click();
  await page.getByTestId("new-session-tui").selectOption("default");
  await page.getByTestId("new-session-prompt").fill("hello from web hub");
  await expect(page.getByTestId("new-session-start")).toBeEnabled();
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
  const sessionPath = new URL(page.url()).pathname;
  const createdId = sessionPath.split("/")[2];
  const stored = await (await page.request.get(`/v1/instances/${createdId}`)).json();
  expect(stored.tui).toBe("default");

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
  // Reload waits for cookie-backed bootstrap and the durable journal read again.
  await expect(page.getByTestId("message").filter({ hasText: "echo: hello from web hub" })).toHaveCount(1, {
    timeout: 20_000,
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
  const ownApproval = () =>
    page
      .getByTestId("approval-row")
      .filter({ has: page.locator(`a[href="/s/${createdId}"]`) });
  const approval = ownApproval().filter({ hasText: "echo e2e" });
  await expect(approval).toBeVisible({ timeout: 20_000 });
  await approval.getByRole("button", { name: "允许一次" }).click();
  await expect(ownApproval()).toHaveCount(0, {
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

/**
 * D-027: an image pasted into the composer is staged on the Hub and its
 * metadata reaches the Node, with the bytes never entering a command frame.
 */
test("paste an image: it stages on the Hub and its metadata reaches the Node", async ({ page }) => {
  test.skip(process.env.HUB_E2E_EXTERNAL === "1", "Needs the in-process fake Node");
  await login(page);

  await page.getByTitle("新建", { exact: true }).click();
  await expect(page.getByTestId("new-session-sheet")).toBeVisible();
  await expect(page.getByTestId("new-session-host")).toContainText("e2e-fake-node", { timeout: 20_000 });
  const host = await page
    .getByTestId("new-session-host")
    .locator("option")
    .filter({ hasText: "e2e-fake-node" })
    .getAttribute("value");
  await page.getByTestId("new-session-host").selectOption(host!);
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0);
  await page.getByTestId("new-session-prompt").fill("attachment session");
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
  await expect(page.getByTestId("session-page")).toBeVisible();

  // The fake Node raises an approval on every create, and a pending one holds
  // this instance's composer disabled. Answer exactly this instance's
  // approvals through the API: the UI rows are all labelled alike, so picking
  // the right one by text is not reliable here.
  const instanceId = new URL(page.url()).pathname.split("/").pop() as string;
  await expect
    .poll(
      async () =>
        await page.evaluate(async (id) => {
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

  // A real 1x1 red PNG, pasted the way a browser delivers one.
  const uploads: number[] = [];
  page.on("response", (response) => {
    if (new URL(response.url()).pathname === "/v1/objects") uploads.push(response.status());
  });
  await page.getByTestId("composer-input").fill("what colour is the image?");
  await page.evaluate(async () => {
    const base64 =
      "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
    const bytes = Uint8Array.from(atob(base64), (char) => char.charCodeAt(0));
    const file = new File([bytes], "red.png", { type: "image/png" });
    const data = new DataTransfer();
    data.items.add(file);
    const area = document.querySelector("[data-testid='composer-input']");
    area?.dispatchEvent(new ClipboardEvent("paste", { clipboardData: data, bubbles: true }));
  });

  // The chip appears, the upload succeeds, and only then can the send go.
  await expect(page.getByTestId("attachment-chip")).toBeVisible({ timeout: 20_000 });
  await expect.poll(() => uploads, { timeout: 20_000 }).toContain(200);
  await expect(page.getByTestId("composer-send")).toBeEnabled({ timeout: 20_000 });
  await page.getByTestId("composer-send").click();

  // The Node echoes the resolved media type, proving the metadata arrived.
  await expect(
    page.getByTestId("message").filter({ hasText: "[attachments: image/png]" }),
  ).toBeVisible({ timeout: 20_000 });
  // The chip is consumed by the send and the bubble keeps the thumbnail.
  await expect(page.getByTestId("attachment-chip")).toHaveCount(0);
  await expect(page.getByTestId("sent-attachments").first()).toBeVisible();
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
  await page.getByTitle("新建", { exact: true }).click();
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
  await expect(slider).toHaveAttribute("data-tiers", "low,medium,high,xhigh,max,ultracode");
  const box = await slider.boundingBox();
  expect(box).toBeTruthy();
  await page.mouse.move(box!.x + box!.width - 3, box!.y + box!.height / 2);
  await page.mouse.down();
  await page.mouse.move(box!.x + box!.width - 3, box!.y + box!.height / 2, { steps: 3 });
  await page.mouse.up();
  // The far-right stop is ultracode: the xhigh tier plus the workflow flag.
  // The chip only re-renders once instance.configure round-trips through the
  // Hub, so these wait on the wire like every other live assertion here.
  await expect(page.getByTestId("composer")).toHaveAttribute("data-effort", "ultracode", {
    timeout: 20_000,
  });
  await expect(page.getByTestId("composer")).toHaveAttribute("data-ultracode", "1", {
    timeout: 20_000,
  });
  await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-ember", "1", {
    timeout: 20_000,
  });
  await expect
    .poll(() =>
      configureBodies.some(
        (body) => body.operation === "instance.configure" && body.payload?.effort?.name === "ultracode",
      ),
    )
    .toBeTruthy();

  await slider.focus();
  await page.keyboard.press("Home");
  await expect(page.getByTestId("composer")).toHaveAttribute("data-effort", "low", {
    timeout: 20_000,
  });
  await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-ember", "0", {
    timeout: 20_000,
  });
  await expect
    .poll(() =>
      configureBodies.some(
        (body) => body.operation === "instance.configure" && body.payload?.effort?.name === "low",
      ),
    )
    .toBeTruthy();
});

/**
 * D-028 §5.1/§1.0: New Session defaults to the native shell-pty carrier (the
 * choice comes from the fake Node's driverInventory), and the resulting
 * session carries BOTH projections — terminal and 结构.
 */
test("native PTY default from the host matrix, with both projections", async ({ page }) => {
  test.skip(process.env.HUB_E2E_EXTERNAL === "1", "Needs the in-process fake Node's matrix");
  await login(page);
  await page.goto("/sessions/new");
  await expect(page.getByTestId("new-session-host")).toContainText("e2e-fake-node", { timeout: 20_000 });

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
  await page.setViewportSize({ width: 400, height: 840 });
  await shot(page, "native-pty-web-1-new-session-400.png");
  await page.setViewportSize({ width: 1440, height: 900 });

  await page.getByTestId("new-session-prompt").fill("native pty session");
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });

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
test("composer queue / steer / interrupt states on a working native session", async ({ page }) => {
  test.skip(process.env.HUB_E2E_EXTERNAL === "1", "Needs the in-process fake Node");
  const commands: { operation?: string; payload?: { mode?: string; prompt?: string } }[] = [];
  page.on("request", (request) => {
    if (request.method() !== "POST") return;
    if (!new URL(request.url()).pathname.endsWith("/commands")) return;
    const body = request.postDataJSON() as (typeof commands)[number] | null;
    if (body) commands.push(body);
  });

  await login(page);
  await page.goto("/sessions/new");
  await expect(page.getByTestId("new-session-host")).toContainText("e2e-fake-node", { timeout: 20_000 });
  await page.getByTestId("new-session-prompt").fill("working session");
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
  const instanceId = new URL(page.url()).pathname.split("/").pop() as string;
  await answerPendingApprovals(page, instanceId);

  // Answering the create approval completes the fake turn: the composer is
  // idle. Start a fresh "hold-working" turn to exercise the working controls.
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-status", "idle", {
    timeout: 10_000,
  });
  await page.getByTestId("composer-input").fill("hold-working long tool");
  await page.getByTestId("composer-send").click();
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-status", "working", {
    timeout: 10_000,
  });
  // While `sending` the dock is disabled; type the held text first so the
  // queue button's enablement marks the settled POST flight (it is also
  // disabled on empty text, so it can only be enabled with text present).
  await page.getByTestId("composer-input").fill("after this turn");
  await expect(page.getByTestId("composer-queue")).toBeEnabled({ timeout: 10_000 });

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
  await page.setViewportSize({ width: 400, height: 840 });
  await shot(page, "native-pty-web-1-composer-working-400.png");
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.getByTestId("composer-input").fill("jump now");
  await steer.click();
  // D-042: the steer gesture confirms through the in-app Sheet, not
  // window.confirm.
  await expect(page.getByTestId("composer-confirm-title")).toHaveText("插队发送");
  await page.getByTestId("composer-confirm-ok").click();
  await expect
    .poll(() => commands.some((c) => c.operation === "instance.send" && c.payload?.mode === "steer"))
    .toBeTruthy();
  // The steer interrupted the turn (badge) and the fake Node reported idle,
  // which flushes the held row as an ordinary new turn.
  await expect(page.getByTestId("composer-interrupted-chip")).toBeVisible();
  await expect
    .poll(() => commands.filter((c) => c.operation === "instance.send").length)
    .toBeGreaterThanOrEqual(3);
  const sends = commands.filter((c) => c.operation === "instance.send");
  expect(sends[0]?.payload?.prompt).toBe("hold-working long tool");
  expect(sends[1]?.payload).toMatchObject({ prompt: "jump now", mode: "steer" });
  expect(sends.at(-1)?.payload?.prompt).toBe("later");

  // Plain Esc interrupts: start another hold-working turn, then cancel it.
  await page.getByTestId("composer-input").fill("hold-working stop me");
  await page.getByTestId("composer-send").click();
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-status", "working", {
    timeout: 10_000,
  });
  await expect(page.getByTestId("composer-interrupt")).toBeEnabled({ timeout: 10_000 });
  await page.getByTestId("composer-input").focus();
  await page.keyboard.press("Escape");
  // D-042: Esc opens the in-app Sheet confirm; Esc inside it cancels, so
  // press the destructive action explicitly.
  await expect(page.getByTestId("composer-confirm-title")).toHaveText("打断当前 turn");
  await page.getByTestId("composer-confirm-ok").click();
  await expect
    .poll(() => commands.some((c) => c.operation === "instance.cancel"))
    .toBeTruthy();
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-status", "idle", {
    timeout: 10_000,
  });
  await expect(page.getByTestId("composer-interrupt")).toHaveCount(0);
  await expect(page.getByTestId("composer-steer")).toHaveCount(0);
  await expect(page.getByTestId("composer-send")).toHaveAttribute("data-mode", "new-turn");
});

test("a hook-carried approval shows the real tool input and an always-allow option", async ({
  page,
}) => {
  // D-028 §4.4 tier A end to end through the Hub: the card the Node builds
  // from a real PermissionRequest carries the harness-hook carrier, the tool's
  // actual input rather than a screen scrape, and an always-allow button that
  // exists only because the harness offered a permission_suggestion.
  test.skip(
    process.env.HUB_E2E_EXTERNAL === "1",
    "External Node is covered by the real shell workspace flow",
  );
  await login(page);

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
  // The fake Node raises the tier A card for a prompt naming the hook path.
  await page.getByTestId("new-session-prompt").fill("hook-approval please");
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
  const instanceId = new URL(page.url()).pathname.split("/").pop()!;

  await page.goto("/approvals");
  await expect(page.getByTestId("approvals-page")).toBeVisible();
  const row = page.getByTestId("approval-row").filter({ hasText: "/tmp/hook-approval.txt" });
  // The real tool input is what the human reads before deciding (§2.2).
  await expect(row).toBeVisible({ timeout: 20_000 });
  await expect(row).toContainText("Write");
  // All three options the card carried, including the suggested grant.
  await expect(row.getByRole("button", { name: "允许一次" })).toBeVisible();
  await expect(row.getByRole("button", { name: "拒绝" })).toBeVisible();
  const always = row.getByRole("button", { name: "始终允许 (acceptEdits)" });
  await expect(always).toBeVisible();

  await always.click();
  await expect(
    page.getByTestId("approval-row").filter({ hasText: "/tmp/hook-approval.txt" }),
  ).toHaveCount(0, { timeout: 20_000 });

  // The card WAS this instance's blocking launch approval; the click above has
  // already answered it. Do NOT call answerPendingApprovals here: once the
  // fake node's card is answered it leaves interaction.list (it serves only
  // its pending map), so the helper's "an interaction exists" wait would time
  // out on the already-resolved card. The enabled composer below is the
  // release signal.
  await page.goto(`/s/${instanceId}`);
  await expect(page.getByTestId("session-page")).toBeVisible();
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });

  // Release the placement slot. The fake host advertises maxInstances 8 and
  // the suite is serial, so a session left live here makes a later spec fail
  // placement (PLACEMENT_UNSATISFIABLE) far from the spec that leaked it. The
  // fake Node never exits, so only a forced DELETE settles the row.
  const deleted = await page.request.delete(`/v1/instances/${instanceId}?force=1`);
  expect(deleted.ok()).toBe(true);
  await expect
    .poll(async () => (await page.request.get(`/v1/instances/${instanceId}`)).status())
    .toBe(404);
});

/**
 * D-028 §7 in the browser: assistant text must grow in place as deltas land,
 * and records the human did not write must not render as their bubble.
 *
 * «现在 Structural 的界面不是按文本流式出现的…我觉得它的实时性不够» and
 * «结构化界面会把追加的 prompt 信息也额外展示了 … 容易让人误解是我发了这些信息».
 */
test("structured view streams assistant text and separates injected records", async ({ page }) => {
  test.skip(process.env.HUB_E2E_EXTERNAL === "1", "Needs the in-process fake Node");
  await login(page);
  await page.goto("/sessions/new");
  await expect(page.getByTestId("new-session-host")).toContainText("e2e-fake-node", { timeout: 20_000 });
  await page.getByTestId("new-session-prompt").fill("stream please");
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
  const instanceId = new URL(page.url()).pathname.split("/").pop()!;
  await answerPendingApprovals(page, instanceId);

  // The fake Node replies to a `stream ` prompt as an open/append chain. The
  // assembler must merge it into ONE bubble carrying the whole text — a bubble
  // per chunk is exactly the "not streaming, just re-rendering" failure.
  await page.getByTestId("composer-input").fill("stream the reply");
  await page.getByTestId("composer-send").click();
  const streamed = page.getByTestId("message").filter({ hasText: "echo: stream the reply" });
  await expect(streamed).toHaveCount(1, { timeout: 20_000 });

  // Injected records are collapsed, not drawn as "You", and the toggle hides
  // them entirely. The fake Node emits none, so assert the invariant that
  // holds either way: nothing claiming to be the user that the user never sent.
  const bubbles = await page.getByTestId("message").filter({ hasText: /^You/ }).allTextContents();
  expect(bubbles.every((text) => !text.includes("<task-notification>"))).toBe(true);
  expect(bubbles.every((text) => !text.includes("<command-name>"))).toBe(true);

  await shot(page, "native-pty-web-3-structured-stream-1440.png");
  await page.setViewportSize({ width: 400, height: 840 });
  await shot(page, "native-pty-web-3-structured-stream-400.png");
  await page.setViewportSize({ width: 1440, height: 900 });

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
  await expect
    .poll(async () => (await page.request.get(`/v1/instances/${instanceId}`)).status())
    .toBe(404);
});
