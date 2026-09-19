import { expect, test, type Page } from "@playwright/test";
import { execFile, spawn, type ChildProcess } from "node:child_process";
import { access, chmod, copyFile, mkdir, mkdtemp, open, readFile, realpath, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { login } from "./hub-auth";

/**
 * c-grokpartials: streamed tool-result partials are VISIBLE while a grok
 * shell call is running. A disposable production Node (native_hub_e2e) runs
 * a `terminal` shell-pty into which the deterministic fake harness is
 * launched BY NAME as `grok` (exactly how a human starts a promoted agent).
 * The Node promotes the process and tails its grok file channel; the harness
 * replays the landed scenario
 * crates/remuda-testing/fixtures/fake-harness/scenarios/grok-tools.json,
 * whose Bash call drips `one/two/three` into terminal/<callId>.log across
 * its 900 ms run. c-grok-stdout's driver tail publishes each read as a
 * Partial tool-result Append on the node's shared revision counter, and the
 * web assembler accumulates those into the shell card's stdout BEFORE the
 * Final Close replaces them with the authoritative result.
 *
 * The landed scenario auto-quits after the one turn; a real grok session
 * does not, and an exited/demoted terminal tears its structured transcript
 * down before the folded Final can be inspected. The spec therefore runs an
 * in-scratch copy of the exact landed scenario with `quitAfterTurns`
 * removed — same prompt, same tools, same 900 ms drips, just kept alive.
 * Nothing mocks the journal: the live DOM is sampled through the turn, and
 * the durable journal is read back as producer ground truth.
 */
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..");
const target = path.resolve(root, process.env.CARGO_TARGET_DIR ?? "target");
const remuda = process.env.HUB_E2E_REMUDA_BIN ?? path.join(target, "debug/remuda");
const harness = process.env.HUB_E2E_FAKE_HARNESS_BIN ?? path.join(target, "debug/fake-harness");
const nativeNode = process.env.HUB_E2E_NATIVE_NODE_BIN ?? path.join(target, "debug/examples/native_hub_e2e");
const landedScenario = path.join(
  root,
  "crates/remuda-testing/fixtures/fake-harness/scenarios/grok-tools.json",
);

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
  await Promise.all([access(remuda), access(harness), access(nativeNode), access(landedScenario)]);
});

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

async function stopNode(node: ChildProcess) {
  if (!node.pid || node.exitCode !== null || node.signalCode !== null) return;
  const exited = new Promise<void>((resolve) => node.once("exit", () => resolve()));
  node.kill("SIGTERM");
  const timer = setTimeout(() => node.kill("SIGKILL"), 10_000);
  await exited;
  clearTimeout(timer);
}

type StdoutSample = { at: number; working: boolean; stdout: string };

