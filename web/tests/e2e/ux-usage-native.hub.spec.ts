/**
 * c-ctxusage r6/r7 item 1: native transcript usage drives the context chip
 * THROUGH the owner-reported path — a shell terminal where the user runs the
 * fake harness as a promoted `claude --kind claude … --script …`, the shell
 * promoter binds the foreground process and hydrates its real transcript,
 * and the Hub rollup derives contextUsedTokens from the 2.1.289 split-cache
 * shape (input + cache read + cache creation).
 *
 * Setup mirrors promoted-claude.hub.spec.ts exactly (bootstrap login,
 * pre-written node enrollment token, REMUDA_CLAUDE_BIN /
 * REMUDA_CLAUDE_CONFIG_DIR, existing data dir, proc-fs alias, node-exited
 * check inside the host poll, host:port Hub URL).
 */

import { spawn, type ChildProcess } from "node:child_process";
import { promisify } from "node:util";
import { execFile } from "node:child_process";
import {
  access,
  chmod,
  copyFile,
  mkdir,
  mkdtemp,
  open,
  realpath,
  rm,
  stat,
  writeFile,
} from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { expect, test, type Page } from "@playwright/test";

import { login } from "./hub-auth";

const root = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../..",
);
const target = process.env.CARGO_TARGET_DIR
  ? path.resolve(process.env.CARGO_TARGET_DIR)
  : path.resolve(root, "target");
const remuda =
  process.env.HUB_E2E_REMUDA_BIN ?? path.join(target, "debug/remuda");
const harness =
  process.env.HUB_E2E_FAKE_HARNESS_BIN ??
  path.join(target, "debug/fake-harness");
const nativeNode =
  process.env.HUB_E2E_NATIVE_NODE_BIN ??
  path.join(target, "debug/examples/native_hub_e2e");

// Refresh the three executables so a standalone gate run uses this tree.
test.beforeAll(async () => {
  test.setTimeout(600_000);
  const args = ["build", "--locked"];
  if (!process.env.HUB_E2E_REMUDA_BIN)
    args.push("-p", "remuda", "--bin", "remuda");
  if (!process.env.HUB_E2E_FAKE_HARNESS_BIN) {
    args.push("-p", "remuda-testing", "--bin", "fake-harness");
  }
  if (!process.env.HUB_E2E_NATIVE_NODE_BIN) {
    args.push("-p", "remuda-node", "--example", "native_hub_e2e");
  }
  if (args.length > 2) {
    await promisify(execFile)("cargo", args, {
      cwd: root,
      env: process.env,
      timeout: 570_000,
      maxBuffer: 8 * 1024 * 1024,
    });
  }
  // Runs AFTER the build: on a fixture-less host binaries genuinely do not
  // exist, so this is a real missing-bin skip, not a permanent skip.
  await Promise.all([access(remuda), access(harness), access(nativeNode)]);
});

// 3_000 + 94_000 + (2_000 5m + 1_000 1h) = 100_000 of the 200k fallback
// window → the chip reads exactly 50%.
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
          duration_ms: 3_000,
          exit_code: 0,
        },
      ],
      stop_reason: "end_turn",
      usage: {
        input_tokens: 3_000,
        output_tokens: 40,
        cached_tokens: 94_000,
        cache_creation_5m: 2_000,
        cache_creation_1h: 1_000,
      },
    },
  ],
};

async function stopNode(node: ChildProcess) {
  if (!node.pid || node.exitCode !== null || node.signalCode !== null) return;
  const exited = new Promise<void>((resolve) =>
    node.once("exit", () => resolve()),
  );
  node.kill("SIGTERM");
  const timer = setTimeout(() => node.kill("SIGKILL"), 10_000);
  await exited;
  clearTimeout(timer);
}

async function command(
  page: Page,
  instanceId: string,
  operation: string,
  payload: object = {},
) {
  const response = await page.request.post(
    `/v1/instances/${instanceId}/commands`,
    {
      headers: { Origin: new URL(page.url()).origin },
      data: { operation, payload },
    },
  );
  expect(response.ok()).toBe(true);
}

async function rawKeys(page: Page, instanceId: string, text: string) {
  await command(page, instanceId, "tty.write", {
    dataBase64: Buffer.from(text).toString("base64"),
    source: "ui",
  });
}

function quote(value: string): string {
  return `'${value.replaceAll("'", "'\\''")}'`;
}

