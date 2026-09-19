import { expect, test, type Page } from "@playwright/test";
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
  stat,
  writeFile,
} from "node:fs/promises";
import { hostname } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { login } from "./hub-auth";

/**
 * c-grok-e2e — the grok structural chain, end to end. A real native Node
 * drives the fake harness running **as grok on PATH** through a real PTY (the
 * D-025 promoted path: a `grok` process detected in a terminal session),
 * with a per-test shadow GROK_HOME. The production grok file adapter tails
 * that home's ACP `updates.jsonl` / `events.jsonl`, projects the file-tier
 * turn.live phases and the ask_user_question interaction, journals them
 * through the Hub, and the browser structured view renders:
 *
 *   1. the named grok tool card — stable native name
 *      `run_terminal_command` plus the ACP human title, a Running dot and
 *      the live stdout partials tailed from `terminal/<callId>.log`
 *      (c-grok-stdout) while the tool runs; the Final text replaces the
 *      partials exactly once;
 *   2. the native `thinking` phase in the live strip and the thought block
 *      in the transcript while it streams;
 *   3. the strip's decided-by chip reading `file` (data-decided-by=file) at
 *      turn end — no hook channel exists in this run;
 *   4. the ask_user_question card with the fixture's Alpha/Beta options,
 *      marked answerable=false / carrier=native-tty at the interaction
 *      entity level, with the approvals queue rendering the native-tty
 *      explanation and no inline answer form.
 *
 * Scripts are the committed fake-harness scenarios
 * (crates/remuda-testing/fixtures/fake-harness/scenarios/grok-tools.json and
 * grok-question.json), loaded at test time and copied into the temp dir with
 * ONLY duration_ms overridden (the committed 900 ms / 20 ms values cannot
 * hold an observable running/pending window) — every payload, name and
 * prefix comes from the fixture.
 *
 * Ground truth for timing is the harness `--events-out` JSONL
 * (turn_start/turn_end wallMs), never the strip's own clock. Nothing here
 * injects journal rows: every assertion is a projection of a real
 * fake-harness session.
 */
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..");
const target = path.resolve(root, process.env.CARGO_TARGET_DIR ?? "target");
const remuda = process.env.HUB_E2E_REMUDA_BIN ?? path.join(target, "debug/remuda");
const harness = process.env.HUB_E2E_FAKE_HARNESS_BIN ?? path.join(target, "debug/fake-harness");
const nativeNode = process.env.HUB_E2E_NATIVE_NODE_BIN ?? path.join(target, "debug/examples/native_hub_e2e");
const fixtureDir = path.join(
  root,
  "crates/remuda-testing/fixtures/fake-harness/scenarios",
);

/** Committed evidence only under REMUDA_EVIDENCE=1; default runs stay git-clean. */
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence
  ? path.join(root, "docs/design/evidence")
  : path.join(root, "web/test-results/evidence");

/** The shell call stays running long enough to race all the Running assertions. */
const SHELL_TOOL_MS = 30_000;
/** The question stays pending through both pages and the evidence captures. */
const QUESTION_TOOL_MS = 30_000;

