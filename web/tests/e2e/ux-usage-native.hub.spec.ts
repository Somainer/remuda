/**
 * c-ctxusage r6 item 1: native transcript usage drives the context chip
 * THROUGH the owner-reported path — a shell terminal where the user types
 * `claude --kind claude … --script …`, the promoter binds the foreground
 * process and hydrates the real fake-harness transcript (2.1.289 dialect
 * with a split cache_creation object).
 *
 * Before the fix the instance was a plain `/bin/sh` running NATIVEUSAGE\n
 * (which launched nothing), and every failure became a permanent test.skip,
 * so the chip/popover the owner reported was never exercised.
 *
 * Context basket (input + cache read + cache creation) = 50%:
 *   3,000 + 94,000 + (2,000 5m + 1,000 1h) = 100,000 of the 200k fallback.
 */

import { execFile, spawn, type ChildProcess } from "node:child_process";
import { promisify } from "node:util";
import {
  access,
  chmod,
  copyFile,
  mkdtemp,
  mkdir,
  open,
  realpath,
  rm,
  writeFile,
} from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { expect, test, type Page } from "@playwright/test";

const execFileAsync = promisify(execFile);

const root = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../..",
);

const target = process.env.CARGO_TARGET_DIR
  ? path.resolve(process.env.CARGO_TARGET_DIR)
  : path.resolve(root, process.env.CARGO_TARGET_DIR_REL ?? "target");
const remuda =
  process.env.HUB_E2E_REMUDA_BIN ?? path.join(target, "debug/remuda");
const harness =
  process.env.HUB_E2E_FAKE_HARNESS_BIN ??
  path.join(target, "debug/fake-harness");
const nativeNode =
  process.env.HUB_E2E_NATIVE_NODE_BIN ??
  path.join(target, "debug/examples/native_hub_e2e");

test.beforeAll(
  async () => {
    const args = ["build", "--locked"];
    if (!process.env.HUB_E2E_REMUDA_BIN)
      args.push("-p", "remuda", "--bin", "remuda");
    if (!process.env.HUB_E2E_FAKE_HARNESS_BIN) {
      args.push("-p", "remuda-testing", "--bin", "fake-harness");
    }
    if (!process.env.HUB_E2E_NATIVE_NODE_BIN) {
      args.push("-p", "remuda-node", "--example", "native_hub_e2e");
    }
    await execFileAsync("cargo", args, {
      cwd: root,
      env: process.env,
      timeout: 570_000,
      maxBuffer: 8 * 1024 * 1024,
    });
    await Promise.all([access(remuda), access(harness), access(nativeNode)]);
  },
  { timeout: 600_000 },
);

// A real self-skip: when the three fixture binaries are missing AND the test
// was launched without explicit prebuilt overrides. There is no catch->skip
// around the native path itself.
test.skip(async () => {
  for (const bin of [remuda, harness, nativeNode]) {
    try {
      await access(bin);
    } catch {
      return true;
    }
  }
  return false;
});

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
  if (node.exitCode !== null || node.signalCode !== null) return;
  node.kill("SIGTERM");
  await new Promise<void>((resolve) => {
    const exited = () => {
      node.once("exit", () => resolve());
    };
    const timer = setTimeout(() => {
      node.kill("SIGKILL");
      resolve();
    }, 10_000);
    exited();
    node.once("exit", () => clearTimeout(timer));
  });
}

async function login(page: Page, bootstrap: string) {
  const body = JSON.stringify({
    bootstrapToken: bootstrap,
    deviceName: "usage-native-e2e",
  });
  let cookie = "";
  for (let i = 0; i < 60; i++) {
    const response = await page.request.post("/v1/login", {
      headers: {
        Origin: new URL(page.url()).origin,
        "Content-Type": "application/json",
      },
      data: body,
    });
    const setCookie = response.headers()["set-cookie"];
    if (setCookie) {
      cookie = setCookie.split(";")[0];
      break;
    }
    await page.waitForTimeout(1_000);
  }
  expect(cookie).toBeTruthy();
}

async function command(page: Page, instanceId: string, payload: object) {
  const response = await page.request.post(
    `/v1/instances/${instanceId}/commands`,
    {
      headers: { Origin: new URL(page.url()).origin },
      data: payload,
    },
  );
  expect(response.ok(), `${response.status()}: ${await response.text()}`).toBe(
    true,
  );
}

async function rawKeys(page: Page, instanceId: string, text: string) {
  await command(page, instanceId, {
    operation: "tty.write",
    payload: { dataBase64: Buffer.from(text).toString("base64"), source: "ui" },
  });
}

function quote(value: string) {
  return `'${value.replaceAll("'", "'\\''")}'`;
}

