import { expect, test, devices, webkit, type Browser, type BrowserContext, type Page } from "@playwright/test";
import { execFile, spawn, type ChildProcess } from "node:child_process";
import {
  access,
  chmod,
  copyFile,
  mkdir,
  mkdtemp,
  open,
  readFile,
  realpath,
  rm,
  writeFile,
} from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { setMode } from "./appearanceHelper";
import { login } from "./hub-auth";

/**
 * UO-6b session-page leaf components — owner-demo regressions, rendered by
 * Remuda only (no mocked UI):
 *
 *  1. An EXITED session settles the live strip: the 「文本生成中」 row stops,
 *     its clock freezes on the last turn, and no stall note or interrupt
 *     button survives. Proven two ways: the shared fake Node's
 *     mfix-chrome-combo exit fixture (the exact shape m-realdevice pinned the
 *     bug with) and a real native Node + fake harness + instance.close.
 *  2. Notification rows collapse to one quiet 24px row with a +N expander and
 *     clear on session end; the toast is top-anchored and can never cover the
 *     composer on a 390px phone.
 *  3. The thought disclosure shows exactly ONE marker.
 *
 * Screenshots are committed only under REMUDA_EVIDENCE=1 (390 and 1440, dark
 * and light). The WebKit/iPhone real-condition test connects to a
 * `playwright run-server` over PW_TEST_CONNECT_WS_ENDPOINT (the jammy
 * container browser server), or launches the bundled WebKit when installed;
 * it skips on hosts with neither.
 */
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..");
const target = path.resolve(root, process.env.CARGO_TARGET_DIR ?? "target");
const remuda = process.env.HUB_E2E_REMUDA_BIN ?? path.join(target, "debug/remuda");
const harness = process.env.HUB_E2E_FAKE_HARNESS_BIN ?? path.join(target, "debug/fake-harness");
const nativeNode = process.env.HUB_E2E_NATIVE_NODE_BIN ?? path.join(target, "debug/examples/native_hub_e2e");

const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence
  ? path.join(root, "docs/design/evidence")
  : path.join(root, "web/test-results/evidence");

async function shot(page: Page, name: string): Promise<void> {
  if (!evidence) return;
  await mkdir(shotDir, { recursive: true });
  await page.evaluate(() => document.fonts.ready);
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

function quote(value: string): string {
  return `'${value.replaceAll("'", "'\\''")}'`;
}

async function command(page: Page, instanceId: string, operation: string, payload = {}) {
  const response = await page.request.post(`/v1/instances/${instanceId}/commands`, {
    headers: { Origin: new URL(page.url()).origin },
    data: { operation, payload },
  });
  expect(response.ok()).toBe(true);
}

async function rawKeys(page: Page, instanceId: string, text: string) {
  await command(page, instanceId, "tty.write", {
    dataBase64: Buffer.from(text).toString("base64"),
    source: "ui",
  });
}

type NativeEvent = { event: string };

async function waitForEventCount(file: string, name: string, count: number, timeoutMs: number): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const text = await readFile(file, "utf8").catch(() => "");
    const seen = text
      .split("\n")
      .filter(Boolean)
      .map((line) => JSON.parse(line) as NativeEvent)
      .filter((row) => row.event === name).length;
    if (seen >= count) return;
    if (Date.now() > deadline) throw new Error(`ground truth ${name} x${count} not seen within ${timeoutMs}ms`);
    await new Promise((resolve) => setTimeout(resolve, 200));
  }
}

async function stopNode(node: ChildProcess) {
  if (!node.pid || node.exitCode !== null || node.signalCode !== null) return;
  const exited = new Promise<void>((resolve) => node.once("exit", () => resolve()));
  node.kill("SIGTERM");
  const timer = setTimeout(() => node.kill("SIGKILL"), 10_000);
  await exited;
  clearTimeout(timer);
}