async function shot(page: Page, name: string, redact: string[] = []) {
  if (!evidence) return;
  await mkdir(shotDir, { recursive: true });
  // Evidence images must carry no temp/home paths: scrub any rendered text
  // node containing them immediately before the capture.
  await page.evaluate((needles) => {
    const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
    const nodes: Text[] = [];
    for (;;) {
      const node = walker.nextNode();
      if (!node) break;
      nodes.push(node as Text);
    }
    for (const node of nodes) {
      let text = node.nodeValue ?? "";
      for (const needle of needles) {
        if (needle) text = text.split(needle).join("$TEST_DIR");
      }
      if (text !== node.nodeValue) node.nodeValue = text;
    }
  }, redact);
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

/**
 * Load a committed scenario fixture and write a patched copy. Only two
 * things are patched, both needed to observe settled state over the web:
 *
 *  - `duration_ms` for the named tools, so the live Running / pending window
 *    is observable (the committed 900 ms / 20 ms values finish too fast);
 *  - `quit_after_turns`, set to keep the fake harness alive after the turn.
 *    The committed value (1) makes the process exit at turn end, which tears
 *    down promotion and reverts the session to a plain terminal screen — so
 *    no post-turn structured card could ever be observed.
 *
 * Every payload, tool name and match prefix stays byte-for-byte from the
 * fixture. Every requested duration override must match a tool (a rename
 * must break loudly), and the returned facts are pinned against the fixture.
 */
async function patchedScenario(
  sourceName: string,
  destPath: string,
  durations: Record<string, number>,
): Promise<{ path: string; prefix: string; thinking: string; stdoutLines: string[] }> {
  const source = path.join(fixtureDir, sourceName);
  const scenario = JSON.parse(await readFile(source, "utf8")) as {
    quit_after_turns?: number;
    turns: Array<{
      match_prefix?: string;
      thinking?: string;
      tools: Array<{ name: string; duration_ms?: number; result?: string }>;
    }>;
  };
  // Keep the harness alive after the turn so promotion (and thus the
  // structured file-tier view) survives to the settled assertions; the test
  // node is stopped in teardown.
  scenario.quit_after_turns = 0;
  const matched = new Set<string>();
  for (const turn of scenario.turns) {
    for (const tool of turn.tools) {
      if (Object.hasOwn(durations, tool.name)) {
        tool.duration_ms = durations[tool.name];
        matched.add(tool.name);
      }
    }
  }
  for (const name of Object.keys(durations)) {
    expect(matched.has(name), `fixture ${sourceName} has no tool named ${name} to slow down`).toBe(true);
  }
  await writeFile(destPath, JSON.stringify(scenario));
  const first = scenario.turns[0]!;
  expect(first.thinking, `fixture ${sourceName} must stream a thought block`).toBeTruthy();
  const bash = first.tools.find((tool) => tool.name === "Bash");
  const stdoutLines = (bash?.result ?? "").split("\n").filter((line) => line.length > 0);
  return {
    path: destPath,
    prefix: first.match_prefix ?? "",
    thinking: first.thinking ?? "",
    stdoutLines,
  };
}

function quote(value: string): string {
  return `'${value.replaceAll("'", "'\\''")}'`;
}

async function command(page: Page, instanceId: string, operation: string, payload = {}) {
  // Bound the POST: under host load the node can stall on acking, and an
  // unbounded wait would consume the whole test budget. The hub queues and
  // resends commands, so a timed-out write is retried by the caller below.
  const response = await page.request.post(`/v1/instances/${instanceId}/commands`, {
    headers: { Origin: new URL(page.url()).origin },
    data: { operation, payload },
    timeout: 15_000,
  });
  expect(response.ok()).toBe(true);
}

async function rawKeys(page: Page, instanceId: string, text: string) {
  await command(page, instanceId, "tty.write", {
    dataBase64: Buffer.from(text).toString("base64"),
    source: "ui",
  });
}

type NativeEvent = { event: string; wallMs?: number };

async function nativeEvents(file: string): Promise<NativeEvent[]> {
  const text = await readFile(file, "utf8").catch(() => "");
  return text
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line) as NativeEvent);
}

async function waitForNthEvent(file: string, name: string, n: number, timeoutMs: number): Promise<NativeEvent> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const found = (await nativeEvents(file)).filter((row) => row.event === name);
    if (found.length > n) return found[n]!;
    if (Date.now() > deadline) throw new Error(`ground-truth ${name} #${n + 1} not seen within ${timeoutMs}ms`);
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

let binariesReady = false;

