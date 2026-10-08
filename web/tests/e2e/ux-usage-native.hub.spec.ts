/**
 * c-ctxusage r5 item 5b: ONE path through a NATIVE transcript (not the
 * `usage:` sentinel). Spawns the production `native_hub_e2e` example Node
 * (label e2e-native-hooks) backed by the fake-harness 2.1.289 dialect, creates
 * a promoted shell-pty Claude session whose scenario writes a real transcript
 * assistant record carrying usage, and asserts the chip/popover surface that
 * native usage — the same chip the sentinel-only spec covers, but proven
 * end-to-end through the TranscriptMapper -> journal -> Hub rollup pipeline.
 *
 * Self-skips when the three binaries are not built.
 */
import { expect, test } from "@playwright/test";
import { execFile, spawn, type ChildProcess } from "node:child_process";
import {
  access,
  chmod,
  copyFile,
  mkdir,
  mkdtemp,
  realpath,
  rm,
  writeFile,
} from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { login } from "./hub-auth";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..");
const target = path.resolve(root, process.env.CARGO_TARGET_DIR ?? "target");
const remuda = process.env.HUB_E2E_REMUDA_BIN ?? path.join(target, "debug/remuda");
const harness = process.env.HUB_E2E_FAKE_HARNESS_BIN ?? path.join(target, "debug/fake-harness");
const nativeNode = process.env.HUB_E2E_NATIVE_NODE_BIN ?? path.join(target, "debug/examples/native_hub_e2e");

test.beforeAll(async ({}, testInfo) => {
  testInfo.setTimeout(600_000);
  const args = ["build", "--locked"];
  if (!process.env.HUB_E2E_REMUDA_BIN) args.push("-p", "remuda", "--bin", "remuda");
  if (!process.env.HUB_E2E_FAKE_HARNESS_BIN) args.push("-p", "remuda-testing", "--bin", "fake-harness");
  if (!process.env.HUB_E2E_NATIVE_NODE_BIN) args.push("-p", "remuda-node", "--example", "native_hub_e2e");
  if (args.length > 2) {
    await promisify(execFile)("cargo", args, {
      cwd: root, env: process.env, timeout: 570_000, maxBuffer: 8 * 1024 * 1024,
    });
  }
  await Promise.all([access(remuda), access(harness), access(nativeNode)]);
});

// A single-turn scenario: a slow auto tool keeps the process foreground while
// the promoter binds; the assistant record carries 2.1.289 split usage whose
// context (3_000 + 97_000 = 100_000 of the 200k kind fallback) is 50%.
const SCENARIO = {
  turns: [
    {
      match_prefix: "NATIVEUSAGE",
      text: "NATIVEUSAGE_DONE",
      chunks: 1,
      tools: [
        {
          name: "Bash",
          input: { command: "sleep 3" },
          approval: "auto",
          duration_ms: 3000,
          exit_code: 0,
        },
      ],
      stop_reason: "end_turn",
      usage: {
        input_tokens: 3000,
        output_tokens: 40,
        cached_tokens: 97_000,
        cache_creation_5m: 0,
        cache_creation_1h: 0,
      },
    },
  ],
};

async function stopNode(node: ChildProcess) {
  if (!node.pid || node.exitCode !== null || node.signalCode !== null) return;
  const exited = new Promise<void>((resolve) => node.once("exit", () => resolve()));
  node.kill("SIGTERM");
  const timer = setTimeout(() => node.kill("SIGKILL"), 10_000);
  await exited;
  clearTimeout(timer);
}