type Harness = {
  dir: string;
  dataHandle?: { fd: number };
  node: ChildProcess;
  instanceId: string;
  eventsFile: string;
  settingsFile: string;
  scriptFile: string;
  bin: string;
  workspace: string;
  claudeHome: string;
};

test.beforeAll(async ({}, testInfo) => {
  testInfo.setTimeout(600_000);
  const args = ["build", "--locked"];
  if (!process.env.HUB_E2E_REMUDA_BIN) args.push("-p", "remuda", "--bin", "remuda");
  if (!process.env.HUB_E2E_FAKE_HARNESS_BIN) args.push("-p", "remuda-testing", "--bin", "fake-harness");
  if (!process.env.HUB_E2E_NATIVE_NODE_BIN) args.push("-p", "remuda-node", "--example", "native_hub_e2e");
  if (args.length > 2) {
    await promisify(execFile)("cargo", args, {
      cwd: root,
      env: process.env,
      timeout: 570_000,
      maxBuffer: 8 * 1024 * 1024,
    });
  }
  await Promise.all([access(remuda), access(harness), access(nativeNode)]);
});

test.describe.configure({ mode: "serial" });

test("the live strip settles on the fake-Node exited-combo fixture (m-realdevice shape)", async ({ page }) => {
  await login(page, "e2e-uo6b");
  await page.goto("/sessions/new");
  const host = await page
    .getByTestId("new-session-host")
    .locator("option")
    .filter({ hasText: "e2e-fake-node" })
    .getAttribute("value");
  await page.getByTestId("new-session-host").selectOption(host!);
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0);
  await page.getByTestId("new-session-prompt").fill("mfix-chrome-combo");
  await page.getByTestId("new-session-start").click();
  await page.waitForURL(/\/s\//, { timeout: 20_000 });
  const id = new URL(page.url()).pathname.split("/")!.pop()!;

  const strip = page.getByTestId("live-status-strip");
  // The fixture ends with the entity("exited") record: the strip must settle
  // instead of keeping the stalled 工具运行中 row with a growing timer.
  await expect(strip).toHaveAttribute("data-turn", "ended", { timeout: 20_000 });
  await expect(strip).toHaveAttribute("data-phase", "turn-ended");
  await expect(strip).toHaveAttribute("data-settled", "exited");
  // No stall warning, no interrupt, no live token count on the settled row.
  await expect(page.getByTestId("live-health-hook")).toHaveCount(0);
  await expect(page.getByTestId("live-interrupt")).toHaveCount(0);
  await expect(page.getByTestId("live-token-count")).toHaveCount(0);
  const elapsed = page.getByTestId("live-elapsed");
  const first = await elapsed.textContent();
  await page.waitForTimeout(2_200);
  expect(await elapsed.textContent()).toBe(first);
  await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
});