test.beforeAll(async ({}, testInfo) => {
  testInfo.setTimeout(600_000);
  const args = ["build", "--locked"];
  if (!process.env.HUB_E2E_REMUDA_BIN) args.push("-p", "remuda", "--bin", "remuda");
  if (!process.env.HUB_E2E_FAKE_HARNESS_BIN) args.push("-p", "remuda-testing", "--bin", "fake-harness");
  if (!process.env.HUB_E2E_NATIVE_NODE_BIN) args.push("-p", "remuda-node", "--example", "native_hub_e2e");
  try {
    if (args.length > 2) {
      await promisify(execFile)("cargo", args, {
        cwd: root,
        env: process.env,
        timeout: 570_000,
        maxBuffer: 8 * 1024 * 1024,
      });
    }
    await Promise.all([access(remuda), access(harness), access(nativeNode)]);
    binariesReady = true;
  } catch (error) {
    // Skip cleanly when the build inputs are unavailable (offline checkout,
    // no toolchain), the same contract the other hub specs carry.
    console.warn(`grok-structural hub e2e skipped: binaries unavailable: ${String(error)}`);
  }
});

/** One scratch tree plus an enrolled native node. */
type Harness = {
  dir: string;
  workspace: string;
  grokHome: string;
  eventsFile: string;
  node: ChildProcess;
  hostId: string;
  enrollToken: string;
  /** Mutable sink the node's stdout/stderr are appended to for the whole run. */
  log: { text: string };
  redact: string[];
  dataFd: Awaited<ReturnType<typeof open>> | undefined;
};

async function startHarness(page: Page, testName: string): Promise<Harness> {
  const scratch = path.join(root, "target", "hub-e2e");
  await mkdir(scratch, { recursive: true });
  const dir = await realpath(await mkdtemp(path.join(scratch, "gk-")));
  const dataDir = path.join(dir, "data");
  await mkdir(dataDir);
  // Existence/permission check before opening the /proc/self/fd alias.
  await stat(dataDir);
  // Linux /proc/self/fd alias keeps the data directory resolvable (see
  // promoted-claude.hub.spec.ts for the sockaddr-length rationale).
  const dataFd = process.platform === "linux" ? await open(dataDir, "r") : undefined;
  const dataPath = dataFd ? `/proc/${process.pid}/fd/${dataFd.fd}` : dataDir;
  const bin = path.join(dir, "bin");
  const workspace = path.join(dir, "workspace");
  const grokHome = path.join(dir, "grok-home");
  const claudeHome = path.join(dir, "claude-home");
  const eventsFile = path.join(dir, "native-events.jsonl");
  const tokenFile = path.join(dir, "enroll-token");
  const shell = path.join(bin, "test-shell");
  const log = { text: "" };

  await Promise.all([mkdir(bin), mkdir(workspace), mkdir(grokHome), mkdir(claudeHome)]);
  // The fake harness stands in for grok itself: it is `grok` on PATH, and
  // the promoted-process detector identifies it by that basename. A
  // `claude` copy keeps the production shim pinned as in the other
  // promoted specs.
  await copyFile(harness, path.join(bin, "grok"));
  await chmod(path.join(bin, "grok"), 0o700);
  await copyFile(harness, path.join(bin, "claude"));
  await chmod(path.join(bin, "claude"), 0o700);
  await writeFile(
    shell,
    "#!/bin/sh\nif [ \"${1:-}\" = --version ]; then exec /bin/sh --version; fi\nexec /bin/sh\n",
    { mode: 0o700 },
  );

  await login(page, `e2e-grok-structural-${testName}`);
  const minted = await page.request.post("/v1/hosts/enroll-token", {
    headers: { Origin: new URL(page.url()).origin },
  });
  expect(minted.ok()).toBe(true);
  const enrollToken = (await minted.json()).token;
  await writeFile(tokenFile, enrollToken, { mode: 0o600 });
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
      // Per-test shadow grok home: the promoted file adapter discovers the
      // session here, and the fake grok is told to write the same tree via
      // --home. The operator's real ~/.grok is never touched.
      GROK_HOME: grokHome,
      SHELL: shell,
      PATH: `${bin}:/usr/bin:/bin:/usr/sbin:/sbin`,
      REMUDA_HERDR_ORPHAN_SWEEP: "0",
      RUST_LOG: "info",
    },
  });
  node.stdout?.on("data", (chunk) => {
    log.text += String(chunk);
  });
  node.stderr?.on("data", (chunk) => {
    log.text += String(chunk);
  });
  node.on("error", (error) => {
    log.text += error.message;
  });

  let hostId = "";
  await expect.poll(async () => {
    if (node.exitCode !== null || node.signalCode !== null) {
      throw new Error(`native node exited before enrollment (${node.exitCode ?? node.signalCode})`);
    }
    const response = await page.request.get("/v1/hosts");
    const body = await response.json() as { items: { hostId: string; labels?: string[]; online?: boolean }[] };
    hostId = body.items.find((row) => row.labels?.includes("test=promoted-hooks") && row.online)?.hostId ?? "";
    return hostId;
  }, { timeout: 90_000 }).not.toBe("");

  return {
    dir,
    workspace,
    grokHome,
    eventsFile,
    node,
    hostId,
    enrollToken,
    log,
    redact: [dir, workspace, grokHome, claudeHome],
    dataFd,
  };
}

