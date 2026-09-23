import { expect, test, type Page, type TestInfo } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { bootstrapToken } from "./hub-auth";

/**
 * c-mfix: the three owner-reported iPhone (Safari) defects, verified on the
 * WebKit engine with a phone device descriptor — the combination the earlier
 * mobile acceptance never used (Playwright/Chromium render screenshots only):
 *
 *   (a) tapping the composer and raising the soft keyboard used to leave an
 *       empty page — the shell shrank to visualViewport.height while staying
 *       anchored at layout y=0, so its upper rows sat outside the visible
 *       band. The shell now pins itself to the whole band (height +
 *       offsetTop); status bar, transcript and composer must remain visible
 *       and non-empty while the keyboard is up.
 *   (b) the tty segment must render a non-zero-height terminal on WebKit,
 *       with the fitted grid (rows/cols) populated; the effective renderer is
 *       recorded (webgl|canvas|dom).
 *   (c) `model` / `effort` are KNOWN observation kinds (protocol §5.1); they
 *       render as change records, never as 未识别事件. Genuinely unknown kinds
 *       still go through OpaqueRow (D-052).
 *
 * The soft keyboard is emulated the only way headless WebKit allows: the
 * visualViewport geometry is overridden (shrunk height, raised offsetTop — the
 * iPad/scroll case of iOS Safari) and its resize/scroll events fire, exactly
 * the signal the app listens to. Sentinels that depend on the fake Node
 * (/model:, /effort:) skip when the Node fails to emit them instead of
 * failing on an environment that cannot drive the scenario.
 */

test.describe.configure({ mode: "serial" });

const created: string[] = [];
const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence
  ? path.join(here, "../../../docs/design/evidence")
  : path.join(here, "../../test-results/evidence");

async function shot(page: Page, name: string, clipToBand = false) {
  if (!evidence) return;
  await mkdir(shotDir, { recursive: true });
  // A full-page capture still shows the layout viewport's off-band strip
  // above the keyboard; clip to the visualViewport band so the evidence is
  // the picture the phone screen actually displays.
  const clip = clipToBand
    ? await page.evaluate(() => ({
        x: 0,
        y: window.visualViewport.offsetTop,
        width: 390,
        height: window.visualViewport.height,
      }))
    : undefined;
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled", clip });
}

async function fakeHostId(page: Page): Promise<string | null> {
  const response = await page.request.get("/v1/hosts");
  if (!response.ok()) return null;
  const body = (await response.json()) as { items?: { id?: string; label?: string }[] };
  const host = (body.items ?? []).find((row) => row.label === "e2e-fake-node") ?? body.items?.[0];
  return host?.id ?? null;
}

async function createTerminal(page: Page): Promise<string> {
  const hostId = await fakeHostId(page);
  expect(hostId, "fake node host").toBeTruthy();
  const response = await page.request.post("/v1/instances", {
    data: { hostId, workspaceId: "wsp_e2e", kind: "terminal", driver: "shell-pty", prompt: "m realdevice" },
  });
  expect(response.ok(), `create terminal: ${response.status()} ${await response.text()}`).toBe(true);
  const body = (await response.json()) as { instance: { instanceId?: string; id?: string } };
  const id = body.instance.instanceId ?? body.instance.id;
  expect(id).toBeTruthy();
  created.push(id!);
  return id!;
}

async function createClaudeSession(page: Page, prompt: string): Promise<string> {
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  await expect(hostPicker).toContainText("e2e-fake-node", { timeout: 20_000 });
  const hostId = await hostPicker
    .locator("option")
    .filter({ hasText: "e2e-fake-node" })
    .getAttribute("value");
  expect(hostId).toBeTruthy();
  await hostPicker.selectOption(hostId!);
  await page.getByTestId("new-session-kind-claude").click();
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0, {
    timeout: 20_000,
  });
  await page.getByTestId("new-session-prompt").fill(prompt);
  await page.getByTestId("new-session-start").click();
  await page.waitForURL(/\/s\//, { timeout: 20_000 });
  const id = new URL(page.url()).pathname.split("/").pop()!;
  created.push(id);
  return id;
}

async function clearApprovals(page: Page, instanceId: string) {
  // Same answer/wait shape as effort-sync.hub.spec.ts: an approval answered
  // before the request is durable gets resurrected by its late journal event.
  await page.evaluate(async (id) => {
    const listPending = async () => {
      const body = await (await fetch("/v1/interactions", { credentials: "include" })).json();
      return (body.items ?? []).filter(
        (item: { instanceId?: string; state?: string }) =>
          item.instanceId === id && item.state === "pending",
      );
    };
    const deadline = Date.now() + 10_000;
    let mine = await listPending();
    while (mine.length === 0 && Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 100));
      mine = await listPending();
    }
    for (const item of mine) {
      const optionId = item.request?.options?.[0]?.id;
      if (!optionId) continue;
      await fetch(`/v1/interactions/${item.interactionId ?? item.id}/answer`, {
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
    let remaining = await listPending();
    while (remaining.length > 0 && Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 100));
      remaining = await listPending();
    }
  }, instanceId);
}