test("UO-6b r2: a HUB-detected Node restart settles the live strip and shows Resume", async ({ page }) => {
  test.slow();
  const pageErrors: string[] = [];
  page.on("pageerror", (error) => pageErrors.push(error.stack ?? error.message));
  await login(page, "e2e-uo6b-epoch");
  await page.goto("/sessions/new");
  const host = await page
    .getByTestId("new-session-host")
    .locator("option")
    .filter({ hasText: "e2e-fake-node" })
    .getAttribute("value");
  await page.getByTestId("new-session-host").selectOption(host!);
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0);
  await page.getByTestId("new-session-prompt").fill("uo6b-epoch-live");
  await page.getByTestId("new-session-start").click();
  await page.waitForURL(/\/s\//, { timeout: 20_000 });
  const id = new URL(page.url()).pathname.split("/")!.pop()!;

  const strip = page.getByTestId("live-status-strip");
  // The fixture keeps a turn genuinely live: hook text-streaming latch plus a
  // fresh screen spinner, and the local clock is growing.
  await expect(strip).not.toHaveAttribute("data-turn", "ended", { timeout: 20_000 });
  const elapsed = page.getByTestId("live-elapsed");
  await expect(elapsed).toBeVisible();
  const before = await elapsed.textContent();
  await page.waitForTimeout(2_200);
  expect(await elapsed.textContent(), "the live clock was not growing").not.toBe(before);

  // Restart the fake Node while the turn is live. Its next hello drops this
  // session from the inventory, so the HUB reconciles the row to exited and
  // appends node_epoch_changed itself — a hub-origin native lifecycle with
  // severity + message and NO status (the payload round 1 threw on).
  await rawKeys(page, id, "TTYNODE_RESTART\r");

  // The strip settles: ended, node-restart attribution, frozen clock, every
  // live affordance gone — no React error from the status-less diagnostic.
  await expect(strip).toHaveAttribute("data-turn", "ended", { timeout: 30_000 });
  await expect(strip).toHaveAttribute("data-phase", "turn-ended");
  await expect(strip).toHaveAttribute("data-settled", "node-restart");
  await expect(page.getByTestId("live-interrupt")).toHaveCount(0);
  const frozen = await elapsed.textContent();
  await page.waitForTimeout(2_200);
  expect(await elapsed.textContent(), "the settled clock kept growing").toBe(frozen);

  // The instance row the Hub reconciled drives the banner + Resume.
  await expect(page.getByTestId("node-restart-banner")).toContainText("Node 重启，会话已结束", {
    timeout: 20_000,
  });
  await expect(page.getByTestId("node-restart-resume")).toBeEnabled();
  expect(pageErrors, `page errors during restart: ${pageErrors.join("\n")}`).toEqual([]);
  await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
});

test("a thought disclosure carries exactly one marker", async ({ page }) => {
  // Compact mode would fold the thought behind a group trigger; turn it off
  // up front so the disclosure renders on its own near the followed bottom.
  await page.addInitScript(() => {
    try {
      localStorage.setItem("runtime.compact", "0");
    } catch {
      /* ignore */
    }
  });
  await login(page, "e2e-uo6b");
  await page.goto("/sessions/new");
  const host = await page
    .getByTestId("new-session-host")
    .locator("option")
    .filter({ hasText: "e2e-fake-node" })
    .getAttribute("value");
  await page.getByTestId("new-session-host").selectOption(host!);
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0);
  // The `live` row-phrase sentinel journals a live workflow run plus one
  // closed assistant turn (a Bash tool and a thought) directly, with no
  // approval round-trip.
  await page.getByTestId("new-session-prompt").fill("workflow card live row-phrase");
  await page.getByTestId("new-session-start").click();
  await page.waitForURL(/\/s\//, { timeout: 20_000 });
  const id = new URL(page.url()).pathname.split("/")!.pop()!;

  // Ground truth first: the thought observation is in the bounded journal.
  await expect
    .poll(
      async () =>
        page.evaluate(async (iid) => {
          const res = await fetch(`/v1/instances/${iid}/journal`, { credentials: "include" });
          const body = (await res.json()) as {
            events?: Array<{ event?: { kind?: string } }>;
          };
          return (body.events ?? []).some((row) => row.event?.kind === "thought");
        }, id),
      { timeout: 20_000, intervals: [300] },
    )
    .toBe(true);

  // Walk the virtualised transcript from bottom to top until the thought
  // details node is drawn.
  const summaries = page.locator("details").filter({ hasText: /thinking/ });
  await expect
    .poll(
      async () =>
        page.evaluate(async () => {
          const scroller = document.querySelector<HTMLElement>("[data-testid='transcript-scroller']");
          if (!scroller) return false;
          for (let pos = scroller.scrollHeight; pos >= 0; pos -= Math.floor(scroller.clientHeight * 0.8)) {
            scroller.scrollTop = pos;
            await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
            if ([...document.querySelectorAll("details")].some((el) => /thinking/.test(el.textContent ?? ""))) {
              return true;
            }
          }
          return false;
        }),
      { timeout: 20_000, intervals: [500] },
    )
    .toBe(true);
  await summaries.first().scrollIntoViewIfNeeded();
  await expect(summaries.first()).toBeVisible();
  for (const [width, height, mode, suffix] of [
    [1440, 900, "dark", "1440-dark"],
    [390, 844, "dark", "390-dark"],
    [1440, 900, "light", "1440-light"],
    [390, 844, "light", "390-light"],
  ] as const) {
    await page.setViewportSize({ width, height });
    await setMode(page, mode);
    await page.emulateMedia({ reducedMotion: "reduce" });
    await page.waitForTimeout(120);
    await shot(page, `uo6b-thinking-marker-${suffix}.png`);
  }
  await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
});