/** Create a terminal session, launch the patched grok script, promote, open structured. */
async function launchGrokSession(
  page: Page,
  h: Harness,
  scriptPath: string,
  name: string,
): Promise<{ id: string; strip: ReturnType<Page["getByTestId"]> }> {
  const workspaces = await (await page.request.get(`/v1/hosts/${h.hostId}/workspaces`)).json();
  const created = await page.request.post("/v1/instances", {
    headers: { Origin: new URL(page.url()).origin },
    data: {
      hostId: h.hostId,
      workspaceId: workspaces.workspaces[0].workspaceId,
      cwd: h.workspace,
      kind: "terminal",
      driver: "shell-pty",
      name,
      tui: "default",
    },
  });
  const result = await created.json();
  expect(created.ok(), `instance create ${created.status()}: ${JSON.stringify(result)}`).toBe(true);
  const id: string = result.instance.instanceId ?? result.instance.id;
  await page.goto(`/s/${id}/tty`);
  const session = page.getByTestId("session-page");
  await expect(session).toHaveAttribute("data-lifecycle", "running", { timeout: 20_000 });

  // Launch the fake harness as grok in the shell. Explicit --kind selects
  // the grok dialect (the production materializer is not on this path);
  // --home pins the artifact tree inside the shadow GROK_HOME.
  await rawKeys(
    page,
    id,
    `grok --kind grok --no-alt-screen --home ${quote(h.grokHome)} --cwd ${quote(h.workspace)} --script ${quote(scriptPath)} --events-out ${quote(h.eventsFile)}\r`,
  );
  await expect(session).toHaveAttribute("data-mode", "promoted", { timeout: 20_000 });
  await page.getByTestId("view-switch-structured").click();
  await expectStructuredPane(page);
  return { id, strip: page.getByTestId("live-status-strip") };
}

/**
 * Wait for the structured transcript pane to be mounted. The promoted
 * shell-pty pane can transiently render the raw screen fallback while a
 * 2 s instance poll is mid-flight (genericPty flips true until the file
 * signalTier re-hydrates).
 */
async function expectStructuredPane(page: Page, timeout = 30_000): Promise<void> {
  await expect(page.getByTestId("transcript")).toBeVisible({ timeout });
}

/**
 * Bring the settled tool card into the drawn range and return it. With the
 * harness kept alive past the turn, promotion (and the file-tier structured
 * view) does not tear down; the only remaining obstacle is the virtualized,
 * bottom-pinned transcript. Release the bottom-pin by scrolling the scroller
 * to the top (its onScroll handler clears the pin so the layout effect does
 * not snap back), then open the compact summary that swallowed the tool.
 * A guarded reload covers the rare hydration lag on a loaded host.
 */