/**
 * Raise the soft keyboard the headless way: visualViewport becomes the short
 * band starting at offsetTop (the iOS scroll/Android-resize geometry), and
 * the listeners the app subscribes fire. Own-property override is preferred;
 * some engines define the getters on the prototype, so walk up as fallback.
 */
async function raiseKeyboard(page: Page, keyboardHeight = 308) {
  await page.evaluate(
    (keyboardHeight) => {
      const vv = window.visualViewport;
      // iOS scroll geometry: the visible band keeps the layout viewport's
      // width, its bottom edge stays put, and it starts keyboardHeight lower.
      const height = Math.max(240, window.innerHeight - keyboardHeight);
      const offsetTop = window.innerHeight - height;
      const override = (name: string, value: number) => {
        const desc = { configurable: true, get: () => value } as PropertyDescriptor;
        try {
          Object.defineProperty(vv, name, desc);
        } catch {
          let proto: object | null = vv;
          while (proto) {
            try {
              Object.defineProperty(proto, name, desc);
              break;
            } catch {
              proto = Object.getPrototypeOf(proto);
            }
          }
        }
      };
      override("height", height);
      override("offsetTop", offsetTop);
      vv.dispatchEvent(new Event("resize"));
      vv.dispatchEvent(new Event("scroll"));
      window.dispatchEvent(new Event("resize"));
    },
    keyboardHeight,
  );
  await page.waitForTimeout(250);
}

async function bandRect(page: Page, selector: string) {
  return page.evaluate((sel) => {
    const el = document.querySelector(sel);
    if (!el) return null;
    const r = el.getBoundingClientRect();
    const vv = window.visualViewport;
    return {
      top: Math.round(r.top),
      bottom: Math.round(r.bottom),
      height: Math.round(r.height),
      width: Math.round(r.width),
      text: (el.textContent ?? "").trim().length,
      bandTop: Math.round(vv.offsetTop),
      bandBottom: Math.round(vv.offsetTop + vv.height),
    };
  }, selector);
}

/**
 * The terminal refits across several animation frames after the viewport
 * change (ResizeObserver + debounced PTY resize); poll until its laid-out box
 * actually settles inside the band rather than measuring the mid-cascade box.
 */
async function waitForInBand(page: Page, selector: string, minHeight: number) {
  const deadline = Date.now() + 5_000;
  let last: Awaited<ReturnType<typeof bandRect>> = null;
  while (Date.now() < deadline) {
    last = await bandRect(page, selector);
    if (
      last &&
      last.height >= minHeight &&
      last.width > 0 &&
      last.top >= last.bandTop - 1 &&
      last.bottom <= last.bandBottom + 1
    ) {
      return last;
    }
    await page.waitForTimeout(100);
  }
  return last;
}

async function postCommand(page: Page, id: string, operation: string, payload: unknown) {
  const result = await page.evaluate(
    async ({ id, operation, payload }) => {
      const res = await fetch(`/v1/instances/${id}/commands`, {
        method: "POST",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ operation, payload }),
      });
      return { status: res.status, body: await res.text() };
    },
    { id, operation, payload },
  );
  expect(result.status, `command ${operation}: ${result.body}`).toBe(200);
}

/** Skip when the fake Node never emits the observation the UI must render. */
async function expectObservationOrSkip(page: Page, id: string, kind: string) {
  const journal = await page.evaluate(async (id) => {
    const res = await fetch(`/v1/instances/${id}/journal`, { credentials: "include" });
    return res.ok() ? await res.text() : "";
  }, id);
  if (!journal.includes(`"kind":"${kind}"`) && !journal.includes(`"kind": "${kind}"`)) {
    test.skip(true, `fake Node did not emit a ${kind} observation for the sentinel`);
  }
}

