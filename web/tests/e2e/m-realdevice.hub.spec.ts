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
  // Evidence stays at the 390px phone width: clip from the centre on the
  // 393px device, and use the whole width on narrower devices. Band shots clip
  // vertically to the visualViewport band; the off-band layout strip is not
  // part of what the phone displays.
  const clip = await page.evaluate((bandOnly) => {
    const x = Math.max(0, Math.round((window.innerWidth - 390) / 2));
    return {
      x,
      y: bandOnly ? window.visualViewport.offsetTop : 0,
      width: Math.min(390, window.innerWidth),
      height: bandOnly ? window.visualViewport.height : window.innerHeight,
    };
  }, clipToBand);
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

/**
 * The exact soft-keyboard sequence the coordinator's verifier uses on a real
 * iPhone: the LAYOUT viewport keeps its size (innerHeight unchanged), the
 * visual viewport keeps its width with bottom pinned (height shrinks,
 * offsetTop = keyboard height), iOS scrolls the layout viewport to reveal the
 * focused input, then the three listeners fire in this order.
 */
async function raiseKeyboardIosExact(page: Page, kb: number) {
  await page.evaluate((KB) => {
    const vv = window.visualViewport;
    const h = window.innerHeight - KB;
    Object.defineProperty(vv, "height", { configurable: true, get: () => h });
    Object.defineProperty(vv, "offsetTop", { configurable: true, get: () => KB });
    window.scrollTo(0, KB);
    vv.dispatchEvent(new Event("resize"));
    vv.dispatchEvent(new Event("scroll"));
    window.dispatchEvent(new Event("resize"));
  }, kb);
  await page.waitForTimeout(300);
}

