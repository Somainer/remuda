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
 * c-livephrase — the screen-tier spinner status line, end to end.
 *
 * The fake harness (claude 2.1.270 *modern* dialect: OSC busy edges, no
 * literal "esc to interrupt" phrase) paints real captured status lines:
 *
 *   · Razzmatazzing… (2m 10s · ↓ 12.0k tokens · thinking some more with xhigh effort)
 *   · Razzmatazzing… (49m 38s · ↓ 66.0k tokens · thinking some more with xhigh effort)
 *
 * The native PTY carrier's emulator parses them off the rendered grid and
 * journals `live.status` lifecycles; the page renders verb/elapsed/tokens/
 * phrase, offers "Esc 打断", and mounts a collapsed live thinking row.
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

type NativeEvent = { event: string; wallMs?: number };

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

test("screen spinner status: verb/tokens/phrase reach the strip and Esc interrupts", async ({ page }, testInfo) => {
  test.setTimeout(240_000);
  const scratch = path.join(root, "target", "hub-e2e");
  await mkdir(scratch, { recursive: true });
  const dir = await realpath(await mkdtemp(path.join(scratch, "lp-")));
  const dataDir = path.join(dir, "data");
  await mkdir(dataDir);
  const dataStat = await stat(dataDir);
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
    // ~12 s reasoning window; the two scripted spinner frames advance on the
    // fake's one-second repaint and then hold. The modern dialect supplies
    // the OSC busy edges — the literal "esc to interrupt" text is absent.
    await writeFile(
      scriptFile,
      JSON.stringify({
        version: 1,
        turns: [
          {
            match_prefix: "LIVEPHRASE",
            thinking: "weighing B-trees against LSM-tree storage engines in extensive detail",
            think_chunks: 48,
            chunk_delay_ms: 250,
            text: "conclusion: workload decides",
            chunks: 1,
            spinner: {
              // Hold frame 1 for the first 3 one-second repaints (the native
              // poll runs every 800 ms), then frame 2 for the rest of the turn.
              frames: [
                "· Razzmatazzing… (2m 10s · ↓ 12.0k tokens · thinking some more with xhigh effort)",
                "· Razzmatazzing… (2m 10s · ↓ 12.0k tokens · thinking some more with xhigh effort)",
                "· Razzmatazzing… (2m 10s · ↓ 12.0k tokens · thinking some more with xhigh effort)",
                "· Razzmatazzing… (49m 38s · ↓ 66.0k tokens · thinking some more with xhigh effort)",
              ],
            },
            stop_reason: "end_turn",
            usage: { input_tokens: 40, output_tokens: 12 },
          },
        ],
      }),
    );

    await login(page, "e2e-live-phrase");
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
    await expect
      .poll(async () => {
        if (node && (node.exitCode !== null || node.signalCode !== null)) {
          throw new Error(`native node exited before enrollment (${node.exitCode ?? node.signalCode})`);
        }
        const response = await page.request.get("/v1/hosts");
        const body = (await response.json()) as {
          items: { hostId: string; labels?: string[]; online?: boolean }[];
        };
        hostId = body.items.find((row) => row.labels?.includes("test=promoted-hooks") && row.online)?.hostId ?? "";
        return hostId;
      }, { timeout: 90_000 })
      .not.toBe("");
    const workspaces = await (await page.request.get(`/v1/hosts/${hostId}/workspaces`)).json();
    const created = await page.request.post("/v1/instances", {
      headers: { Origin: new URL(page.url()).origin },
      data: {
        hostId,
        workspaceId: workspaces.workspaces[0].workspaceId,
        cwd: workspace,
        kind: "terminal",
        driver: "shell-pty",
        name: "ux-livephrase",
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

    await rawKeys(
      page,
      id,
      `claude --kind claude --dialect-version modern --settings ${quote(settingsFile)} --home ${quote(claudeHome)} --script ${quote(scriptFile)} --events-out ${quote(eventsFile)}\r`,
    );
    await expect(session).toHaveAttribute("data-mode", "promoted", { timeout: 20_000 });
    await page.getByTestId("view-switch-structured").click();
    await expect(page.getByTestId("transcript")).toBeVisible();
    const strip = page.getByTestId("live-status-strip");

    // Record the strip's spinner fields over time so a frame the 800 ms poll
    // sees only briefly cannot be missed by point-in-time polling.
    await page.evaluate(() => {
      const readings: Array<{ phase: string | null; tokens: string | null; verb: string | null }> = [];
      (window as unknown as { __lpReadings?: typeof readings }).__lpReadings = readings;
      const record = () => {
        const pick = (testid: string) =>
          document.querySelector(`[data-testid='${testid}']`)?.textContent ?? null;
        const last = readings.at(-1);
        const row = {
          phase: document.querySelector("[data-testid='live-status-strip']")?.getAttribute("data-phase") ?? null,
          tokens: pick("live-token-count"),
          verb: pick("live-verb"),
        };
        if (
          !last ||
          last.phase !== row.phase ||
          last.tokens !== row.tokens ||
          last.verb !== row.verb
        ) {
          readings.push(row);
        }
      };
      record();
      new MutationObserver(record).observe(document.body, {
        attributes: true,
        childList: true,
        subtree: true,
        characterData: true,
      });
    });

    await rawKeys(page, id, "LIVEPHRASE reason about storage engines");
    await new Promise((resolve) => setTimeout(resolve, 300));
    await rawKeys(page, id, "\r");

    // 1 — the strip reads the spinner frames: thinking phase, verb, the screen
    //     token estimate and the effort-qualified phrase.
    await expect(strip).toHaveAttribute("data-phase", "thinking", { timeout: 20_000 });
    await expect(page.getByTestId("live-verb")).toHaveText("Razzmatazzing…", { timeout: 10_000 });
    const tokenCount = page.getByTestId("live-token-count");
    await expect(tokenCount).toContainText("66.0k tokens", { timeout: 15_000 });
    expect(await tokenCount.getAttribute("data-source")).toBe("screen");
    await expect(page.getByTestId("live-phrase")).toContainText("xhigh effort");
    await expect(page.getByTestId("live-elapsed")).toBeVisible();

    // 2 — both scripted frames crossed the wire (the first may be brief: the
    //     fake advances frames on its one-second repaint).
    const readings = await page.evaluate(() =>
      (window as unknown as { __lpReadings?: Array<{ tokens: string | null }> }).__lpReadings ?? [],
    );
    const tokenHistory = readings.map((row) => row.tokens).filter((text): text is string => !!text);
    expect(tokenHistory, `token readings: ${JSON.stringify(tokenHistory)}`).toContainEqual(
      expect.stringContaining("12.0k tokens"),
    );
    expect(tokenHistory).toContainEqual(expect.stringContaining("66.0k tokens"));

    // 3 — a collapsed live "thinking" row is mounted in the transcript, so a
    //     reader scrolled away from the dock still sees the model reasoning.
    const liveThought = page.locator("details").filter({ hasText: "从屏幕猜测" }).first();
    await expect(liveThought).toBeVisible({ timeout: 5_000 });

    await shot(page, "live-phrase-1-strip-thinking.png");

    // 4 — the interrupt affordance is offered (modern dialect: proven by the
    //     OSC busy edge, not by retired hint text) and sends Esc through the
    //     existing keys path; the harness ground truth records the interrupt.
    const interrupt = page.getByTestId("live-interrupt");
    await expect(interrupt).toBeVisible();
    expect(await interrupt.textContent()).toContain("Esc");
    await interrupt.click();
    const groundTruth = await waitForEvent(eventsFile, "interrupt", 20_000);
    expect(groundTruth.event).toBe("interrupt");

    // 5 — the spinner reading clears: the verb leaves the strip and the live
    //     thinking row unmounts.
    await expect(page.getByTestId("live-verb")).toHaveCount(0, { timeout: 20_000 });
    await expect(liveThought).toHaveCount(0, { timeout: 20_000 });

    // 6 — both themes at 390 and 1440 px: the richer strip never causes a
    //     horizontal page scroll.
    const setTheme = async (theme: "night" | "ledger") => {
      await page.evaluate((value) => {
        document.documentElement.dataset.theme = value;
      }, theme);
    };
    // Restart one short turn just to repaint the populated strip for the
    // matrix captures.
    await rawKeys(page, id, "LIVEPHRASE again");
    await new Promise((resolve) => setTimeout(resolve, 300));
    await rawKeys(page, id, "\r");
    await expect(page.getByTestId("live-token-count")).toContainText("12.0k", { timeout: 20_000 });
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
      await shot(page, `live-phrase-1-strip-${suffix}.png`);
    }
  } catch (error) {
    const diaryPath = testInfo.outputPath("node-log.txt");
    await writeFile(diaryPath, nodeLog).catch(() => {});
    await testInfo.attach("node-log.txt", { path: diaryPath, contentType: "text/plain" }).catch(() => {});
    throw error;
  } finally {
    if (node) await stopNode(node);
    if (!evidence) await rm(dir, { recursive: true, force: true }).catch(() => {});
  }
});