test.beforeEach(async ({ page }) => {
  // Mobile-UA login: the shared hub-auth.login() waits on /sessions
  // (session-list), but under a phone device descriptor the router's viewport
  // gate client-side-redirects /sessions → /m after mount, so it waits on a
  // list that unmounts. Tolerate either landing, and the redirect in flight.
  await page.goto("/login");
  const useCode = page.getByTestId("login-use-code");
  if (await useCode.isVisible().catch(() => false)) await useCode.click();
  await page.getByTestId("login-tab-bootstrap").click();
  await page.getByTestId("login-device-name").fill("mfix-webkit");
  await page.getByTestId("login-bootstrap-token").fill(bootstrapToken);
  await page.getByTestId("login-submit").click();
  await expect
    .poll(
      async () => {
        const url = new URL(page.url()).pathname;
        if (/\/m(?:[/?]|$)/.test(url) && (await page.getByTestId("home-list").count()) > 0) {
          return "m";
        }
        if (url.startsWith("/sessions") && (await page.getByTestId("session-list").count()) > 0) {
          return "sessions";
        }
        return null;
      },
      { timeout: 20_000 },
    )
    .toBeTruthy();
});

test.afterEach(async ({ page }) => {
  for (const id of created.splice(0)) {
    await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
  }
});

test("(a) soft keyboard leaves status bar, transcript and composer inside the visible band", async ({
  page,
}, testInfo) => {
  if (!(await fakeHostId(page))) test.skip(true, "fake Node not registered");
  const id = await createClaudeSession(page, "m realdevice keyboard");
  await clearApprovals(page, id);
  await page.goto(`/s/${id}/structured`);
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-view", "structured");
  // The fake Node appends an assistant journal line; the transcript must have
  // rendered content before the keyboard opens, otherwise "non-empty" is vacuous.
  await expect(page.getByTestId("transcript")).not.toBeEmpty({ timeout: 20_000 });

  // click focuses (and on touch engines taps) the composer, which is what
  // raises the real keyboard on the owner's phone.
  await page.getByTestId("composer-input").click();
  await raiseKeyboard(page);
  await shot(page, "m-realdevice-1-keyboard-390.png", true);

  // The shell itself is pinned to exactly the visualViewport band (either
  // Shell or PhoneShell renders the data-compact root).
  const shell = page.locator("[data-compact]");
  await expect(shell).toHaveCSS("position", "fixed");
  const shellBox = await shell.boundingBox();
  expect(shellBox).toBeTruthy();
  const vv = await page.evaluate(() => ({
    top: Math.round(window.visualViewport.offsetTop),
    height: Math.round(window.visualViewport.height),
  }));
  expect(Math.round(shellBox!.y)).toBe(vv.top);
  expect(Math.round(shellBox!.height)).toBe(vv.height);

  // All three rows the owner reported missing stay visible AND non-empty, and
  // sit fully inside the band above the keyboard.
  for (const [label, selector] of [
    ["status bar", "[data-testid='session-status-label']"],
    ["transcript", "[data-testid='transcript']"],
    ["composer", "[data-testid='composer-input']"],
  ] as const) {
    const rect = await bandRect(page, selector);
    expect(rect, `${label} mounted`).not.toBeNull();
    expect(rect!.height, `${label} height`).toBeGreaterThan(0);
    expect(rect!.width, `${label} width`).toBeGreaterThan(0);
    expect(rect!.top, `${label} top within band`).toBeGreaterThanOrEqual(rect!.bandTop - 1);
    expect(rect!.bottom, `${label} bottom above keyboard`).toBeLessThanOrEqual(rect!.bandBottom + 1);
    if (selector.includes("transcript")) {
      expect(rect!.text, `${label} non-empty`).toBeGreaterThan(0);
    }
  }

  // iOS scrolls the layout viewport during focus; pinning must make the
  // document itself non-scrollable so that scroll cannot empty the screen.
  const scroll = await page.evaluate(() => ({
    x: window.scrollX,
    y: window.scrollY,
    overflow: document.scrollingElement
      ? document.scrollingElement.scrollHeight - document.scrollingElement.clientHeight
      : 0,
  }));
  expect(scroll.y).toBe(0);
  expect(scroll.overflow).toBeLessThanOrEqual(1);
  testInfo.attach("soft-keyboard-geometry", {
    body: JSON.stringify({ visualViewport: vv, scroll }, null, 2),
    contentType: "application/json",
  });
});

