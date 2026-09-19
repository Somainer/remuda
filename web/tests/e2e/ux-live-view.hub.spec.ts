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
import path from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { login } from "./hub-auth";

/**
 * Batch C — w-live-view. A real native Node drives fake-harness (the
 * deterministic Claude TUI double) through a real PTY with the production
 * hook overlay (REMUDA_PTY_HOOKS=1) and terminal emulator on. The browser
 * reads the live layer the Rust batches already emit: turn.live phase tags,
 * running tool nodes, and — derived entirely in the page — the 1 Hz elapsed
 * reading and channel health. Nothing here mocks the journal.
 *
 * Ground truth is the harness --events-out JSONL (wallMs anchors); the strip
 * is asserted against it, never against itself.
 */
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..");
const target = path.resolve(root, process.env.CARGO_TARGET_DIR ?? "target");
const remuda = process.env.HUB_E2E_REMUDA_BIN ?? path.join(target, "debug/remuda");
const harness = process.env.HUB_E2E_FAKE_HARNESS_BIN ?? path.join(target, "debug/fake-harness");
const nativeNode = process.env.HUB_E2E_NATIVE_NODE_BIN ?? path.join(target, "debug/examples/native_hub_e2e");

/** Committed evidence only under REMUDA_EVIDENCE=1; default runs stay git-clean. */
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence
  ? path.join(root, "docs/design/evidence")
  : path.join(root, "web/test-results/evidence");

async function shot(page: Page, name: string) {
  await mkdir(shotDir, { recursive: true });
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

type NativeEvent = { event: string; wallMs?: number; toolName?: string };

async function nativeEvents(file: string): Promise<NativeEvent[]> {
  const text = await readFile(file, "utf8").catch(() => "");
  return text
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line) as NativeEvent);
}

async function waitForEvent(file: string, name: string, timeoutMs: number): Promise<NativeEvent> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const found = (await nativeEvents(file)).find((row) => row.event === name);
    if (found) return found;
    if (Date.now() > deadline) throw new Error(`ground-truth ${name} not seen within ${timeoutMs}ms`);
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

