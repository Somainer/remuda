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
  writeFile,
} from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { login } from "./hub-auth";

/**
 * c-turnend — the turn must be decided from EVERY channel the session already
 * has, not from hook lifecycle tags alone. Two real-native-Node scenarios on
 * the fake harness (REMUDA_PTY_HOOKS=1, terminal emulator on), ground truth
 * from the harness --events-out JSONL:
 *
 *  1. The Stop is lost: hook delivery is broken mid-turn (the per-instance
 *     hook socket is unlinked), so no Stop can land. The screen/pty goes idle
 *     anyway; the strip must leave any stale phase, report 回合结束 decided by
 *     the screen, stop the clock, and POST the prompt that was queued while
 *     the turn was busy.
 *  2. The owner's incident order: a real hook Stop (idle), then the pty idle,
 *     then a `Notification` carrying Claude Code's idle_prompt. That advisory
 *     must never reopen the turn: the strip stays ended (by the hook Stop),
 *     the held prompt flushes, and the Notification surfaces as an in-app
 *     notification entry rather than a 等待操作 phase.
 */
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..");
const target = path.resolve(root, process.env.CARGO_TARGET_DIR ?? "target");
const remuda = process.env.HUB_E2E_REMUDA_BIN ?? path.join(target, "debug/remuda");
const harness = process.env.HUB_E2E_FAKE_HARNESS_BIN ?? path.join(target, "debug/fake-harness");
const nativeNode = process.env.HUB_E2E_NATIVE_NODE_BIN ?? path.join(target, "debug/examples/native_hub_e2e");

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

async function waitForEvent(file: string, name: string, timeoutMs: number): Promise<NativeEvent> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const text = await readFile(file, "utf8").catch(() => "");
    const found = text
      .split("\n")
      .filter(Boolean)
      .map((line) => JSON.parse(line) as NativeEvent)
      .find((row) => row.event === name);
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

