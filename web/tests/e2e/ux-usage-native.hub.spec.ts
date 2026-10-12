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

import {
  parseBuiltExecutables,
  resolveBuiltBinary,
  type BuiltBinary,
} from "../../cargo-artifacts";
import { login } from "./hub-auth";

const root = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../..",
);
const target = process.env.CARGO_TARGET_DIR
  ? path.resolve(process.env.CARGO_TARGET_DIR)
  : path.resolve(root, "target");

/** Binaries the hook builds itself vs. operator-supplied overrides. The two
 * sets are resolved differently (r10 item 6): a built-but-missing artifact is
 * a hook failure; only an external override may ENOENT-skip. */
const BIN_SPECS: {
  binary: BuiltBinary;
  override: string | undefined;
}[] = [
  { binary: { label: "remuda", relPath: "remuda" }, override: process.env.HUB_E2E_REMUDA_BIN },
  {
    binary: { label: "fake-harness", relPath: "fake-harness" },
    override: process.env.HUB_E2E_FAKE_HARNESS_BIN,
  },
  {
    binary: { label: "native node", relPath: path.join("examples", "native_hub_e2e") },
    override: process.env.HUB_E2E_NATIVE_NODE_BIN,
  },
];

// Resolved after beforeAll; consumed by the scenarios below.
const bins: Record<string, string> = {};

// Refresh the three executables so a standalone gate run uses this tree.
test.beforeAll(async () => {
  test.setTimeout(600_000);
  // Take the executable paths from THIS bounded build: --message-format=json
  // makes cargo report each compiler-artifact.executable itself, so a stale
  // target/debug binary can never mask a missing fresh output under the
  // selected target triple (and the selected triple, not the host triple,
  // is what gets used). No post-build cargo/rustc probes - they ran
  // unbounded and could hang the Playwright worker (c-ctxusage r11 item 2).
  const args = ["build", "--locked", "--message-format=json"];
  for (const spec of BIN_SPECS) {
    if (spec.override) {
      continue;
    }
    if (spec.binary.relPath === "remuda") {
      args.push("-p", "remuda", "--bin", "remuda");
    } else if (spec.binary.relPath === "fake-harness") {
      args.push("-p", "remuda-testing", "--bin", "fake-harness");
    } else {
      args.push("-p", "remuda-node", "--example", "native_hub_e2e");
    }
  }
  let built = new Map<string, string>();
  if (args.length > 3) {
    const { stdout } = await promisify(execFile)("cargo", args, {
      cwd: root,
      env: process.env,
      timeout: 570_000,
      maxBuffer: 256 * 1024 * 1024,
    });
    built = parseBuiltExecutables(stdout);
  }
  // Runs AFTER the build. A binary the hook did NOT just build (an operator
  // override env var) may legitimately be absent on a fixture-less host: ENOENT
  // on that path is the real missing-prerequisite SKIP; every other access
  // error still surfaces. A binary the hook DID just build is resolved against
  // cargo's own reported artifact path and a miss there THROWS - a failed
  // build or wrong path resolution must fail the hook, never masquerade as a
  // skip (c-ctxusage r10 item 6, r11 item 2).
  const missing: string[] = [];
  for (const spec of BIN_SPECS) {
    if (spec.override) {
      try {
        await access(spec.override);
      } catch (error) {
        if ((error as NodeJS.ErrnoException | undefined)?.code === "ENOENT") {
          missing.push(spec.binary.label);
          continue;
        }
        throw error;
      }
      bins[spec.binary.label] = spec.override;
    } else {
      bins[spec.binary.label] = resolveBuiltBinary(built, spec.binary);
    }
  }
  test.skip(
    missing.length > 0,
    `native usage e2e binaries missing (operator override paths): ${missing.join(", ")}`,
  );
});

/** Resolved remuda/fake-harness/native-node paths for the spawned processes. */
function builtBin(label: "remuda" | "fake-harness" | "native node"): string {
  const resolved = bins[label];
  if (!resolved) {
    throw new Error(`${label} binary was not resolved before the test ran`);
  }
  return resolved;
}

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
    await copyFile(builtBin("fake-harness"), path.join(bin, "claude"));
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
    node = spawn(builtBin("native node"), [], {
      cwd: dir,
      stdio: ["ignore", "pipe", "pipe"],
      env: {
        ...process.env,
        REMUDA_DATA_DIR: dataPath,
        HUB_E2E_NODE_HUB_URL: hub.toString(),
        HUB_E2E_NODE_TOKEN_FILE: tokenFile,
        HUB_E2E_NODE_WORKSPACE: workspace,
        HUB_E2E_NODE_DATA_DIR: dataPath,
        HUB_E2E_REMUDA_BIN: builtBin("remuda"),
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
    // Wait for the prompt body to be read and painted BEFORE the CR: a body
    // and CR landing in one harness PTY read are treated as a paste, so the
    // two writes must be separated by an observed condition, not a fixed
    // delay. The renderer-independent xterm buffer is that observation - the
    // draft is visible there regardless of canvas/webgl/DOM rendering.
    await expect
      .poll(
        async () =>
          (
            await page.evaluate(() => window.__ttyLab?.bufferText() ?? "")
          ).includes("NATIVEUSAGE probe"),
        { timeout: 20_000, intervals: [100, 250] },
      )
      .toBe(true);
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