async function bootHarness(
  page: Page,
  tag: string,
  opts: { idleMessage?: string } = {},
): Promise<Harness> {
  const scratch = path.join(root, "target", "hub-e2e");
  await mkdir(scratch, { recursive: true });
  const dir = await realpath(await mkdtemp(path.join(scratch, `uo6b-${tag}-`)));
  const dataDir = path.join(dir, "data");
  await mkdir(dataDir);
  const dataHandle = process.platform === "linux" ? await open(dataDir, "r") : undefined;
  const dataPath = dataHandle ? `/proc/${process.pid}/fd/${dataHandle.fd}` : dataDir;
  const bin = path.join(dir, "bin");
  const workspace = path.join(dir, "workspace");
  const claudeHome = path.join(dir, "claude-home");
  const eventsFile = path.join(dir, "native-events.jsonl");
  const settingsFile = path.join(dir, "user-settings.json");
  const scriptFile = path.join(dir, "scenario.json");
  const tokenFile = path.join(dir, "enroll-token");
  const shell = path.join(bin, "test-shell");
  await Promise.all([mkdir(bin), mkdir(workspace), mkdir(claudeHome)]);
  await copyFile(harness, path.join(bin, "claude"));
  await chmod(path.join(bin, "claude"), 0o700);
  await writeFile(
    shell,
    "#!/bin/sh\nif [ \"${1:-}\" = --version ]; then exec /bin/sh --version; fi\nexec /bin/sh\n",
    { mode: 0o700 },
  );
  await writeFile(settingsFile, JSON.stringify({ env: { UO6B: "retained" } }));
  const turn = (prefix: string, text: string) => ({
    match_prefix: prefix,
    text,
    chunks: 1,
    idle_notification: true,
    ...(opts.idleMessage !== undefined ? { idle_notification_message: opts.idleMessage } : {}),
    stop_reason: "end_turn",
    usage: { input_tokens: 30, output_tokens: 8 },
  });
  await writeFile(
    scriptFile,
    JSON.stringify({
      turns: [turn("NOTESTURN", "first turn done"), turn("NOTESTURN", "second turn done")],
    }),
  );

  const minted = await page.request.post("/v1/hosts/enroll-token", {
    headers: { Origin: new URL(page.url()).origin },
  });
  expect(minted.ok()).toBe(true);
  await writeFile(tokenFile, (await minted.json()).token as string, { mode: 0o600 });
  const hub = new URL(process.env.VITE_HUB_URL ?? `http://${process.env.HUB_E2E_LISTEN ?? "127.0.0.1:58880"}`);
  hub.protocol = hub.protocol === "https:" ? "wss:" : "ws:";
  hub.pathname = "/v1/node";
  const node = spawn(nativeNode, [], {
    cwd: dir,
    stdio: ["ignore", "pipe", "pipe"],
    env: {
      ...process.env,
      REMUDA_DATA_DIR: dataPath,
      HUB_E2E_NODE_HUB_URL: hub.toString(),
      HUB_E2E_NODE_TOKEN_FILE: tokenFile,
      HUB_E2E_NODE_WORKSPACE: workspace,
      HUB_E2E_NODE_DATA_DIR: dataPath,
      HUB_E2E_REMUDA_BIN: remuda,
      REMUDA_PTY_EMULATOR: "1",
      REMUDA_PTY_HOOKS: "1",
      REMUDA_SHIM: "on",
      REMUDA_CLAUDE_BIN: path.join(bin, "claude"),
      REMUDA_CLAUDE_CONFIG_DIR: claudeHome,
      CLAUDE_CONFIG_DIR: claudeHome,
      SHELL: shell,
      PATH: `${bin}:/usr/bin:/bin:/usr/sbin:/sbin`,
      REMUDA_HERDR_ORPHAN_SWEEP: "0",
      RUST_LOG: "warn",
    },
  });

  let hostId = "";
  await expect.poll(async () => {
    if (node.exitCode !== null || node.signalCode !== null) {
      throw new Error(`native node exited early (${node.exitCode ?? node.signalCode})`);
    }
    const response = await page.request.get("/v1/hosts");
    const body = (await response.json()) as { items: { hostId: string; labels?: string[]; online?: boolean }[] };
    hostId = body.items.find((row) => row.labels?.includes("test=promoted-hooks") && row.online)?.hostId ?? "";
    return hostId;
  }, { timeout: 90_000 }).not.toBe("");
  const workspaces = await (await page.request.get(`/v1/hosts/${hostId}/workspaces`)).json();
  const created = await page.request.post("/v1/instances", {
    headers: { Origin: new URL(page.url()).origin },
    data: {
      hostId,
      workspaceId: workspaces.workspaces[0].workspaceId,
      cwd: workspace,
      kind: "terminal",
      driver: "shell-pty",
      name: "uo6b-notes",
      tui: "default",
    },
  });
  const result = await created.json();
  expect(created.ok(), `instance create ${created.status()}: ${JSON.stringify(result)}`).toBe(true);
  const instanceId = result.instance.instanceId ?? result.instance.id;

  return {
    dir,
    dataHandle,
    node,
    instanceId,
    eventsFile,
    settingsFile,
    scriptFile,
    bin,
    workspace,
    claudeHome,
  };
}

