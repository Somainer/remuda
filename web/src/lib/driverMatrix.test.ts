import { describe, expect, it } from "vitest";
import { DRIVER_LABELS, defaultDriver, launchPreview, legacyDrivers, shellPtyAllowed, type HostMatrix } from "./driverMatrix";

const withCli = (kinds: string[]): HostMatrix => ({
  cli: kinds.map((kind) => ({ kind, installed: true })),
});

describe("New Session driver default (D-028 §5.1, matrix from the Node, never hardcoded)", () => {
  it("terminal is always shell-pty", () => {
    expect(defaultDriver(undefined, "terminal")).toBe("shell-pty");
    expect(shellPtyAllowed(undefined, "terminal")).toBe(true);
  });

  it("falls back to legacy drivers when the CLI exists but no driver matrix was reported", () => {
    const host = withCli(["claude", "codex", "grok", "agy"]);
    expect(shellPtyAllowed(host, "claude")).toBe(false);
    expect(defaultDriver(host, "claude")).toBe("claude-pty");
    for (const kind of ["codex", "grok", "agy"] as const) {
      expect(shellPtyAllowed(host, kind)).toBe(false);
      expect(defaultDriver(host, kind)).toBe("generic-pty");
    }
  });

  it("missing CLI inventory never defaults an agent kind to shell-pty", () => {
    expect(defaultDriver({}, "claude")).not.toBe("shell-pty");
  });

  it("falls back to a legacy driver when the harness binary is not installed", () => {
    const host = withCli(["codex"]);
    expect(shellPtyAllowed(host, "grok")).toBe(false);
    expect(defaultDriver(host, "grok")).toBe("generic-pty");
    expect(defaultDriver(host, "claude")).toBe("claude-pty");
    // codex IS installed but no matrix was reported → still a legacy carrier.
    expect(defaultDriver(host, "codex")).toBe("generic-pty");
  });

  it("honours an explicit launchable shell-pty descriptor from driverInventory", () => {
    const host: HostMatrix = {
      ...withCli(["claude"]),
      capabilities: { driverInventory: [{ kind: "shell-pty", launchable: true }, { kind: "claude-print" }] },
    };
    expect(shellPtyAllowed(host, "claude")).toBe(true);
  });

  it("a non-launchable shell-pty descriptor removes the default even with the CLI present", () => {
    const host: HostMatrix = {
      ...withCli(["claude"]),
      capabilities: { driverInventory: [{ kind: "shell-pty", launchable: false }] },
    };
    expect(shellPtyAllowed(host, "claude")).toBe(false);
    expect(defaultDriver(host, "claude")).toBe("claude-pty");
  });

  it("an inventory without a shell-pty row also falls back (the matrix says no)", () => {
    const host: HostMatrix = {
      ...withCli(["claude"]),
      capabilities: { driverInventory: [{ kind: "claude-print", launchable: true }] },
    };
    expect(shellPtyAllowed(host, "claude")).toBe(false);
  });

  it("an installed:false CLI entry refuses the native default", () => {
    const host = withCli([]);
    host.cli = [{ kind: "grok", installed: false }];
    expect(shellPtyAllowed(host, "grok")).toBe(false);
  });

  it("keeps the legacy drivers selectable but secondary", () => {
    expect(legacyDrivers("claude")).toEqual(["claude-pty", "generic-pty", "claude-print"]);
    expect(legacyDrivers("grok")).toEqual(["generic-pty"]);
  });

  it("never defaults to claude-print for any host shape (D-035)", () => {
    // A print session ends after one turn and needs a manual resume, so it is a
    // diagnostic carrier chosen explicitly — never a default and never a
    // fallback. Every reachable host shape is checked, including the ones that
    // report print as the only launchable driver.
    const shapes: HostMatrix[] = [
      {},
      { cli: [] },
      withCli(["claude"]),
      { ...withCli(["claude"]), capabilities: null },
      { ...withCli(["claude"]), capabilities: { driverInventory: [] } },
      { ...withCli(["claude"]), capabilities: { driverInventory: [{ kind: "shell-pty", launchable: true }] } },
      { ...withCli(["claude"]), capabilities: { driverInventory: [{ kind: "shell-pty", launchable: false }] } },
      { ...withCli(["claude"]), capabilities: { driverInventory: [{ kind: "claude-print", launchable: true }] } },
    ];
    for (const host of shapes) {
      for (const kind of ["claude", "codex", "grok", "agy"] as const) {
        expect(defaultDriver(host, kind)).not.toBe("claude-print");
      }
      expect(defaultDriver(host, "terminal")).not.toBe("claude-print");
    }
  });

  it("labels claude-print as diagnostic so the picker cannot read as a peer carrier", () => {
    expect(DRIVER_LABELS["claude-print"]).toContain("诊断");
  });

  it("previews the materialized argv summary per kind", () => {
    expect(launchPreview({ kind: "claude", effortName: "high", yolo: false })).toContain("claude");
    expect(launchPreview({ kind: "claude", effortName: "high" })).toContain("--effort high");
    expect(launchPreview({ kind: "claude", effortName: "xhigh", yolo: true })).toContain(
      "--dangerously-skip-permissions",
    );
    expect(launchPreview({ kind: "codex" })).toContain("codex");
    expect(launchPreview({ kind: "grok", yolo: true })).toContain("--always-approve");
    expect(launchPreview({ kind: "agy" })).toContain("agy");
    expect(launchPreview({ kind: "terminal" })).toContain("$SHELL");
  });
});