async function settleStructuredToolCard(page: Page, timeoutMs: number) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    if (await page.getByTestId("transcript").isVisible().catch(() => false)) {
      await page.getByTestId("transcript-scroller").evaluate((el) => {
        el.scrollTop = 0;
      });
      await page.waitForTimeout(200);
      const fold = page.getByTestId("compact-fold").first();
      if (await fold.isVisible().catch(() => false)) await fold.click().catch(() => {});
      const card = page.getByTestId("tool-card").first();
      if (await card.count().catch(() => 0)) return card;
    }
    if (Date.now() >= deadline) throw new Error("structured tool card never settled");
    await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
    await page.waitForTimeout(800);
  }
}

/** Record every data-phase transition the strip paints. */
async function recordPhases(page: Page): Promise<void> {
  await page.evaluate(() => {
    const seen: Array<{ phase: string | null; at: number }> = [];
    (window as unknown as { __livePhases?: typeof seen }).__livePhases = seen;
    const record = () => {
      const phase =
        document.querySelector("[data-testid='live-status-strip']")?.getAttribute("data-phase") ?? null;
      const last = seen.at(-1);
      if (!last || last.phase !== phase) seen.push({ phase, at: performance.now() });
    };
    record();
    new MutationObserver(record).observe(document.body, {
      attributes: true,
      attributeFilter: ["data-phase"],
      childList: true,
      subtree: true,
    });
  });
}

/** All phase tags the durable journal currently carries. */
function journalPhases(page: Page, id: string) {
  return page.evaluate(async (iid) => {
    const res = await fetch(`/v1/instances/${iid}/journal`, { credentials: "include" });
    const body = (await res.json()) as {
      events?: Array<{ event?: { payload?: { relatedIds?: { phase?: string } } } }>;
    };
    return (body.events ?? [])
      .map((row) => row.event?.payload?.relatedIds?.phase)
      .filter((phase): phase is string => typeof phase === "string");
  }, id);
}

async function finishHarness(
  page: Page,
  h: Harness,
  id: string | undefined,
  testInfo: { outputPath: (name: string) => string },
) {
  if (id) {
    // Bound the close POST: under host load the node can stop acking, and an
    // unawaited HTTP response would otherwise eat the whole test budget in
    // teardown (stopNode reaps the process regardless).
    await Promise.race([
      command(page, id, "instance.close"),
      new Promise((resolve) => setTimeout(resolve, 5_000)),
    ]).catch(() => {});
  }
  await stopNode(h.node);
  h.dataFd?.close();
  // Read the mutable sink only now, after the streams have closed, so the
  // artefact carries promotion, the grok adapter and any late panic.
  // Scrub the token, the temp tree, the operator HOME and the host name.
  const safeLog = h.log.text
    .replaceAll(h.enrollToken || "__unused__", "[redacted]")
    .replaceAll(h.dir, "$TEST_DIR")
    .replaceAll(process.env.HOME || "__unused__", "$HOME")
    .replaceAll(hostname(), "test-host");
  const logPath = testInfo.outputPath("native-node.log");
  await writeFile(logPath, safeLog).catch(() => {});
  await rm(h.dir, { recursive: true, force: true });
}

