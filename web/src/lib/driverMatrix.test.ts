import { describe, expect, it } from "vitest";
import { defaultDriver, launchPreview, legacyDrivers, shellPtyAllowed, type HostMatrix } from "./driverMatrix";

const withCli = (kinds: string[]): HostMatrix => ({
  cli: kinds.map((kind) => ({ kind, installed: true })),
});

describe("New Session driver default (D-028 §5.1, matrix from the Node, never hardcoded)", () => {
  it("terminal is always shell-pty", () => {
    expect(defaultDriver(undefined, "terminal")).toBe("shell-pty");
    expect(shellPtyAllowed(undefined, "terminal")).toBe(true);
  });

  it("defaults every agent kind to shell-pty when the CLI exists and no matrix was reported", () => {
    const host = withCli(["claude", "codex", "grok", "agy"]);
    for (const kind of ["claude", "codex", "grok", "agy"] as const) {
      expect(shellPtyAllowed(host, kind)).toBe(true);
      expect(defaultDriver(host, kind)).toBe("shell-pty");
    }
  });

  it("missing CLI inventory is treated as 'not reported', not 'unsupported'", () => {
    expect(defaultDriver({}, "claude")).toBe("shell-pty");
  });

  it("falls back to a legacy driver when the harness binary is not installed", () => {
    const host = withCli(["codex"]);
    expect(shellPtyAllowed(host, "grok")).toBe(false);
    expect(defaultDriver(host, "grok")).toBe("generic-pty");
    expect(defaultDriver(host, "claude")).toBe("claude-print");
    // codex IS installed → native PTY stays the default.
    expect(defaultDriver(host, "codex")).toBe("shell-pty");
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
    expect(defaultDriver(host, "claude")).toBe("claude-print");
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
    expect(legacyDrivers("claude")).toEqual(["claude-print", "claude-pty", "generic-pty"]);
    expect(legacyDrivers("grok")).toEqual(["generic-pty"]);
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