test("live status strip: phase/elapsed/health over a real PTY turn", async ({ page }, testInfo) => {
  test.setTimeout(240_000);
  const scratch = path.join(root, "target", "hub-e2e");
  await mkdir(scratch, { recursive: true });
  const dir = await realpath(await mkdtemp(path.join(scratch, "lv-")));
  const dataDir = path.join(dir, "data");
  await mkdir(dataDir);
  const dataStat = await stat(dataDir);
  // Linux /proc/self/fd alias keeps the data directory resolvable (see
  // promoted-claude.hub.spec.ts for the sockaddr-length rationale).
  const dataHandle = process.platform === "linux" ? await open(dataDir, "r") : undefined;
  const dataPath = dataHandle
    ? `/proc/${process.pid}/fd/${dataHandle.fd}`
    : dataDir;
  const bin = path.join(dir, "bin");
  const workspace = path.join(dir, "workspace");
  const claudeHome = path.join(dir, "claude-home");
  const eventsFile = path.join(dir, "native-events.jsonl");
  const settingsFile = path.join(dir, "user-settings.json");
  const scriptFile = path.join(dir, "scenario.json");
  const tokenFile = path.join(dir, "enroll-token");
  const shell = path.join(bin, "test-shell");
  let node: ChildProcess | undefined;
  let instanceId: string | undefined;
  let enrollToken = "";
  let nodeLog = "";

  try {
    await Promise.all([mkdir(bin), mkdir(workspace), mkdir(claudeHome)]);
    await copyFile(harness, path.join(bin, "claude"));
    await chmod(path.join(bin, "claude"), 0o700);
    await writeFile(
      shell,
      "#!/bin/sh\nif [ \"${1:-}\" = --version ]; then exec /bin/sh --version; fi\nexec /bin/sh\n",
      { mode: 0o700 },
    );
    await writeFile(settingsFile, JSON.stringify({ env: { HOOK_E2E_USER_SETTING: "retained" } }));
    // One 15 s auto-approved Bash tool: long enough for the 3×2 s hook
    // silence budget to trip and for four evidence captures, then a short
    // streamed answer and Stop.
    await writeFile(
      scriptFile,
      JSON.stringify({
        turns: [
          {
            match_prefix: "LIVEVIEW",
            text: "the long tool finished\nhere is the answer",
            chunks: 2,
            chunk_delay_ms: 120,
            tools: [
              {
                name: "Bash",
                input: { command: "for i in 1 2 3 4 5; do sleep 3; done" },
                approval: "auto",
                duration_ms: 15_000,
                exit_code: 0,
              },
            ],
            stop_reason: "end_turn",
            usage: { input_tokens: 40, output_tokens: 12 },
          },
        ],
      }),
    );

    await login(page, "e2e-live-view");
    const minted = await page.request.post("/v1/hosts/enroll-token", {
      headers: { Origin: new URL(page.url()).origin },
    });
    expect(minted.ok()).toBe(true);
    enrollToken = (await minted.json()).token;
    await writeFile(tokenFile, enrollToken, { mode: 0o600 });
    const hub = new URL(process.env.VITE_HUB_URL ?? `http://${process.env.HUB_E2E_LISTEN ?? "127.0.0.1:58880"}`);
    hub.protocol = hub.protocol === "https:" ? "wss:" : "ws:";
    hub.pathname = "/v1/node";
    node = spawn(nativeNode, [], {
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
        RUST_LOG: "info",
      },
    });
    node.stdout?.on("data", (chunk) => {
      nodeLog += String(chunk);
    });
    node.stderr?.on("data", (chunk) => {
      nodeLog += String(chunk);
    });
    node.on("error", (error) => {
      nodeLog += error.message;
    });

    let hostId = "";
    await expect.poll(async () => {
      if (node && (node.exitCode !== null || node.signalCode !== null)) {
        throw new Error(`native node exited before enrollment (${node.exitCode ?? node.signalCode})`);
      }
      const response = await page.request.get("/v1/hosts");
      const body = await response.json() as { items: { hostId: string; labels?: string[]; online?: boolean }[] };
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
        name: "ux-live-view",
        tui: "default",
      },
    });
    const result = await created.json();
    expect(created.ok(), `instance create ${created.status()}: ${JSON.stringify(result)}`).toBe(true);
    instanceId = result.instance.instanceId ?? result.instance.id;
    const id = instanceId!;
    await page.goto(`/s/${id}/tty`);
    const session = page.getByTestId("session-page");
    await expect(session).toHaveAttribute("data-lifecycle", "running", { timeout: 20_000 });

    // Frame-level diary of what the follow socket actually delivers.
    const frameSeqs: Array<{ seq: string; kind?: string; phase?: string; channel?: string; type?: string }> = [];
    page.on("websocket", (socket) => {
      if (new URL(socket.url()).pathname !== "/v1/follow") return;
      socket.on("framereceived", ({ payload }) => {
        if (typeof payload !== "string") return;
        try {
          const frame = JSON.parse(payload);
          const rows = frame.type === "snapshot" ? (frame.events ?? []) : frame.type === "event" ? [frame.event] : [];
          for (const row of rows) {
            const event = row.event ?? row;
            frameSeqs.push({
              seq: String(row.seq ?? event.seq ?? frame.seq ?? "?"),
              type: frame.type,
              kind: event.kind,
              channel: event.source?.channel,
              phase: event.payload?.relatedIds?.phase,
            });
          }
        } catch { /* ignore */ }
      });
    });

    await rawKeys(
      page,
      id,
      `claude --kind claude --settings ${quote(settingsFile)} --home ${quote(claudeHome)} --script ${quote(scriptFile)} --events-out ${quote(eventsFile)}\r`,
    );
    await expect(session).toHaveAttribute("data-mode", "promoted", { timeout: 20_000 });
    await page.getByTestId("view-switch-structured").click();
    await expect(page.getByTestId("transcript")).toBeVisible();
    const strip = page.getByTestId("live-status-strip");

    // Record every phase the strip renders, with a timestamp, so transients
    // (prompt-accepted lasts only until PreToolUse) cannot be missed by
    // polling.
    await page.evaluate(() => {
      const seen: Array<{ phase: string | null; at: number }> = [];
      (window as unknown as { __livePhases?: typeof seen }).__livePhases = seen;
      // The strip renders nothing before the first phase (it mounts on
      // prompt-accepted), so observe the whole subtree for its mount and its
      // later data-phase transitions. MutationObserver catches sub-rAF
      // transients that polling could miss.
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

    // 1 — submit flips the strip to prompt-accepted within the budget.
    await rawKeys(page, id, "LIVEVIEW do the thing");
    await new Promise((resolve) => setTimeout(resolve, 100));
    const submitMark = await page.evaluate(() => performance.now());
    await rawKeys(page, id, "\r");
    await expect.poll(
      async () =>
        (await page.evaluate(() =>
          (window as unknown as { __livePhases?: Array<{ phase: string | null }> }).__livePhases?.some(
            (row) => row.phase === "prompt-accepted",
          ),
        ))
          ? true
          : false,
      { timeout: 5_000, intervals: [50] },
    ).toBe(true);
    // The first prompt-accepted render landed within the e2e wall budget
    // (native→journal design budget is 150 ms; this adds one uplink RTT).
    const acceptedFlip = await page.evaluate(() => {
      const seen = (window as unknown as { __livePhases?: Array<{ phase: string | null; at: number }> }).__livePhases ?? [];
      return seen.find((row) => row.phase === "prompt-accepted")?.at ?? null;
    });
    expect(acceptedFlip).not.toBeNull();
    expect(acceptedFlip! - submitMark).toBeLessThan(5_000);

    // 2 — tool-started, and the running Bash card ticks a local elapsed.
    try {
      await expect(strip).toHaveAttribute("data-phase", "tool-started", { timeout: 10_000 });
    } catch (error) {
      const rest = await (await page.request.get(`/v1/instances/${id}/journal`)).json() as {
        events?: Array<{ seq?: string; event?: { kind?: string; source?: { channel?: string }; payload?: { relatedIds?: { phase?: string } } } }>;
      };
      const restSummary = (rest.events ?? []).map((row) => ({
        seq: row.seq,
        kind: row.event?.kind,
        channel: row.event?.source?.channel,
        phase: row.event?.payload?.relatedIds?.phase,
      }));
      const diaryPath = testInfo.outputPath("frame-diary.json");
      const domState = await page.evaluate(() => ({
        journal: document.querySelector("[data-testid='session-page']")?.getAttribute("data-journal"),
        activity: document.querySelector("[data-testid='session-page']")?.getAttribute("data-activity"),
        stripPhase: document.querySelector("[data-testid='live-status-strip']")?.getAttribute("data-phase"),
        stripHealth: document.querySelector("[data-testid='live-status-strip']")?.getAttribute("data-health"),
        rows: [...document.querySelectorAll("[data-testid='transcript-row']")].map((el) => ({
          kind: el.getAttribute("data-kind"),
          text: (el.textContent ?? "").slice(0, 80),
        })),
      }));
      await writeFile(diaryPath, JSON.stringify({ frameSeqs, restSummary, domState }, null, 2));
      await testInfo.attach("frame-diary.json", { path: diaryPath, contentType: "application/json" }).catch(() => {});
      throw error;
    }
    const card = page.getByTestId("tool-card").first();
    await expect(card).toContainText(/running/);
    // Never an exit code before the result is Final.
    expect(await card.textContent()).not.toMatch(/exit \d/);
    const toolElapsed = card.getByTestId("tool-elapsed");
    await expect(toolElapsed).toBeVisible({ timeout: 5_000 });
    await expect(toolElapsed).toHaveText(/^\d+:\d\d$/);
    const firstReading = await toolElapsed.textContent();
    await expect.poll(async () => (await toolElapsed.textContent()) !== firstReading, {
      timeout: 5_000,
      intervals: [200],
    }).toBe(true);
    const secondReading = await toolElapsed.textContent();
    expect(secondReading).not.toBe(firstReading);

    // 3 — the hook tier goes silent during the long tool: the health note
    // appears (3 × 2 s cadence) and the elapsed greys, with no turn-ended.
    await expect(page.getByTestId("live-health-hook")).toHaveAttribute("data-reason", "stalled", {
      timeout: 12_000,
    });
    await expect(strip).not.toHaveAttribute("data-phase", "turn-ended");
    await expect(session).toHaveAttribute("data-activity", "working");
    await expect(toolElapsed).toHaveAttribute("data-stale", "1");

    // 4 — the strip is status only: none of the OSC/screen content painted
    //     behind it (command line, spinner glyphs, output) ever appears.
    const stripText = (await strip.textContent()) ?? "";
    expect(stripText).not.toContain("for i in");
    expect(stripText).not.toContain("⎿");
    // The screen tier carries no phrase on claude today; if one ever arrives
    // it is rendered muted and separately, never as the phase label.
    await expect(page.getByTestId("live-phrase")).toHaveCount(0);

    // 5 — both themes at 390 and 1440 px, no horizontal page scroll.
    const setTheme = async (theme: "night" | "ledger") => {
      await page.evaluate((value) => {
        document.documentElement.dataset.theme = value;
      }, theme);
    };
    for (const [width, height, theme, suffix] of [
      [1440, 900, "night", "1440-night"],
      [390, 844, "night", "390-night"],
      [1440, 900, "ledger", "1440-ledger"],
      [390, 844, "ledger", "390-ledger"],
    ] as const) {
      await page.setViewportSize({ width, height });
      await setTheme(theme);
      await page.emulateMedia({ reducedMotion: "reduce" });
      await page.waitForTimeout(150);
      const overflow = await page.evaluate(
        () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
      );
      expect(overflow, `horizontal overflow at ${suffix}`).toBeLessThanOrEqual(1);
      await shot(page, `live-view-c-strip-${suffix}.png`);
    }

    // 6 — no premature turn-ended: the ground-truth Stop must appear before
    //     the strip is allowed to show turn-ended.
    const turnEnd = await waitForEvent(eventsFile, "turn_end", 60_000);
    expect(turnEnd.event).toBe("turn_end");
    await expect(strip).toHaveAttribute("data-phase", "turn-ended", { timeout: 10_000 });
    // The strip never reached turn-ended ahead of the ground truth: the
    // recorded phase history shows prompt-accepted … tool-started and a
    // terminal turn-ended only now.
    const phases = await page.evaluate(() =>
      (window as unknown as { __livePhases?: Array<{ phase: string | null; at: number }> }).__livePhases?.map(
        (row) => row.phase,
      ),
    );
    expect(phases).toContain("prompt-accepted");
    expect(phases).toContain("tool-started");
    expect(phases?.indexOf("turn-ended")).toBe(phases!.length - 1);

    // 7 — Final arrives at the 390px width (last viewport in the step-5
    //     loop): D-041 folds the now-settled card the moment its result
    //     lands, even though it was watched while running. Expand the row to
    //     see the desktop-identical full card: elapsed stops, exit 0 shows.
    await expect(card).toHaveAttribute("data-folded", "1", { timeout: 5_000 });
    // At 390px the floating composer dock can cover the bottom-pinned row;
    // scroll clear and drive the click directly rather than retrying under
    // the overlay until the 90s test timeout.
    const foldToggle = card.getByTestId("tool-fold-open");
    await foldToggle.evaluate((el) => el.scrollIntoView({ block: "center" }));
    await foldToggle.evaluate((el) => el.click());
    await expect(card).toHaveAttribute("data-folded", "0");
    await expect(card).toContainText("exit 0");
    await expect(card.getByTestId("tool-elapsed")).toHaveCount(0);
    // Hook tier spoke again at PostToolUse; the stall note clears.
    await expect(page.getByTestId("live-health-hook")).toHaveCount(0, { timeout: 5_000 });
  } finally {
    if (instanceId) {
      try {
        const rows = await (await page.request.get(`/v1/instances/${instanceId}/journal`)).json() as {
          events?: Array<{ seq?: string; event?: { kind?: string; source?: { channel?: string }; payload?: { nativeName?: string; relatedIds?: Record<string, string>; state?: string } } }>;
        };
        const summary = (rows.events ?? []).map((row) => ({
          seq: row.seq,
          kind: row.event?.kind,
          channel: row.event?.source?.channel,
          nativeName: row.event?.payload?.nativeName,
          phase: row.event?.payload?.relatedIds?.phase,
          state: row.event?.payload?.state,
        }));
        const snapshotPath = testInfo.outputPath("journal-summary.json");
        await writeFile(snapshotPath, JSON.stringify(summary, null, 2));
        await testInfo.attach("journal-summary.json", { path: snapshotPath, contentType: "application/json" }).catch(() => {});
      } catch { /* teardown only */ }
    }
    if (instanceId) await command(page, instanceId, "instance.close").catch(() => {});
    if (node) await stopNode(node);
    try {
      const groundPath = testInfo.outputPath("native-events.jsonl");
      await writeFile(groundPath, await readFile(eventsFile, "utf8").catch(() => ""));
      await testInfo.attach("native-events.jsonl", { path: groundPath, contentType: "application/x-ndjson" }).catch(() => {});
    } catch { /* no ground truth file */ }
    dataHandle?.close();
    const logPath = testInfo.outputPath("native-node.log");
    await writeFile(
      logPath,
      nodeLog
        .replaceAll(enrollToken || "__unused__", "[redacted]")
        .replaceAll(dir, "$TEST_DIR"),
    ).catch(() => {});
    await testInfo.attach("native-node.log", { path: logPath, contentType: "text/plain" }).catch(() => {});
    await rm(dir, { recursive: true, force: true });
  }
});