async function launchPromoted(page: Page, h: Harness) {
  await page.goto(`/s/${h.instanceId}/tty`);
  const session = page.getByTestId("session-page");
  await expect(session).toHaveAttribute("data-lifecycle", "running", { timeout: 20_000 });
  await rawKeys(
    page,
    h.instanceId,
    `claude --kind claude --settings ${quote(h.settingsFile)} --home ${quote(h.claudeHome)} --script ${quote(h.scriptFile)} --events-out ${quote(h.eventsFile)}\r`,
  );
  await expect(session).toHaveAttribute("data-mode", "promoted", { timeout: 20_000 });
  await page.getByTestId("view-switch-structured").click();
  await expect(page.getByTestId("transcript")).toBeVisible();
}

test("notifications collapse to one quiet row and clear with the session; toast never covers the composer", async ({
  page,
}) => {
  test.setTimeout(240_000);
  await login(page, "e2e-uo6b-notes");
  const h = await bootHarness(page, "notes");
  try {
    await launchPromoted(page, h);

    // Turn 1: Stop then the idle_prompt advisory.
    // Text and CR in one write: a separate CR races the pty echo and loses
    // the last character of the draft (a fake-harness/pty timing quirk).
    await rawKeys(page, h.instanceId, "NOTESTURN one");
    await new Promise((resolve) => setTimeout(resolve, 300));
    await rawKeys(page, h.instanceId, "\r");
    await waitForEventCount(h.eventsFile, "idle_notification", 1, 60_000);
    await expect(page.getByTestId("session-notification")).toHaveCount(1, { timeout: 10_000 });
    // The fresh advisory also toasts once.
    await expect(page.getByTestId("notification-toast")).toBeVisible({ timeout: 10_000 });

    // Turn 2 produces a second advisory: still exactly one visible row, with
    // the +1 inline expander instead of a stack above the composer.
    await rawKeys(page, h.instanceId, "NOTESTURN two");
    await new Promise((resolve) => setTimeout(resolve, 300));
    await rawKeys(page, h.instanceId, "\r");
    await waitForEventCount(h.eventsFile, "idle_notification", 2, 60_000);
    await expect(page.getByTestId("notification-older")).toHaveText("+1", { timeout: 10_000 });
    expect(await page.getByTestId("session-notification").count()).toBe(1);

    // The collapsed dock is exactly ONE 24px row: the +N expander sits inside
    // the newest row rather than forming a second row above the composer.
    expect(await page.getByTestId("session-notification-more-row").count()).toBe(0);
    const panelBox = await page.getByTestId("session-notifications").boundingBox();
    expect(panelBox, "notification panel must have geometry").toBeTruthy();
    expect(panelBox!.height, "collapsed notification area must be one 24px row").toBe(24);

    // Phone width: the top-anchored toast sits entirely ABOVE the composer.
    await page.setViewportSize({ width: 390, height: 844 });
    await expect.poll(async () => page.getByTestId("notification-toast").isVisible().catch(() => false)).toBe(true);
    const geometry = await page.evaluate(() => {
      const rect = (sel: string) => {
        const el = document.querySelector<HTMLElement>(sel);
        if (!el) return null;
        const r = el.getBoundingClientRect();
        return { top: r.top, bottom: r.bottom, height: r.height };
      };
      return { toast: rect("[data-testid='notification-toast']"), composer: rect("[data-testid='composer']") };
    });
    expect(geometry.toast).toBeTruthy();
    expect(geometry.composer).toBeTruthy();
    expect(geometry.toast!.bottom).toBeLessThanOrEqual(geometry.composer!.top + 1);
    await shot(page, "uo6b-toast-above-composer-390-dark.png");

    // Ending the session collapses notifications: one muted quiet row, no
    // dialog affordance, toast dismissed.
    await command(page, h.instanceId, "instance.close");
    await expect(page.getByTestId("session-notifications")).toHaveAttribute("data-settled", "1", {
      timeout: 10_000,
    });
    await expect(page.getByTestId("notification-toast")).toHaveCount(0);
    expect(await page.getByTestId("session-notification").count()).toBe(1);
    expect(
      await page.getByTestId("session-notification").first().getAttribute("data-settled"),
    ).toBe("1");

    for (const [width, height, mode, suffix] of [
      [1440, 900, "dark", "1440-dark"],
      [390, 844, "dark", "390-dark"],
      [1440, 900, "light", "1440-light"],
      [390, 844, "light", "390-light"],
    ] as const) {
      await page.setViewportSize({ width, height });
      await setMode(page, mode);
      await page.emulateMedia({ reducedMotion: "reduce" });
      await page.waitForTimeout(120);
      const overflow = await page.evaluate(
        () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
      );
      expect(overflow, `horizontal overflow at ${suffix}`).toBeLessThanOrEqual(1);
      await shot(page, `uo6b-settled-strip-${suffix}.png`);
    }
  } finally {
    await command(page, h.instanceId, "instance.close").catch(() => undefined);
    await stopNode(h.node);
    h.dataHandle?.close();
    await rm(h.dir, { recursive: true, force: true }).catch(() => undefined);
  }
});