test("native transcript usage drives the context chip through the real promoted path", async ({
  page,
}) => {
  // mkdtemp requires its parent to exist already.
  const scratch = path.join(target, "hub-e2e", "usage-native");
  await mkdir(scratch, { recursive: true });
  const dir = await realpath(
    await mkdtemp(path.join(scratch, "usage-native-")),
  );
  const dataDir = path.join(dir, "data");
  await mkdir(dataDir, { recursive: true });
  const workspace = path.join(dir, "workspace");
  const bin = path.join(dir, "bin");
  const claudeHome = path.join(dir, "claude-home");
  const scriptFile = path.join(dir, "scenario.json");
  const eventsFile = path.join(dir, "native-events.jsonl");
  const tokenFile = path.join(dir, "enroll-token");
  const shell = path.join(bin, "test-shell");
  await Promise.all([
    mkdir(dataDir),
    mkdir(workspace),
    mkdir(bin),
    mkdir(claudeHome),
  ]);
  await copyFile(harness, path.join(bin, "claude"));
  await chmod(path.join(bin, "claude"), 0o700);
  await writeFile(
    shell,
    '#!/bin/sh\nif [ "${1:-}" = --version ]; then exec /bin/sh --version; fi\nexec /bin/sh\n',
    { mode: 0o700 },
  );
  await writeFile(scriptFile, JSON.stringify(SCENARIO));

  const hub = new URL(
    process.env.VITE_HUB_URL ??
      `http://127.0.0.1:${process.env.HUB_E2E_LISTEN ?? "58880"}`,
  );
  hub.protocol = hub.protocol === "https:" ? "wss" : "ws";
  hub.pathname = "/v1/node";

  // Linux uses the /proc/self/fd alias (same as promoted-claude.spec.ts): the
  // native example canonicalizes paths and needs the handle kept open.
  const dataHandle =
    process.platform === "linux" ? await open(dataDir, "r") : undefined;
  const dataPath =
    process.platform === "linux"
      ? `/proc/${process.pid}/fd/${dataHandle!.fd}`
      : dataDir;

  const nodeLog = path.join(dir, "node.log");
  let node: ChildProcess | undefined;
  let instanceId: string | undefined;
  try {
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
        REMUDA_HERDR_ORPHAN_SWEEP: "0",
        SHELL: shell,
        PATH: `${bin}:/usr/bin:/bin:/usr/sbin:/sbin`,
      },
    });
    node.stdout?.on("data", (chunk) => {
      const text = String(chunk);
      if (process.env.HUB_E2E_LOG === "1") process.stdout.write(text);
    });
    node.stderr?.on("data", (chunk) => process.stderr.write(chunk));
    void nodeLog;

    let hostId = "";
    await expect
      .poll(
        async () => {
          const response = await page.request.get("/v1/hosts");
          const body = (await response.json()) as {
            items?: { hostId: string; labels?: string[]; online?: boolean }[];
          };
          const host = body.items?.find(
            (row) => row.labels?.includes("test=promoted-hooks") && row.online,
          );
          hostId = host?.hostId ?? "";
          return hostId;
        },
        { timeout: 90_000, intervals: [200, 500, 1000] },
      )
      .not.toBe("");

    const minted = await page.request.post("/v1/hosts/enroll-token", {
      headers: { Origin: new URL(page.url()).origin },
    });
    await login(page, ((await minted.json()) as { token: string }).token);

    const workspacesResponse = await page.request.get(
      `/v1/hosts/${hostId}/workspaces`,
    );
    const workspaces = (await workspacesResponse.json()) as {
      workspaces: { workspaceId: string }[];
    };
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
    expect(created.ok(), `${created.status()}: ${await created.text()}`).toBe(
      true,
    );

    const id = instanceId!;
    await page.goto(`/s/${id}`);
    await expect(page.getByTestId("session-page")).toHaveAttribute(
      "data-lifecycle",
      "running",
      { timeout: 20_000 },
    );

    const settingsFile = path.join(dir, "user-settings.json");
    // Launch the harness EXACTLY as a promoted Claude session (the
    // promoted-claude.spec.ts sequence): launch command first, wait for
    // data-mode=promoted, then send prompt body and CR as separate writes.
    await rawKeys(
      page,
      id,
      `claude --kind claude --home ${quote(claudeHome)} --settings ${quote(
        settingsFile,
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

    // The owner-reported chip: 100k of 200k.
    const chip = page.getByTestId("context-chip");
    await expect(chip).toHaveText("50%", { timeout: 60_000 });

    await chip.click();
    const popover = page.getByTestId("context-usage-popover");
    await expect(popover).toBeVisible();
    // Non-zero split cache creation (r6: was fixed at zero in the old skip)
    // and the cached-read bucket the chip math includes.
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