type Harness = {
  dir: string;
  dataDir: string;
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

async function bootHarness(page: Page, tag: string, scenario: unknown): Promise<Harness> {
  const scratch = path.join(root, "target", "hub-e2e");
  await mkdir(scratch, { recursive: true });
  const dir = await realpath(await mkdtemp(path.join(scratch, `te-${tag}-`)));
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
  await writeFile(settingsFile, JSON.stringify({ env: { HOOK_E2E_USER_SETTING: "retained" } }));
  await writeFile(scriptFile, JSON.stringify(scenario));

  const minted = await page.request.post("/v1/hosts/enroll-token", {
    headers: { Origin: new URL(page.url()).origin },
  });
  expect(minted.ok()).toBe(true);
  const enrollToken = (await minted.json()).token as string;
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
      SHELL: shell,
      PATH: `${bin}:/usr/bin:/bin:/usr/sbin:/sbin`,
      REMUDA_HERDR_ORPHAN_SWEEP: "0",
      RUST_LOG: "info",
    },
  });

  let hostId = "";
  await expect.poll(async () => {
    if (node.exitCode !== null || node.signalCode !== null) {
      throw new Error(`native node exited before enrollment (${node.exitCode ?? node.signalCode})`);
    }
    const response = await page.request.get("/v1/hosts");
    const body = (await response.json()) as {
      items: { hostId: string; labels?: string[]; online?: boolean }[];
    };
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
      name: `turn-end-${tag}`,
      tui: "default",
    },
  });
  const result = await created.json();
  expect(created.ok(), `instance create ${created.status()}: ${JSON.stringify(result)}`).toBe(true);
  const instanceId = result.instance.instanceId ?? result.instance.id;

  return {
    dir,
    dataDir,
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

async function teardown(page: Page, h: Harness) {
  await command(page, h.instanceId, "instance.close").catch(() => {});
  await stopNode(h.node);
  h.dataHandle?.close();
  await rm(h.dir, { recursive: true, force: true });
}

async function launchAndPromote(page: Page, h: Harness, promptPrefix: string) {
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
  await rawKeys(page, h.instanceId, `${promptPrefix} do the thing`);
  await new Promise((resolve) => setTimeout(resolve, 100));
  await rawKeys(page, h.instanceId, "\r");
}

const LONG_TOOL_SCENARIO = {
  turns: [
    {
      match_prefix: "TURNEND",
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
};

test.describe.configure({ mode: "serial" });

test("screen ends a turn whose Stop hook is lost and flushes the held prompt", async ({ page }) => {
  test.setTimeout(240_000);

  await login(page, "e2e-turn-end-lost");
  const h = await bootHarness(page, "lost", LONG_TOOL_SCENARIO);
  const queued = `QUEUED PROMPT ${Date.now()}`;
  const sent: string[] = [];
  page.on("request", (req) => {
    const url = req.url();
    if (req.method() === "POST" && url.includes(`/v1/instances/${h.instanceId}/commands`)) {
      sent.push(req.postData() ?? "");
    }
  });
  try {
    await launchAndPromote(page, h, "TURNEND");
    const strip = page.getByTestId("live-status-strip");
    await expect(strip).toHaveAttribute("data-phase", "tool-started", { timeout: 10_000 });

    // Queue a prompt while the turn is busy; it must be held, not POSTed yet.
    const composer = page.getByTestId("composer-input");
    await composer.fill(queued);
    await composer.press("Enter");
    await expect.poll(() => sent.some((body) => body.includes(queued)), { timeout: 3_000 }).toBe(false);

    // Break hook delivery mid-turn: unlink the per-instance hook socket so the
    // later PostToolUse/Stop relays cannot connect and no Stop can land.
    await rm(path.join(h.dataDir, "instances", h.instanceId, "hook.sock"), { force: true });

    // Ground truth: the harness finishes the turn regardless of hooks.
    await waitForEvent(h.eventsFile, "turn_end", 60_000);

    // The strip leaves the stale phase and reports a screen-decided end.
    await expect(strip).toHaveAttribute("data-phase", "turn-ended", { timeout: 20_000 });
    await expect.poll(async () => strip.getAttribute("data-turn"), { timeout: 20_000 }).toBe("ended");
    await expect(page.getByTestId("live-decided-by")).toHaveAttribute("data-channel", "screen", {
      timeout: 5_000,
    });
    // The clock is stopped/greyed at the end and the interrupt affordance gone.
    const elapsed = page.getByTestId("live-elapsed");
    await expect(elapsed).toHaveAttribute("data-stale", "1");
    await expect(page.getByTestId("live-interrupt")).toHaveCount(0);

    // The prompt queued while busy is actually POSTed, exactly once.
    await expect
      .poll(() => sent.filter((body) => body.includes(queued)).length, { timeout: 20_000 })
      .toBe(1);
  } finally {
    await teardown(page, h);
  }
});

test("an idle_prompt Notification after Stop never reopens the turn and is listed in-app", async ({ page }) => {
  test.setTimeout(240_000);

  await login(page, "e2e-turn-end-idle");
  const h = await bootHarness(page, "idle", {
    turns: [
      {
        match_prefix: "IDLEPING",
        text: "done, now idle",
        chunks: 1,
        idle_notification: true,
        stop_reason: "end_turn",
        usage: { input_tokens: 30, output_tokens: 8 },
      },
    ],
  });
  const queued = `IDLE QUEUE ${Date.now()}`;
  const sent: string[] = [];
  page.on("request", (req) => {
    const url = req.url();
    if (req.method() === "POST" && url.includes(`/v1/instances/${h.instanceId}/commands`)) {
      sent.push(req.postData() ?? "");
    }
  });
  try {
    await launchAndPromote(page, h, "IDLEPING");
    const strip = page.getByTestId("live-status-strip");
    const composer = page.getByTestId("composer-input");
    await composer.fill(queued);
    await composer.press("Enter");

    // Ground truth ordering: Stop lands, then the idle_prompt Notification.
    await waitForEvent(h.eventsFile, "stop_hook", 60_000);
    await waitForEvent(h.eventsFile, "idle_notification", 60_000);

    // The turn stays ended by the hook Stop and never collapses to 等待操作.
    await expect(strip).toHaveAttribute("data-phase", "turn-ended", { timeout: 20_000 });
    await expect(page.getByTestId("live-decided-by")).toHaveAttribute("data-channel", "hook");
    await expect(strip).not.toHaveAttribute("data-phase", "blocked");
    await expect(page.getByTestId("live-interrupt")).toHaveCount(0);

    // The held prompt flushes on the working→idle edge, exactly once.
    await expect
      .poll(() => sent.filter((body) => body.includes(queued)).length, { timeout: 20_000 })
      .toBe(1);

    // The idle advisory is surfaced inside the app, not folded into the phase.
    const note = page.getByTestId("session-notification").first();
    await expect(note).toBeVisible({ timeout: 10_000 });
    await expect(note).toHaveAttribute("data-type", "idle_prompt");
    expect(await note.textContent()).toContain("waiting for your input");
  } finally {
    await teardown(page, h);
  }
});