/**
 * Real-condition phone check: WebKit with an iPhone device profile (390px).
 * Prefers a `playwright run-server` (the jammy container on focal hosts) over
 * PW_TEST_CONNECT_WS_ENDPOINT; otherwise launches the bundled Playwright
 * WebKit when its browser is installed, and skips on hosts with neither.
 */
test("iPhone/WebKit: a 2,000-character CJK toast stays clamped above the composer", async () => {
  const endpoint = process.env.PW_TEST_CONNECT_WS_ENDPOINT;
  let browser: Browser;
  if (endpoint) {
    // Playwright's run-server multiplexes browser types on one endpoint; the
    // WebKit client selects its own engine through the connect handshake.
    browser = await webkit.connect(endpoint);
  } else {
    let bundled = true;
    await access(webkit.executablePath()).catch(() => {
      bundled = false;
    });
    test.skip(!bundled, "needs WebKit (PW_TEST_CONNECT_WS_ENDPOINT or a bundled webkit build)");
    browser = await webkit.launch();
  }
  // The custom WebKit context must follow the RUNNING origin, never a
  // hard-coded port: the landing gate runs the hub on non-default ports, and
  // process.env.HUB_E2E_BASE_URL is unset there. The project use is set from
  // the configured origin by playwright.hub.config.ts.
  const baseURL = test.info().project.use.baseURL;
  if (!baseURL) {
    throw new Error("iPhone/WebKit case needs a configured project baseURL");
  }
  const context: BrowserContext = await browser.newContext({
    ...devices["iPhone 13"],
    baseURL,
  });
  const page = await context.newPage();
  // 2,000 CJK characters: before the clamp this toast grew dozens of lines
  // straight down over the composer on a 390px phone.
  const cjkMessage = "通知：等待你的输入，这是一条很长的中文消息".repeat(100).slice(0, 2000);
  expect(cjkMessage).toHaveLength(2000);
  await login(page, "e2e-uo6b-iphone");
  const h = await bootHarness(page, "iphone", { idleMessage: cjkMessage });
  try {
    await launchPromoted(page, h);
    await rawKeys(page, h.instanceId, "NOTESTURN phone");
    await new Promise((resolve) => setTimeout(resolve, 300));
    await rawKeys(page, h.instanceId, "\r");
    await waitForEventCount(h.eventsFile, "idle_notification", 1, 60_000);
    const toast = page.getByTestId("notification-toast");
    await expect(toast).toBeVisible({ timeout: 10_000 });
    const geometry = await page.evaluate(() => {
      const rect = (el: Element) => {
        const r = el.getBoundingClientRect();
        return { top: r.top, bottom: r.bottom, height: r.height };
      };
      const toastEl = document.querySelector<HTMLElement>("[data-testid='notification-toast']");
      const textEl = toastEl?.querySelector<HTMLElement>("span");
      const composerEl = document.querySelector<HTMLElement>("[data-testid='composer']");
      if (!toastEl || !textEl || !composerEl) return null;
      const lineHeight = parseFloat(getComputedStyle(textEl).lineHeight);
      return {
        toast: rect(toastEl),
        composer: rect(composerEl),
        lineHeight,
        textClient: textEl.clientHeight,
        // A clamped element keeps its full content height in scrollHeight.
        textScroll: textEl.scrollHeight,
        viewport: window.innerWidth,
      };
    });
    expect(geometry).toBeTruthy();
    expect(geometry!.viewport).toBe(390);
    expect(geometry!.lineHeight).toBeGreaterThan(0);
    // The message is long enough that clamping actually bites (3-line cap)…
    expect(geometry!.textScroll).toBeGreaterThan(geometry!.textClient);
    // …the text occupies at most three lines and the toast stays bounded…
    expect(geometry!.textClient).toBeLessThanOrEqual(3 * geometry!.lineHeight + 1);
    expect(geometry!.toast.height).toBeLessThanOrEqual(3 * geometry!.lineHeight + 24);
    // …so it sits entirely above the composer instead of covering it.
    expect(geometry!.toast.bottom).toBeLessThanOrEqual(geometry!.composer.top + 1);
    await shot(page, "uo6b-toast-cjk-iphone-webkit-390-dark.png");
  } finally {
    await command(page, h.instanceId, "instance.close").catch(() => undefined);
    await stopNode(h.node);
    h.dataHandle?.close();
    await rm(h.dir, { recursive: true, force: true }).catch(() => undefined);
    await context.close();
    await browser.close();
  }
});