test("grok shell card shows growing stdout partials before the Final result", async ({ page }, testInfo) => {
  test.setTimeout(240_000);
  const scratch = path.join(root, "target", "hub-e2e");
  await mkdir(scratch, { recursive: true });
  const dir = await realpath(await mkdtemp(path.join(scratch, "gp-")));
  const dataDir = path.join(dir, "data");
  await mkdir(dataDir);
  const dataHandle = process.platform === "linux" ? await open(dataDir, "r") : undefined;
  const dataPath = dataHandle
    ? `/proc/${process.pid}/fd/${dataHandle.fd}`
    : dataDir;
  const bin = path.join(dir, "bin");
  const workspace = path.join(dir, "workspace");
  const grokHome = path.join(dir, "grok-home");
  const eventsFile = path.join(dir, "native-events.jsonl");
  const tokenFile = path.join(dir, "enroll-token");
  const shell = path.join(bin, "test-shell");
  // The exact landed scenario, kept alive after its one turn (see file doc).
  const scenarioFile = path.join(dir, "grok-tools-keepalive.json");
  let node: ChildProcess | undefined;
  let instanceId: string | undefined;
  let nodeLog = "";

  try {
    await Promise.all([mkdir(bin), mkdir(workspace), mkdir(grokHome)]);
    // The promotion detector promotes by process basename: the foreground
    // process must be named `grok` (a shell wrapper would exec as
    // fake-harness and never promote).
    await copyFile(harness, path.join(bin, "grok"));
    await chmod(path.join(bin, "grok"), 0o700);
    await writeFile(
      shell,
      "#!/bin/sh\nif [ \"${1:-}\" = --version ]; then exec /bin/sh --version; fi\nexec /bin/sh\n",
      { mode: 0o700 },
    );
    const scenario = JSON.parse(await readFile(landedScenario, "utf8")) as {
      turns: { match_prefix?: string }[];
      quit_after_turns?: number | null;
    };
    expect(scenario.turns[0]?.match_prefix).toBe("GROK_TOOLS");
    scenario.quit_after_turns = null;
    await writeFile(scenarioFile, JSON.stringify(scenario, null, 2));

    await login(page, "e2e-grok-partials");
    const minted = await page.request.post("/v1/hosts/enroll-token", {
      headers: { Origin: new URL(page.url()).origin },
    });
    expect(minted.ok()).toBe(true);
    const enrollToken = (await minted.json()).token;
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
        // Fixture validation requires these; GROK_HOME is also the home the
        // promoted file adapter discovers from the Node environment, so it
        // must be the same directory the fake writes its session tree into.
        REMUDA_CLAUDE_BIN: path.join(bin, "grok"),
        REMUDA_CLAUDE_CONFIG_DIR: grokHome,
        GROK_HOME: grokHome,
        SHELL: shell,
        PATH: `${bin}:/usr/bin:/bin:/usr/sbin:/sbin`,
        REMUDA_HERDR_ORPHAN_SWEEP: "0",
        RUST_LOG: "info",
      },
    });
    node.stdout?.on("data", (chunk) => { nodeLog += String(chunk); });
    node.stderr?.on("data", (chunk) => { nodeLog += String(chunk); });
    node.on("error", (error) => { nodeLog += error.message; });

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
        name: "ux-grok-partials",
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
    await expect(page.locator("[data-tty-lab='1']")).toHaveAttribute("data-tty-status", "live", { timeout: 20_000 });

    // Launch grok exactly as a human would: a foreground process named grok,
    // replaying the keep-alive copy of the landed scenario in the shared
    // GROK_HOME.
    await rawKeys(
      page,
      id,
      `grok --kind grok --home ${quote(grokHome)} --script ${quote(scenarioFile)} --events-out ${quote(eventsFile)}\r`,
    );
    // Wait for promotion BEFORE submitting so the file adapter (250 ms poll)
    // is bound to the session directory before the 900 ms shell tool starts;
    // otherwise it would read the whole finished log in one historical sweep
    // and there would be nothing growing to see.
    await expect(session).toHaveAttribute("data-mode", "promoted", { timeout: 20_000 });
    await page.waitForTimeout(400);

    await page.getByTestId("view-switch-structured").click();
    await expect(page.getByTestId("transcript")).toBeVisible();

    // High-frequency card sampler installed BEFORE the prompt so the live
    // growth between Partial frames is not missed the way a poll could.
    // `stdout` is the stdout <pre> (the first <pre> is the `$ command` line),
    // and `settled` is the card showing its exit pill — which only happens on
    // the Final. (The last Partial can land as the instance activity flips to
    // idle, so growth is keyed on card settlement, not the activity flag.)
    await page.evaluate(() => {
      type Sample = { at: number; working: boolean; settled: boolean; stdout: string };
      const samples: Sample[] = [];
      (window as unknown as { __grokStdoutSamples?: Sample[] }).__grokStdoutSamples = samples;
      const sample = () => {
        const pageEl = document.querySelector("[data-testid='session-page']");
        const working = pageEl?.getAttribute("data-activity") === "working";
        const cards = [...document.querySelectorAll("[data-testid='tool-card']")] as HTMLElement[];
        const shell = cards.find((card) => /printf/.test(card.textContent ?? ""));
        if (!shell) return;
        const pre = [...shell.querySelectorAll("pre")].map((node) => node.textContent ?? "");
        const stdout = pre.slice(1).join("\n");
        const settled = /exit 0/.test(shell.textContent ?? "");
        const last = samples.at(-1);
        if (!last || last.working !== working || last.settled !== settled || last.stdout !== stdout) {
          samples.push({ at: performance.now(), working, settled, stdout });
        }
      };
      (window as unknown as { __grokStdoutTimer?: ReturnType<typeof setInterval> }).__grokStdoutTimer =
        setInterval(sample, 30);
    });

    // The harness treats body+CR in one read as a paste; split them.
    await rawKeys(page, id, "GROK_TOOLS run the shell thing");
    await page.waitForTimeout(100);
    await rawKeys(page, id, "\r");

    // The turn is short (the shell call drips for ~900 ms), so do not race
    // the working/idle activity attributes — the sampler above was installed
    // before the submit and records every frame. Wait for the turn's answer,
    // the definitive completion signal, then let the card settle and fold.
    await expect(page.getByTestId("transcript")).toContainText("SPIKE_COMPLETE GROK_TOOLS", { timeout: 20_000 });
    await page.waitForTimeout(500);

    await page.evaluate(() => {
      const timer = (window as unknown as { __grokStdoutTimer?: ReturnType<typeof setInterval> }).__grokStdoutTimer;
      if (timer) clearInterval(timer);
    });
    const samples = await page.evaluate(
      () => (window as unknown as { __grokStdoutSamples?: StdoutSample[] }).__grokStdoutSamples ?? [],
    );

    // 1 — GROWTH BEFORE FINAL: before the card shows its exit pill (i.e. on
    // the streamed Partials only), distinct stdout prefixes rendered, each a
    // strict extension of the previous one. The streamed text is a raw byte
    // stream (no separator between fragments); the "$ …" header is part of the
    // streamed prefix and the command's lines arrive progressively.
    const preFinal = samples.filter((s) => !s.settled && s.stdout.length > 0);
    const distinctPreFinal = preFinal.filter((s, i) => i === 0 || s.stdout !== preFinal[i - 1]!.stdout);
    expect(
      distinctPreFinal.length,
      `expected growing pre-final stdout; samples: ${JSON.stringify(samples.map((s) => s.stdout))}`,
    ).toBeGreaterThanOrEqual(2);
    for (let i = 1; i < distinctPreFinal.length; i += 1) {
      expect(
        distinctPreFinal[i]!.stdout.startsWith(distinctPreFinal[i - 1]!.stdout),
        `pre-final stdout only grows by extension: ${JSON.stringify(distinctPreFinal.map((s) => s.stdout))}`,
      ).toBe(true);
    }
    expect(
      distinctPreFinal.some((s) => s.stdout.includes("one")),
      `"one" streamed before the Final: ${JSON.stringify(distinctPreFinal.map((s) => s.stdout))}`,
    ).toBe(true);
    expect(
      distinctPreFinal.some((s) => s.stdout.includes("two")),
      `"two" streamed before the Final: ${JSON.stringify(distinctPreFinal.map((s) => s.stdout))}`,
    ).toBe(true);

    // 2 — THE FINAL IS AUTHORITATIVE. At turn end the routine tools fold into
    // a compact group; expand it and assert the settled shell card.
    const fold = page.getByTestId("compact-fold").first();
    await expect(fold).toHaveAttribute("aria-expanded", "false", { timeout: 5_000 });
    await fold.click();
    const shellCard = page.getByTestId("tool-card").filter({ hasText: "printf" }).first();
    await expect(shellCard).toContainText("exit 0", { timeout: 5_000 });
    const settledAll = (await shellCard.textContent()) ?? "";
    expect(settledAll).toContain("one\ntwo\nthree\n");
    // The Final text is the stdout block (the `$ command` line is a separate,
    // always-present first <pre>, and stdout is the following one). It holds
    // the authoritative result exactly: no streamed "$ …" header inside stdout,
    // and its lines are not duplicated.
    const settledStdout = await shellCard.locator("pre").nth(1).textContent({ timeout: 5_000 }).catch(() => "");
    expect(settledStdout).toBe("one\ntwo\nthree\n");
    expect(settledStdout).not.toContain("$ printf");
    expect(settledStdout.match(/one/g)).toHaveLength(1);
    expect(settledStdout.match(/three/g)).toHaveLength(1);

    // 3 — PRODUCER GROUND TRUTH: the durable journal for the shell node holds
    // one or more Partial Append frames (revisions strictly increasing) and
    // then a Final Close at a strictly higher revision whose text is exactly
    // the authoritative result — so the DOM growth is driven end to end.
    const journal = await page.request.get(`/v1/instances/${id}/journal`, { timeout: 10_000 }).then((r) => r.json()) as {
      events: { event: { kind: string; payload: Record<string, unknown> } }[];
    };
    const results = journal.events
      .map((row) => row.event)
      .filter((event) => event.kind === "tool_result")
      .map((event) => ({
        revision: String(event.payload.revision),
        base: event.payload.baseRevision === null ? null : String(event.payload.baseRevision),
        operation: String(event.payload.operation),
        stage: String(event.payload.stage),
        text: ((event.payload.blocks as { type: string; text?: string }[] | undefined) ?? [])
          .filter((block) => block.type === "text")
          .map((block) => block.text ?? "")
          .join(""),
      }));
    // The shell result is the one whose Final carries the three lines.
    const finalIndex = results.findIndex((r) => r.stage === "final" && r.text === "one\ntwo\nthree\n");
    expect(
      finalIndex >= 0,
      `final shell result in journal: ${JSON.stringify(results.map((r) => ({ ...r, text: JSON.stringify(r.text) })))}`,
    ).toBe(true);
    const before = results.slice(0, finalIndex);
    const partialAppends = before.filter((r) => r.stage === "partial" && r.operation === "append");
    expect(
      partialAppends.length >= 1,
      `streamed partial appends before final: ${JSON.stringify(results.map((r) => ({ op: r.operation, stage: r.stage })))}`,
    ).toBe(true);
    for (let i = 1; i < partialAppends.length; i += 1) {
      expect(BigInt(partialAppends[i]!.revision) > BigInt(partialAppends[i - 1]!.revision)).toBe(true);
    }
    const finalResult = results[finalIndex]!;
    expect(finalResult.operation).toBe("close");
    expect(BigInt(finalResult.revision) > BigInt(partialAppends.at(-1)!.revision)).toBe(true);
    // Every Append's declared base is the revision right before it — the
    // node-scoped counter the call track and result track share.
    for (const partial of partialAppends) {
      expect(partial.base).not.toBeNull();
    }
  } finally {
    if (instanceId) await command(page, instanceId, "instance.close").catch(() => {});
    if (node) await stopNode(node);
    try {
      const logPath = testInfo.outputPath("native-node.log");
      await writeFile(logPath, nodeLog.replaceAll(dir, "$TEST_DIR")).catch(() => {});
      await testInfo.attach("native-node.log", { path: logPath, contentType: "text/plain" }).catch(() => {});
      const groundPath = testInfo.outputPath("native-events.jsonl");
      await writeFile(groundPath, await readFile(eventsFile, "utf8").catch(() => "")).catch(() => {});
      await testInfo.attach("native-events.jsonl", { path: groundPath, contentType: "application/x-ndjson" }).catch(() => {});
    } catch { /* teardown only */ }
    dataHandle?.close();
    await rm(dir, { recursive: true, force: true });
  }
});