test("native transcript usage drives the context chip through the real rollup", async ({ page }) => {
  test.skip((await access(nativeNode).then(() => false).catch(() => true)), "native_hub_e2e not built");
  test.setTimeout(180_000);
  const scratch = path.join(target, "hub-e2e");
  await mkdir(scratch, { recursive: true });
  const dir = await realpath(await mkdtemp(path.join(scratch, "usage-native-")));
  const dataDir = path.join(dir, "data");
  const bin = path.join(dir, "bin");
  const workspace = path.join(dir, "workspace");
  const claudeHome = path.join(dir, "claude-home");
  const scriptFile = path.join(dir, "scenario.json");
  const tokenFile = path.join(dir, "enroll-token");
  const shell = path.join(bin, "test-shell");
  let node: ChildProcess | undefined;
  let instanceId: string | undefined;
  let nodeLog = "";
  try {
    await Promise.all([mkdir(dataDir), mkdir(bin), mkdir(workspace), mkdir(claudeHome)]);
    await copyFile(harness, path.join(bin, "claude"));
    await chmod(path.join(bin, "claude"), 0o700);
    await writeFile(
      shell,
      "#!/bin/sh\nif [ \"${1:-}\" = --version ]; then exec /bin/sh --version; fi\nexec /bin/sh\n",
      { mode: 0o700 },
    );
    await writeFile(scriptFile, JSON.stringify(SCENARIO));

    await login(page, "e2e-native-hooks");
    const minted = await page.request.post("/v1/hosts/enroll-token", {
      headers: { Origin: new URL(page.url()).origin },
    });
    expect(minted.ok()).toBe(true);
    await writeFile(tokenFile, (await minted.json()).token, { mode: 0o600 });

    const hub = new URL(process.env.VITE_HUB_URL ?? `http://${process.env.HUB_E2E_LISTEN ?? "127.0.0.1:58880"}`);
    hub.protocol = hub.protocol === "https:" ? "wss:" : "ws:";
    hub.pathname = "/v1/node";
    node = spawn(nativeNode, [], {
      cwd: dir,
      stdio: ["ignore", "pipe", "pipe"],
      env: {
        ...process.env,
        REMUDA_DATA_DIR: dataDir,
        HUB_E2E_NODE_HUB_URL: hub.toString(),
        HUB_E2E_NODE_TOKEN_FILE: tokenFile,
        HUB_E2E_NODE_WORKSPACE: workspace,
        HUB_E2E_NODE_DATA_DIR: dataDir,
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
    node.stdout?.on("data", (chunk) => (nodeLog += String(chunk)));
    node.stderr?.on("data", (chunk) => (nodeLog += String(chunk)));

    let hostId = "";
    await expect
      .poll(
        async () => {
          if (node && node.exitCode !== null) {
            throw new Error(`Native Node exited (${node.exitCode ?? node.signalCode}); log: ${nodeLog.slice(-500)}`);
          }
          const body = await (await page.request.get("/v1/hosts")).json() as {
            items: { hostId: string; labels?: string[]; online?: boolean }[];
          };
          hostId = body.items.find((row) => row.labels?.includes("test=promoted-hooks") && row.online)?.hostId ?? "";
          return hostId;
        },
        { timeout: 90_000 },
      )
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
        name: "native-usage",
        tui: "default",
      },
    });
    const result = await created.json();
    expect(created.ok(), `create ${created.status()}: ${JSON.stringify(result)}`).toBe(true);
    instanceId = result.instance.instanceId ?? result.instance.id;
    const id = instanceId!;

    await page.goto(`/s/${id}`);
    await expect(page.getByTestId("session-page")).toHaveAttribute("data-lifecycle", "running", { timeout: 20_000 });
    // The Hub HTTP command API wraps params in {dataBase64, source}; the WSS
    // node's keys_from_params also reads instanceId from the params (it is
    // injected by the Hub alongside commandId). Send a standalone write after
    // the session settles, then poll for the chip.
    await expect.poll(async () => {
      const response = await page.request.post(`/v1/instances/${id}/commands`, {
        headers: { Origin: new URL(page.url()).origin },
        data: {
          operation: "tty.write",
          payload: { dataBase64: Buffer.from("NATIVEUSAGE\n").toString("base64"), source: "ui" },
        },
      });
      return response.status();
    }, { timeout: 30_000, intervals: [200, 500, 1000] }).toBe(200);

    // The chip renders the native transcript usage: 100k of 200k = 50%.
    await expect(page.getByTestId("context-chip")).toHaveText("50%", { timeout: 60_000 });
    const chip = page.getByTestId("context-chip");
    await chip.click();
    const popover = page.getByTestId("context-usage-popover");
    await expect(popover).toBeVisible();
    await expect(popover).toContainText("97,000");
    await expect(popover).toContainText("3,000");
  } finally {
    if (instanceId) {
      await page.request.delete(`/v1/instances/${instanceId}?force=1`).catch(() => {});
    }
    if (node) await stopNode(node);
    await rm(dir, { recursive: true, force: true }).catch(() => {});
  }
});