async function bandRect(page: Page, selector: string) {  return page.evaluate((sel) => {
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

test.describe("(d) full chrome combo: keyboard up keeps the composer and a >=40% transcript", () => {
  // The owner's real session, reproduced by the fake-Node `mfix-chrome-combo`
  // sentinel: install banner still up, instance exited-but-resumable, run
  // details present, a running AskUserQuestion tool with a stalled hook tier
  // and the screen spinner keeping the strip live.
  for (const [label, width, height, kb] of [
    ["iPhone 15", 393, 659, 336],
    ["iPhone SE", 375, 667, 260],
  ] as const) {
    test(`${label} ${width}x${height} with a ${kb}px keyboard`, async ({ page }) => {
      if (!(await fakeHostId(page))) test.skip(true, "fake Node not registered");
      await page.setViewportSize({ width, height });
      const id = await createClaudeSession(page, "mfix-chrome-combo");
      await clearApprovals(page, id);
      await page.goto(`/s/${id}/structured`);
      const sessionPage = page.getByTestId("session-page");
      await expect(sessionPage).toHaveAttribute("data-view", "structured");

      // Trigger absence → the Node answered like an ordinary echo session.
      const comboReady = await page
        .getByTestId("resume-row")
        .waitFor({ state: "visible", timeout: 12_000 })
        .then(() => true)
        .catch(() => false);
      if (!comboReady) test.skip(true, "fake Node lacks the mfix-chrome-combo sentinel");

      // The chrome combination the owner saw.
      await expect(sessionPage).toHaveAttribute("data-status", "exited");
      await expect(page.getByTestId("resume-row")).toBeVisible();
      await expect(page.getByTestId("run-details")).toBeVisible();
      const strip = page.getByTestId("live-status-strip");
      await expect(strip).toBeVisible();
      await expect(strip).toHaveAttribute("data-phase", "tool-started");
      // The backdated hook record is past the 3x-cadence stall budget.
      await expect(page.getByTestId("live-health-hook")).toHaveAttribute("data-reason", "stalled");
      // The iOS install offer is the banner the owner had not dismissed. WebKit
      // iPhone surfaces beforeinstallprompt-style criteria; headless Chromium
      // does not offer, so treat it as a precondition, not a requirement.
      const installBar = page.getByTestId("install-bar");
      const installVisible = await installBar.isVisible().catch(() => false);
      // The running tool content is the latest transcript content.
      await expect(page.getByTestId("transcript")).toContainText("AskUserQuestion");

      // Focus first, then raise the keyboard exactly the way iOS (and the
      // coordinator's verifier) does — layout viewport unchanged. The
      // composer is disabled in the exited state (as in the owner's session)
      // but still visible and tappable on the phone; force the focus.
      await page.getByTestId("composer-input").click({ force: true });
      if (evidence) {
        // Evidence on the owner's iPhone 15 only; the SE case shares
        // the same layout path at 375px.
        if (width === 393) await shot(page, "m-realdevice-5-chrome-keyboard-down-390.png");
      }
      await raiseKeyboardIosExact(page, kb);

      const band = await page.evaluate(() => ({
        top: Math.round(window.visualViewport.offsetTop),
        bottom: Math.round(window.visualViewport.offsetTop + window.visualViewport.height),
        height: Math.round(window.visualViewport.height),
      }));
      expect(band.height).toBe(height - kb);
      await expect(page.locator("html")).toHaveAttribute("data-keyboard", "1");

      // Non-essential chrome is collapsed.
      for (const gone of [
        "install-bar",
        "resume-row",
        "run-details",
        "annotation-dock",
        "task-track",
        "session-notifications",
      ]) {
        const box = await page.getByTestId(gone).count();
        if (box > 0) {
          // Mounted but display:none is the contract.
          await expect(page.getByTestId(gone)).toBeHidden();
        }
      }
      // The strip is a single line.
      await expect(strip).toBeVisible();
      const stripBox = await strip.boundingBox();
      expect(stripBox).toBeTruthy();
      expect(stripBox!.height).toBeLessThanOrEqual(34);

      // (1) Composer — textarea AND its send control — entirely in band.
      // One in-page measurement pass: boundingBox() can transiently answer
      // null mid React re-render even while the element is laid out.
      const boxes = await page.evaluate(() => {
        const rect = (el: Element | null) => {
          if (!el) return null;
          const r = el.getBoundingClientRect();
          return { top: Math.round(r.top), bottom: Math.round(r.bottom), height: Math.round(r.height) };
        };
        const form = document.querySelector("[data-testid='composer']");
        const send = [...(form?.querySelectorAll("button") ?? [])].find((b) =>
          (b.textContent ?? "").includes("发送"),
        );
        return {
          composer: rect(form),
          input: rect(document.querySelector("[data-testid='composer-input']")),
          send: rect(send ?? null),
        };
      });
      for (const [name, box] of Object.entries(boxes) as [string, typeof boxes.composer][]) {
        expect(box, `${name} mounted`).not.toBeNull();
        expect(box!.top, `${name} top`).toBeGreaterThanOrEqual(band.top - 1);
        expect(box!.bottom, `${name} bottom`).toBeLessThanOrEqual(band.bottom + 1);
      }

      // (2) Transcript keeps >= 40% of the band and shows the latest content.
      const tBox = await bandRect(page, "[data-testid='transcript']");
      expect(tBox, "transcript mounted").not.toBeNull();
      expect(tBox!.top).toBeGreaterThanOrEqual(band.top - 1);
      expect(tBox!.bottom).toBeLessThanOrEqual(band.bottom + 1);
      expect(tBox!.height, `transcript ${tBox!.height} vs band ${band.height}`).toBeGreaterThanOrEqual(
        band.height * 0.4 - 1,
      );

      // Pinned to the bottom and painting content there = latest message
      // visible, not a blank flex remainder.
      const painted = await page.evaluate(() => {
        const root = document.querySelector<HTMLElement>("[data-testid='transcript']");
        if (!root) return { pinned: false, contentAtBottom: false };
        const pinned = root.scrollTop + root.clientHeight >= root.scrollHeight - 4;
        const x = Math.round(root.getBoundingClientRect().left + root.clientWidth / 2);
        const y = Math.round(root.getBoundingClientRect().bottom - 12);
        const hit = document.elementFromPoint(x, y)?.closest("[data-testid='transcript']");
        return { pinned, contentAtBottom: hit === root || root.contains(hit) };
      });
      expect(painted.pinned, "transcript pinned to latest content").toBe(true);
      expect(painted.contentAtBottom, "transcript paints content at its bottom edge").toBe(true);

      if (evidence) {
        if (width === 393) await shot(page, "m-realdevice-6-chrome-keyboard-up-390.png", true);
        // Mechanical BEFORE: the collapse is driven entirely by the attribute;
        // remove it (geometry unchanged) to capture the pre-fix layout.
        await page.evaluate(() => document.documentElement.removeAttribute("data-keyboard"));
        await page.waitForTimeout(200);
        if (width === 393) await shot(page, "m-realdevice-7-chrome-keyboard-up-nocollapse-390.png", true);
        await page.evaluate(() => {
          document.documentElement.dataset.keyboard = "1";
        });
      }

      // Keyboard closes: every piece of chrome comes back.
      await page.evaluate(() => {
        const vv = window.visualViewport;
        Object.defineProperty(vv, "height", { configurable: true, get: () => window.innerHeight });
        Object.defineProperty(vv, "offsetTop", { configurable: true, get: () => 0 });
        vv.dispatchEvent(new Event("resize"));
        window.dispatchEvent(new Event("resize"));
      });
      await expect(page.getByTestId("run-details")).toBeVisible();
      if (installVisible) await expect(page.getByTestId("install-bar")).toBeVisible();
      await expect(page.getByTestId("resume-row")).toBeVisible();
    });
  }
});