test("(b) terminal renders rows in a non-zero-height container on WebKit", async ({
  page,
}, testInfo) => {
  if (!(await fakeHostId(page))) test.skip(true, "fake Node not registered");
  const id = await createTerminal(page);
  await page.goto(`/s/${id}/tty`);
  const lab = page.locator("[data-tty-lab]");
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-view", "tty");
  await expect(lab).toHaveAttribute("data-tty-ready", "1", { timeout: 30_000 });

  const before = await bandRect(page, '[aria-label="终端画面"]');
  expect(before, "terminal viewport mounted").not.toBeNull();
  expect(before!.height).toBeGreaterThan(0);
  expect(before!.width).toBeGreaterThan(0);

  const renderer = await lab.getAttribute("data-tty-renderer");
  expect(["webgl", "canvas", "dom"]).toContain(renderer);
  // Surface the effective engine in the run log: headless WebKit usually
  // reports webgl; the coordinator's device run records what iOS Safari picks.
  console.log(`[m-realdevice] effective tty renderer on ${testInfo.project.name}: ${renderer}`);

  // The grid fitted against the real container, not the default 80x24.
  await expect.poll(async () => Number(await lab.getAttribute("data-tty-rows"))).toBeGreaterThan(3);
  const cols = Number(await lab.getAttribute("data-tty-cols"));
  expect(cols).toBeGreaterThan(20);

  // A painted surface exists: canvas rows for webgl/canvas, DOM rows for the
  // fallback renderer — at least one non-empty row subtree.
  const painted = await page.evaluate(() => {
    const screen = document.querySelector(".xterm-screen");
    if (!screen) return { kind: "none", ok: false };
    const canvas = screen.querySelector("canvas");
    if (canvas && canvas.clientHeight > 0 && canvas.clientWidth > 0) {
      return { kind: `canvas ${canvas.width}x${canvas.height}`, ok: true };
    }
    const rows = screen.querySelectorAll(".xterm-rows > div");
    if (rows.length > 0) return { kind: `dom-rows ${rows.length}`, ok: true };
    return { kind: "empty", ok: false };
  });
  expect(painted.ok, `terminal painted something (${painted.kind})`).toBe(true);
  testInfo.attach("webkit-terminal-renderer", {
    body: JSON.stringify({ renderer, painted, cols, viewport: before }, null, 2),
    contentType: "application/json",
  });
  await shot(page, "m-realdevice-2-terminal-390.png");

  // Keyboard up: the terminal keeps its band and refits to the shorter box
  // instead of collapsing to zero behind the keyboard.
  await raiseKeyboard(page);
  const after = await waitForInBand(page, '[aria-label="终端画面"]', 120);
  expect(after, "terminal viewport after keyboard").not.toBeNull();
  expect(after!.height, `terminal height after keyboard: ${JSON.stringify(after)}`).toBeGreaterThan(100);
  await expect.poll(async () => Number(await lab.getAttribute("data-tty-rows"))).toBeGreaterThan(3);
  await shot(page, "m-realdevice-3-terminal-keyboard-390.png", true);
});

test("(c) model/effort observations render as change records, never as 未识别事件", async ({
  page,
}) => {
  if (!(await fakeHostId(page))) test.skip(true, "fake Node not registered");
  const id = await createClaudeSession(page, "m realdevice observations");
  await clearApprovals(page, id);
  await page.goto(`/s/${id}/structured`);
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-view", "structured");
  await expect(page.getByTestId("transcript")).not.toBeEmpty({ timeout: 20_000 });

  // effort: the same instance.configure path the effort slider uses; the fake
  // Node answers with the effective level as an `effort` observation.
  await postCommand(page, id, "instance.configure", { effort: { name: "xhigh", index: 3 } });
  const effortRow = page.getByTestId("observed-change-row").filter({ hasText: "档位" }).first();
  try {
    await expect(effortRow).toBeVisible({ timeout: 20_000 });
  } catch {
    await expectObservationOrSkip(page, id, "effort");
    // Journal carried it but the UI did not render the row: a real failure.
    throw new Error("effort observation in journal but no change record rendered");
  }
  await expect(effortRow).toContainText("xhigh");

  // model: wait for the launch turn to settle to idle so the send is not
  // queued, then the /model: sentinel emits a `model` observation.
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-status", "idle", {
    timeout: 20_000,
  });
  await postCommand(page, id, "instance.send", { prompt: "/model:e2e/fast" });
  const modelRow = page.getByTestId("observed-change-row").filter({ hasText: "模型" }).last();
  try {
    await expect(modelRow).toBeVisible({ timeout: 20_000 });
  } catch {
    await expectObservationOrSkip(page, id, "model");
    throw new Error("model observation in journal but no change record rendered");
  }
  await expect(modelRow).toContainText("e2e/fast");

  // The whole transcript carries no unrecognized-event wording for the two
  // known kinds.
  await expect(page.getByTestId("transcript")).not.toContainText("未识别事件");
  await shot(page, "m-realdevice-4-observations-390.png");

  // Provenance honesty: the raw payload is still one disclosure away.
  await modelRow.locator("summary").click();
  await expect(page.getByTestId("observed-change-json").last()).toContainText("e2e/fast");
});
