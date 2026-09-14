import { expect, test, type Page } from "@playwright/test";
import { execFile, spawn, type ChildProcess } from "node:child_process";
import { access, chmod, copyFile, mkdir, mkdtemp, readFile, realpath, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { hostname } from "node:os";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { login } from "./hub-auth";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..");
const target = path.resolve(root, process.env.CARGO_TARGET_DIR ?? "target");
const remuda = process.env.HUB_E2E_REMUDA_BIN ?? path.join(target, "debug/remuda");
const harness = process.env.HUB_E2E_FAKE_HARNESS_BIN ?? path.join(target, "debug/fake-harness");

// A standalone Hub gate only builds its fake Hub example. Always refresh the
// default native executables so a clean or stale Cargo target tests this tree.
// Keep this in the spec: an externally reused Hub also needs the local Node.
test.beforeAll(async ({}, testInfo) => {
  testInfo.setTimeout(600_000);
  const args = ["build", "--locked"];
  if (!process.env.HUB_E2E_REMUDA_BIN) args.push("-p", "remuda", "--bin", "remuda");
  if (!process.env.HUB_E2E_FAKE_HARNESS_BIN) args.push("-p", "remuda-testing", "--bin", "fake-harness");
  if (args.length > 2) {
    await promisify(execFile)("cargo", args, {
      cwd: root, env: process.env, timeout: 570_000, maxBuffer: 8 * 1024 * 1024,
    });
  }
  await Promise.all([access(remuda), access(harness)]);
});

type NativeEvent = { event: string; by?: string; outcome?: string };
type JournalEvent = { source?: { channel?: string }; kind?: string; payload?: { nativeName?: string } };
type JournalRow = { seq?: string; observedAt?: string; event: JournalEvent };

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

async function journalRows(page: Page, instanceId: string): Promise<JournalRow[]> {
  const response = await page.request.get(`/v1/instances/${instanceId}/journal`);
  expect(response.ok()).toBe(true);
  const body = await response.json() as { events: JournalRow[] };
  return body.events;
}

async function journal(page: Page, instanceId: string): Promise<JournalEvent[]> {
  return (await journalRows(page, instanceId)).map((row) => row.event);
}

async function nativeEvents(file: string): Promise<NativeEvent[]> {
  const text = await readFile(file, "utf8").catch(() => "");
  return text.split("\n").slice(0, -1).filter(Boolean).map((line) => JSON.parse(line) as NativeEvent);
}

async function stopNode(node: ChildProcess) {
  if (!node.pid || node.exitCode !== null || node.signalCode !== null) return;
  const exited = new Promise<void>((resolve) => node.once("exit", () => resolve()));
  node.kill("SIGTERM");
  const timer = setTimeout(() => node.kill("SIGKILL"), 10_000);
  await exited;
  clearTimeout(timer);
}

/**
 * This spec attaches a disposable production Node to the suite's Hub. The
 * existing fake WebSocket Node cannot prove hook relay, PTY keys, or native
 * interrupt settlement. Setup builds remuda and fake-harness in CARGO_TARGET_DIR;
 * the two HUB_E2E_*_BIN overrides opt into explicit prebuilt executables.
 */
test("promoted Claude: hooks drive activity and 打断 sends a native Esc without closing", async ({ page }, testInfo) => {
  test.setTimeout(180_000);
  // Keep Unix socket paths below sockaddr_un's macOS limit.
  const dir = await realpath(await mkdtemp("/tmp/hge-"));
  const bin = path.join(dir, "bin");
  const workspace = path.join(dir, "workspace");
  const claudeHome = path.join(dir, "claude-home");
  const eventsFile = path.join(dir, "native-events.jsonl");
  const settingsFile = path.join(dir, "user-settings.json");
  const scriptFile = path.join(dir, "scenario.json");
  const tokenFile = path.join(dir, "enroll-token");
  const shell = path.join(bin, "test-shell");
  let node: ChildProcess | undefined;
  let nodeLog = "";
  let instanceId: string | undefined;
  let enrollToken = "";
  const timing: Record<string, unknown>[] = [];
  const recordTiming = (kind: string, data: Record<string, unknown>) => {
    if (timing.length < 256) timing.push({ at: new Date().toISOString(), kind, ...data });
  };
  page.on("websocket", (socket) => {
    if (new URL(socket.url()).pathname !== "/v1/follow") return;
    socket.on("framereceived", ({ payload }) => {
      if (typeof payload !== "string") return;
      try {
        const frame = JSON.parse(payload);
        const rows = frame.type === "snapshot" ? frame.events : frame.type === "event" ? [frame] : [];
        for (const row of Array.isArray(rows) ? rows : []) {
          const event = row.event ?? row;
          if ((row.instanceId ?? event.instanceId ?? frame.instanceId) !== instanceId || event.kind !== "lifecycle") continue;
          const native = event.payload;
          recordTiming("follow-received", {
            seq: row.seq ?? event.seq, observedAt: event.observedAt,
            payloadType: native?.type, nativeName: native?.nativeName,
            remudaActivity: native?.relatedIds?.remudaActivity,
            entityActivity: native?.entityType === "instance" ? native.entity?.activity?.value : undefined,
          });
        }
      } catch { /* Binary or unrelated follow frames carry no timing evidence. */ }
    });
  });
  try {
    await Promise.all([mkdir(bin), mkdir(workspace), mkdir(claudeHome)]);
    // The detector sees the real foreground executable named claude. This is
    // still fake-harness --kind claude, with its production-shaped TUI/hooks.
    await copyFile(harness, path.join(bin, "claude"));
    await chmod(path.join(bin, "claude"), 0o700);
    // Ignore the login-shell argument to isolate user startup files and retain
    // the per-instance shim PATH installed by the production shell driver.
    await writeFile(shell, "#!/bin/sh\nif [ \"${1:-}\" = --version ]; then exec /bin/sh --version; fi\nexec /bin/sh\n", { mode: 0o700 });
    await writeFile(settingsFile, JSON.stringify({ env: { HOOK_E2E_USER_SETTING: "retained" } }));
    await writeFile(scriptFile, JSON.stringify({ turns: [
      { match: "COMPLETE", text: "SPIKE_COMPLETE", chunks: 3, chunk_delay_ms: 500,
        tools: [{ name: "Bash", input: { command: "echo complete" }, approval: "auto", duration_ms: 1200 }] },
      { match: "INTERRUPT", text: "must not complete", tools: [
        { name: "Bash", input: { command: "sleep 30" }, approval: "auto", duration_ms: 30_000 },
      ] },
    ] }));

    await login(page, "e2e-native-hooks");
    const minted = await page.request.post("/v1/hosts/enroll-token", {
      headers: { Origin: new URL(page.url()).origin },
    });
    expect(minted.ok()).toBe(true);
    enrollToken = (await minted.json()).token;
    await writeFile(tokenFile, enrollToken, { mode: 0o600 });
    const hub = new URL(process.env.VITE_HUB_URL ?? `http://${process.env.HUB_E2E_LISTEN ?? "127.0.0.1:58880"}`);
    hub.protocol = hub.protocol === "https:" ? "wss:" : "ws:";
    hub.pathname = "/v1/node";
    node = spawn(remuda, ["node", "--hub-url", hub.toString(), "--host-token-file", tokenFile,
      "--workspace", workspace, "--workspace-root", workspace,
      "--label", "test=promoted-hooks", "--no-herdr-orphan-sweep"], {
      cwd: dir,
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env, REMUDA_DATA_DIR: path.join(dir, "data"),
        REMUDA_PTY_EMULATOR: "1", REMUDA_PTY_HOOKS: "1", REMUDA_SHIM: "on",
        REMUDA_CLAUDE_BIN: path.join(bin, "claude"), REMUDA_CLAUDE_CONFIG_DIR: claudeHome,
        CLAUDE_CONFIG_DIR: claudeHome, SHELL: shell, PATH: `${bin}:/usr/bin:/bin:/usr/sbin:/sbin`,
        REMUDA_HERDR_ORPHAN_SWEEP: "0", RUST_LOG: "info" },
    });
    node.stdout?.on("data", (chunk) => { nodeLog += String(chunk); });
    node.stderr?.on("data", (chunk) => { nodeLog += String(chunk); });
    node.on("error", (error) => { nodeLog += error.message; });
    let hostId = "";
    await expect.poll(async () => {
      if (node && (node.exitCode !== null || node.signalCode !== null)) {
        throw new Error(`Native Node exited before enrollment (${node.exitCode ?? node.signalCode}); see native-node.log`);
      }
      const response = await page.request.get("/v1/hosts");
      const body = await response.json() as { items: { hostId: string; labels?: string[]; online?: boolean }[] };
      const host = body.items.find((row) => row.labels?.includes("test=promoted-hooks") && row.online);
      hostId = host?.hostId ?? "";
      return hostId;
    }, { timeout: 90_000 }).not.toBe("");
    const workspaces = await (await page.request.get(`/v1/hosts/${hostId}/workspaces`)).json();
    const created = await page.request.post("/v1/instances", {
      headers: { Origin: new URL(page.url()).origin },
      data: { hostId, workspaceId: workspaces.workspaces[0].workspaceId, cwd: workspace,
        kind: "terminal", driver: "shell-pty", name: "promoted-hook-e2e" },
    });
    expect(created.ok()).toBe(true);
    const result = await created.json();
    instanceId = result.instance.instanceId ?? result.instance.id;
    expect(instanceId).toBeTruthy();
    const id = instanceId!;
    await page.goto(`/s/${id}/tty`);
    const session = page.getByTestId("session-page");
    const assertActivity = async (activity: "working" | "idle", point: string) => {
      recordTiming("activity-assertion", { point, expected: activity, state: "start" });
      try {
        await expect(session).toHaveAttribute("data-activity", activity, { timeout: 1500 });
        recordTiming("activity-assertion", { point, expected: activity, state: "passed" });
      } catch (error) {
        recordTiming("activity-assertion", { point, expected: activity, state: "failed" });
        throw error;
      } finally {
        const state = await session.evaluate((element) => ({
          activity: element.getAttribute("data-activity"),
          journal: element.getAttribute("data-journal"),
          seq: element.querySelector("header")?.textContent?.match(/seq\s+(\d+)/)?.[1] ?? null,
        }), undefined, { timeout: 1000 }).catch(() => ({ unavailable: true }));
        recordTiming("browser-state", { point, ...state });
      }
    };
    const writeKeys = async (part: string, text: string) => {
      recordTiming("input-request", { part, state: "start" });
      try {
        await rawKeys(page, id, text);
        recordTiming("input-request", { part, state: "completed" });
      } catch (error) {
        recordTiming("input-request", { part, state: "failed" });
        throw error;
      }
    };
    await expect(session).toHaveAttribute("data-lifecycle", "running", { timeout: 20_000 });
    await expect(page.locator("[data-tty-lab='1']")).toHaveAttribute("data-tty-status", "live", { timeout: 20_000 });
    await writeKeys("launch", `claude --kind claude --settings ${quote(settingsFile)} --home ${quote(claudeHome)} --script ${quote(scriptFile)} --events-out ${quote(eventsFile)}\r`);
    await expect(session).toHaveAttribute("data-mode", "promoted", { timeout: 20_000 });
    await expect.poll(async () => {
      const record = await (await page.request.get(`/v1/instances/${id}`)).json();
      return record.signalTier;
    }, { timeout: 10_000 }).toBe("hook");
    expect(await (await page.request.get(`/v1/instances/${id}`)).json()).toMatchObject({
      driver: "shell-pty", kind: "claude", mode: "promoted", launchedBy: "user", signalTier: "hook",
    });
    await page.getByTestId("view-switch-structured").click();

    let turn = 0;
    const submit = async (prompt: string) => {
      turn += 1;
      await writeKeys(`turn-${turn}-body`, prompt);
      // The harness deliberately treats body+CR in one read as paste. A
      // separate command/read tests the same submission boundary as a TUI.
      await new Promise((resolve) => setTimeout(resolve, 100));
      await writeKeys(`turn-${turn}-cr`, "\r");
    };
    await submit("COMPLETE");
    await assertActivity("working", "complete-start");
    await expect(page.getByTestId("composer-interrupt")).toBeVisible();
    await expect.poll(async () => (await nativeEvents(eventsFile)).some((event) => event.event === "turn_end"),
      { timeout: 15_000 }).toBe(true);
    await assertActivity("idle", "complete-end");
    const hooks = (await journal(page, id)).filter((event) => event.source?.channel === "hook")
      .map((event) => event.payload?.nativeName);
    expect(hooks).toEqual(expect.arrayContaining(["SessionStart", "UserPromptSubmit", "PreToolUse", "MessageDisplay", "Stop"]));

    // Reusing the same session requires a fresh native confirmation each time;
    // the first interruption already present in the journal cannot settle the second.
    for (const count of [1, 2]) {
      await submit("INTERRUPT");
      await assertActivity("working", `interrupt-${count}-start`);
      const interrupt = page.getByTestId("composer-interrupt");
      await expect(interrupt).toBeVisible();
      await expect(interrupt).toBeEnabled({ timeout: 5000 });
      page.once("dialog", (dialog) => { void dialog.accept(); });
      await interrupt.click();
      await expect.poll(async () => (await nativeEvents(eventsFile)).filter((event) => event.event === "interrupt"),
        { timeout: 5000 }).toEqual(Array.from({ length: count }, () => expect.objectContaining({ by: "esc" })));
      await expect.poll(async () => (await journal(page, id))
        .filter((event) => event.kind === "lifecycle" && event.payload?.nativeName === "interrupted").length,
        { timeout: 5000 }).toBe(count);
      await assertActivity("idle", `interrupt-${count}-end`);
      await expect(session).toHaveAttribute("data-lifecycle", "running");
      expect((await nativeEvents(eventsFile)).some((event) => event.event === "exit")).toBe(false);
    }
    const screenshot = testInfo.outputPath("promoted-claude-interrupted.png");
    await page.screenshot({ path: screenshot, animations: "disabled" });
    await testInfo.attach("promoted-claude-interrupted", { path: screenshot, contentType: "image/png" });
    // Native liveness, not just a stale lifecycle projection: the same harness
    // accepts and completes a fresh turn after both interrupts.
    await submit("AFTER_INTERRUPT");
    await expect.poll(async () => (await nativeEvents(eventsFile)).filter((event) => event.event === "turn_end").length,
      { timeout: 10_000 }).toBe(2);
    await assertActivity("idle", "after-interrupt-end");
  } finally {
    if (instanceId) {
      try {
        const rows = await journalRows(page, instanceId);
        const snapshot = {
          instance: await (await page.request.get(`/v1/instances/${instanceId}`)).json(),
          journal: rows.map((row) => row.event),
          journalRows: rows,
          nativeEvents: await nativeEvents(eventsFile),
        };
        const snapshotPath = testInfo.outputPath("native-evidence.json");
        await writeFile(snapshotPath, JSON.stringify(snapshot, null, 2).replaceAll(dir, "$TEST_DIR"));
        await testInfo.attach("native-evidence.json", { path: snapshotPath, contentType: "application/json" });
      } catch {
        nodeLog += "Could not capture final instance evidence before cleanup.\n";
      }
    }
    if (instanceId) await command(page, instanceId, "instance.close").catch(() => {});
    if (node) await stopNode(node);
    const timingPath = testInfo.outputPath("native-timing.json");
    await writeFile(timingPath, JSON.stringify(timing, null, 2));
    await testInfo.attach("native-timing.json", { path: timingPath, contentType: "application/json" });
    const logPath = testInfo.outputPath("native-node.log");
    await writeFile(logPath, nodeLog.replaceAll(enrollToken || "__unused__", "[redacted]")
      .replaceAll(dir, "$TEST_DIR").replaceAll(process.env.HOME || "__unused__", "$HOME")
      .replaceAll(hostname(), "test-host"));
    await testInfo.attach("native-node.log", { path: logPath, contentType: "text/plain" });
    await rm(dir, { recursive: true, force: true });
  }
});