test("native transcript usage drives the context chip through the real promoted path", async ({
  page,
}) => {
  test.setTimeout(180_000);
  const scratch = path.join(target, "hub-e2e");
  await mkdir(scratch, { recursive: true });
  const dir = await realpath(
    await mkdtemp(path.join(scratch, "usage-native-")),
  );
  const dataDir = path.join(dir, "data");
  await mkdir(dataDir);
  const dataStat = await stat(dataDir);
  const dataHandle =
    process.platform === "linux" ? await open(dataDir, "r") : undefined;
  const dataPath =
    process.platform === "darwin"
      ? `/.vol/${dataStat.dev}/${dataStat.ino}`
      : dataHandle
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

  try {
    await Promise.all([mkdir(bin), mkdir(workspace), mkdir(claudeHome)]);
    await copyFile(harness, path.join(bin, "claude"));
    await chmod(path.join(bin, "claude"), 0o700);
    await writeFile(
      shell,
      '#!/bin/sh\nif [ "${1:-}" = --version ]; then exec /bin/sh --version; fi\nexec /bin/sh\n',
      { mode: 0o700 },
    );
    await writeFile(
      settingsFile,
      JSON.stringify({ env: { NATIVE_USAGE_SPEC: "1" } }),
    );
    await writeFile(scriptFile, JSON.stringify(SCENARIO));

    // Bootstrap session (Hub access) FIRST, then mint a host enroll token and
    // write it to the file the node reads on startup.
    await login(page, "e2e-native-usage");
    const minted = await page.request.post("/v1/hosts/enroll-token", {
      headers: { Origin: new URL(page.url()).origin },
    });
    expect(minted.ok()).toBe(true);
    const enrollToken = (await minted.json()).token as string;
    await writeFile(tokenFile, enrollToken, { mode: 0o600 });

    // host:port, not a bare port.
    const hub = new URL(
      process.env.VITE_HUB_URL ??
        `http://${process.env.HUB_E2E_LISTEN ?? "127.0.0.1:58880"}`,
    );
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
        REMUDA_CLAUDE_BIN: path.join(bin, "claude"),
        REMUDA_CLAUDE_CONFIG_DIR: claudeHome,
        CLAUDE_CONFIG_DIR: claudeHome,
        REMUDA_PTY_EMULATOR: "1",
        REMUDA_PTY_HOOKS: "1",
        REMUDA_SHIM: "on",
        SHELL: shell,
        PATH: `${bin}:/usr/bin:/bin:/usr/sbin:/sbin`,
        REMUDA_HERDR_ORPHAN_SWEEP: "0",
      },
    });
    node.stderr?.on("data", (chunk) => process.stderr.write(chunk));

    let hostId = "";
    await expect
      .poll(
        async () => {
          if (node && node.exitCode !== null) {
            throw new Error(
              `native node exited before enrollment (${node.exitCode})`,
            );
          }
          const response = await page.request.get("/v1/hosts");
          const body = (await response.json()) as {
            items: {
              hostId: string;
              labels?: string[];
              online?: boolean;
            }[];
          };
          const host = body.items.find(
            (row) => row.labels?.includes("test=promoted-hooks") && row.online,
          );
          hostId = host?.hostId ?? "";
          return hostId;
        },
        { timeout: 90_000, intervals: [200, 500, 1_000] },
      )
      .not.toBe("");

    const workspaces = (await (
      await page.request.get(`/v1/hosts/${hostId}/workspaces`)
    ).json()) as { workspaces: { workspaceId: string }[] };
    const workspaceId = workspaces.workspaces[0].workspaceId;

    const created = await page.request.post("/v1/instances", {
      headers: { Origin: new URL(page.url()).origin },
      data: {
        hostId,
        workspaceId,
        cwd: workspace,
        kind: "terminal",
        driver: "shell-pty",
        tui: "default",
        name: "native-usage",
      },
    });
    const result = (await created.json()) as {
      instance: { instanceId?: string; id?: string };
    };
    instanceId = result.instance.instanceId ?? result.instance.id;
    expect(
      created.ok(),
      `instance create ${created.status()}: ${JSON.stringify(result)}`,
    ).toBe(true);

    const id = instanceId!;
    await page.goto(`/s/${id}`);
    await expect(page.getByTestId("session-page")).toHaveAttribute(
      "data-lifecycle",
      "running",
      { timeout: 20_000 },
    );

    // Launch the harness as a promoted Claude session: launch command first,
    // wait for data-mode=promoted, then prompt body and CR as separate writes
    // (the harness treats one read with body+CR as paste).
    await rawKeys(
      page,
      id,
      `claude --kind claude --settings ${quote(settingsFile)} --home ${quote(
        claudeHome,
      )} --script ${quote(scriptFile)} --events-out ${quote(eventsFile)}\r`,
    );
    await expect(page.getByTestId("session-page")).toHaveAttribute(
      "data-mode",
      "promoted",
      { timeout: 20_000 },
    );

    await rawKeys(page, id, "NATIVEUSAGE probe");
    await page.waitForTimeout(100);
    await rawKeys(page, id, "\r");

    // The owner-reported chip: 100k of 200k = 50%.
    await expect(page.getByTestId("context-chip")).toHaveText("50%", {
      timeout: 60_000,
    });

    const chip = page.getByTestId("context-chip");
    await chip.click();
    const popover = page.getByTestId("context-usage-popover");
    await expect(popover).toBeVisible();
    await expect(popover).toContainText("94,000");
    await expect(popover).toContainText("3,000");
  } finally {
    if (instanceId) {
      await page.request
        .delete(`/v1/instances/${instanceId}?force=1`)
        .catch(() => {});
    }
    if (node) await stopNode(node);
    dataHandle?.close();
    await rm(dir, { recursive: true, force: true }).catch(() => {});
  }
});