test.describe("grok structural chain over a real fake-harness PTY session", () => {
  test.setTimeout(240_000);

  test("named running tool: native name + title, live stdout partials replaced once, file-decided end", async ({ page }, testInfo) => {
    test.skip(!binariesReady, "remuda/fake-harness/native_hub_e2e binaries not built");
    const h = await startHarness(page, "tool");
    let id: string | undefined;
    try {
      const scriptPath = path.join(h.dir, "grok-tools.patched.json");
      const script = await patchedScenario("grok-tools.json", scriptPath, {
        Bash: SHELL_TOOL_MS,
      });
      expect(script.prefix).toBe("GROK_TOOLS");
      // Pin the fixture-derived inputs the exactly-once loop depends on.
      expect(script.thinking).not.toBe("");
      expect(script.stdoutLines).toEqual(["one", "two", "three"]);
      const launched = await launchGrokSession(page, h, script.path, "grok-structural-tools");
      id = launched.id;
      const strip = launched.strip;
      await recordPhases(page);

      // Submit using the fixture's own match prefix.
      await rawKeys(page, id, script.prefix);
      await new Promise((resolve) => setTimeout(resolve, 100));
      await rawKeys(page, id, "\r");

      // Native thinking rides the file-tier turn.live lifecycle. With one
      // thought chunk the thought, Pending and Running frames land within a
      // single 250 ms adapter poll, so the painted strip can fold thinking
      // away just like tool-started — assert the durable phase from the
      // journal; the streaming transcript row covers what was painted.
      await expect
        .poll(async () => (await journalPhases(page, id!)).includes("thinking"), {
          timeout: 15_000,
          intervals: [50],
        })
        .toBe(true);
      const thoughtRow = page.locator('[data-testid="transcript-row"][data-kind="thought"]').first();
      await expect(thoughtRow).toContainText(script.thinking, { timeout: 5_000 });

      // The named grok tool card: ACP human title plus the stable native
      // name, never raw JSON, while the shell call is Running.
      const card = page.getByTestId("tool-card").first();
      await expect(card).toContainText("run_terminal_command", { timeout: 15_000 });
      await expect(card).toContainText("Execute");
      await expect(card).toContainText("printf");
      const nativeName = card.getByTestId("tool-native-name");
      await expect(nativeName).toHaveText("run_terminal_command");
      await expect(card).toContainText(/running/);
      // Never an exit code before the Final result.
      expect(await card.textContent()).not.toMatch(/exit \d/);

      // The turn-anchored strip clock ticks; the card-local tool-elapsed
      // bridge currently cannot mount for grok (see the evidence doc): pin
      // that gap so a fix is a deliberate assertion change, not a silent
      // pass.
      const liveElapsed = page.getByTestId("live-elapsed");
      await expect(liveElapsed).toBeVisible({ timeout: 10_000 });
      await expect(liveElapsed).toHaveText(/^\d+:\d\d$/);
      const firstReading = await liveElapsed.textContent();
      await expect.poll(async () => (await liveElapsed.textContent()) !== firstReading, {
        timeout: 6_000,
        intervals: [200],
      }).toBe(true);
      await expect(card.getByTestId("tool-elapsed")).toHaveCount(0);

      // Live stdout (c-grok-stdout): the terminal-log tail reaches the card
      // as Partial result text before the Final frame.
      await expect.poll(async () => (await card.textContent()) ?? "", {
        timeout: 10_000,
        intervals: [200],
      }).toContain(script.stdoutLines[0]);
      expect(await card.textContent()).not.toMatch(/exit \d/);
      // The transient tool-started episode is durable in the journal even
      // when React folds it into tool-output within one adapter poll.
      await expect
        .poll(async () => (await journalPhases(page, id!)).includes("tool-started"), {
          timeout: 10_000,
          intervals: [200],
        })
        .toBe(true);

      // Evidence at both widths; the grok card rules require the human
      // title heading and the muted native-name label at every width.
      for (const [width, height, suffix] of [
        [1440, 900, "1440"],
        [390, 844, "390"],
      ] as const) {
        await page.setViewportSize({ width, height });
        await page.emulateMedia({ reducedMotion: "reduce" });
        await page.waitForTimeout(150);
        const overflow = await page.evaluate(
          () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
        );
        expect(overflow, `horizontal overflow at ${suffix}`).toBeLessThanOrEqual(1);
        await expect(nativeName).toBeVisible();
        await expect(nativeName).toHaveText("run_terminal_command");
        await expect(card.locator("span").filter({ hasText: "Execute" }).first()).toBeVisible();
        await shot(page, `grok-structural-1-tool-${suffix}.png`, h.redact);
      }
      await page.setViewportSize({ width: 1440, height: 900 });

      // Ground-truth turn end, then the strip decides the end by file.
      await waitForNthEvent(h.eventsFile, "turn_end", 0, 60_000);
      await expect(strip).toHaveAttribute("data-phase", "turn-ended", { timeout: 15_000 });
      await expect(strip).toHaveAttribute("data-decided-by", "file");
      await expect(page.getByTestId("live-decided-by")).toHaveAttribute("data-channel", "file");
      // The journal carries the full (possibly coalesced) episode; the
      // painted recorder proves what the journal cannot — that the strip
      // actually rendered a live phase before settling on turn-ended. None
      // of these need the structured transcript pane mounted.
      const painted = await page.evaluate(() =>
        (window as unknown as { __livePhases?: Array<{ phase: string | null }> }).__livePhases ?? [],
      );
      expect(painted.some((row) => row.phase), "strip never painted a live phase").toBe(true);
      expect(painted.at(-1)?.phase).toBe("turn-ended");
      const allPhases = await journalPhases(page, id);
      expect(allPhases).toEqual(
        expect.arrayContaining(["thinking", "tool-started", "tool-output", "tool-finished", "turn-ended"]),
      );

      // Reading the settled card needs the structured pane mounted and the
      // virtualized row drawn. Under host load the promoted file signalTier
      // can lag (raw-screen fallback) and the transcript stays bottom-
      // pinned; the helper retries hydration + scroll + fold open until the
      // settled tool card is actually present.
      const settledCard = await settleStructuredToolCard(page, 40_000);
      await expect(settledCard).toContainText("exit 0", { timeout: 15_000 });
      await expect(settledCard).not.toContainText(/running/);

      // c-grok-stdout regression class: the Final replaces the partial
      // tail outright. Every scripted stdout line must appear exactly once
      // as a whole line of the settled stdout block (the `$ <command>`
      // header and partials never duplicate a result line).
      const stdoutText = await settledCard.locator("details pre").first().textContent();
      const stdoutLines = (stdoutText ?? "").split("\n");
      for (const line of script.stdoutLines) {
        expect(
          stdoutLines.filter((row) => row === line),
          `stdout line duplicated or missing: ${line}`,
        ).toEqual([line]);
      }
    } finally {
      await finishHarness(page, h, id, testInfo);
    }
  });

  test("unanswerable question: pending native-tty entity, blocked strip, terminal notes in the approvals queue", async ({ page }, testInfo) => {
    test.skip(!binariesReady, "remuda/fake-harness/native_hub_e2e binaries not built");
    const h = await startHarness(page, "question");
    let id: string | undefined;
    try {
      const scriptPath = path.join(h.dir, "grok-question.patched.json");
      const script = await patchedScenario("grok-question.json", scriptPath, {
        ask_user_question: QUESTION_TOOL_MS,
      });
      expect(script.prefix).toBe("QUESTION");
      const launched = await launchGrokSession(page, h, script.path, "grok-structural-question");
      id = launched.id;
      const strip = launched.strip;

      await rawKeys(page, id, script.prefix);
      await new Promise((resolve) => setTimeout(resolve, 100));
      await rawKeys(page, id, "\r");

      const form = page.getByTestId("question-form");
      await expect(form).toBeVisible({ timeout: 15_000 });
      await expect(form).toContainText("Alpha");
      await expect(form).toContainText("Beta");

      // While the question is pending the strip paints the waiting state —
      // derived from the pending interaction, not a file-tier blocked phase.
      await expect(strip).toHaveAttribute("data-phase", "blocked");

      // The interaction entity itself is unanswerable on the native terminal
      // carrier: the file channel shows the question but has no answer path.
      const interactions = await page.evaluate(async (iid) => {
        const res = await fetch(`/v1/interactions?instanceId=${iid}`, { credentials: "include" });
        return (await res.json()) as {
          items?: Array<{
            state?: string;
            answerable?: boolean;
            carrier?: string;
            request?: { kind?: string; fields?: Array<{ options?: Array<{ label?: string }> }> };
          }>;
        };
      }, id);
      const pending = (interactions.items ?? []).find(
        (item) => item.state === "pending" && item.request?.kind === "question",
      );
      expect(pending, `pending question interaction: ${JSON.stringify(interactions.items)}`).toBeTruthy();
      expect(pending!.answerable).toBe(false);
      expect(pending!.carrier).toBe("native-tty");
      expect(pending!.request?.fields?.[0]?.options?.map((o) => o.label)).toEqual(["Alpha", "Beta"]);

      // The approvals queue is the surface that renders unanswerability:
      // it explains the native-tty carrier, points back to the session for
      // the full prompt, and mounts no inline answer form.
      await page.goto("/approvals");
      const row = page.getByTestId("approval-row").filter({ hasText: "Choose the probe result." }).first();
      await expect(row).toBeVisible({ timeout: 10_000 });
      await expect(row).toContainText("来自终端屏幕 · 回答会发送按键");
      await expect(row).toContainText("请打开会话查看完整终端提示");
      await expect(row.getByTestId("question-form")).toHaveCount(0);

      // Back to the session: the dock still carries the question card.
      await page.goto(`/s/${id}/structured`);
      await expect(page.getByTestId("question-form")).toBeVisible({ timeout: 10_000 });

      // Evidence at both widths with the question genuinely pending.
      for (const [width, height, suffix] of [
        [1440, 900, "1440"],
        [390, 844, "390"],
      ] as const) {
        await page.setViewportSize({ width, height });
        await page.emulateMedia({ reducedMotion: "reduce" });
        await page.waitForTimeout(150);
        const overflow = await page.evaluate(
          () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
        );
        expect(overflow, `horizontal overflow at ${suffix}`).toBeLessThanOrEqual(1);
        // Visibility, not just text presence: at 390 px the form and both
        // options must actually render on screen (toContainText passes on
        // hidden/clipped text).
        const pendingForm = page.getByTestId("question-form");
        await expect(pendingForm).toBeVisible();
        await expect(pendingForm.getByRole("radio", { name: /Alpha/ })).toBeVisible();
        await expect(pendingForm.getByRole("radio", { name: /Beta/ })).toBeVisible();
        await shot(page, `grok-structural-1-question-${suffix}.png`, h.redact);
      }
      await page.setViewportSize({ width: 1440, height: 900 });

      // Ground truth first: the harness answers in its own TUI and the turn
      // ends. Only then does the node drop the interaction and the 2 s list
      // poll clear the dock row.
      await waitForNthEvent(h.eventsFile, "turn_end", 0, 45_000);
      // The node must no longer report a pending question for this instance
      // (authoritative, independent of the dock's poll cadence).
      await expect
        .poll(
          async () => {
            const res = await page.evaluate(async (iid) => {
              const r = await fetch(`/v1/interactions?instanceId=${iid}`, { credentials: "include" });
              return (await r.json()) as { items?: Array<{ state?: string; request?: { kind?: string } }> };
            }, id);
            return (res.items ?? []).some(
              (item) => item.state === "pending" && item.request?.kind === "question",
            );
          },
          { timeout: 15_000, intervals: [500] },
        )
        .toBe(false);
      // The dock follows the cleared list.
      await expect(page.getByTestId("question-form")).toHaveCount(0, { timeout: 15_000 });
      const endStrip = page.getByTestId("live-status-strip");
      await expect(endStrip).toHaveAttribute("data-phase", "turn-ended", { timeout: 10_000 });
      await expect(endStrip).toHaveAttribute("data-decided-by", "file");
    } finally {
      await finishHarness(page, h, id, testInfo);
    }
  });
});
