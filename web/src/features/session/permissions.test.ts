import { describe, expect, it } from "vitest";
import {
  isLiveReachable,
  launchPermissionTable,
  runtimePermissionTable,
} from "./permissions";

describe("launch vs runtime permission tables (c-effortui r3 items 5/7)", () => {
  it("New Session's Claude launch table offers every CLI mode incl. plan and dontAsk", () => {
    expect(launchPermissionTable("claude").map((row) => row.id)).toEqual([
      "manual",
      "acceptEdits",
      "plan",
      "auto",
      "bypassPermissions",
      "dontAsk",
    ]);
  });

  it("the live wheel walks the CLI's shift+tab order and gates bypass/dontAsk", () => {
    // Default launch: the measured cycle manual → acceptEdits → plan → auto.
    expect(runtimePermissionTable("claude").map((row) => row.id)).toEqual([
      "manual",
      "acceptEdits",
      "plan",
      "auto",
    ]);
    // dontAsk never joins a live wheel.
    expect(runtimePermissionTable("claude").some((row) => row.id === "dontAsk")).toBe(false);
    // Bypass joins only when the session launched with the allowance.
    expect(runtimePermissionTable("claude").some((row) => row.id === "bypassPermissions")).toBe(false);
    expect(
      runtimePermissionTable("claude", { bypassAllowed: true }).map((row) => row.id),
    ).toEqual(["manual", "acceptEdits", "plan", "auto", "bypassPermissions"]);
  });

  it("the live wheel exists only for Claude; other harnesses get a read-only chip", () => {
    for (const kind of ["codex", "grok", "agy", "terminal", "generic"]) {
      expect(runtimePermissionTable(kind)).toEqual([]);
    }
  });

  it("other harnesses keep their OWN native launch ids — never Claude's", () => {
    expect(launchPermissionTable("grok").map((row) => row.id)).toEqual([
      "native-prompt",
      "auto",
      "always-approve",
    ]);
    expect(launchPermissionTable("agy").map((row) => row.id)).toEqual([
      "native",
      "accept-edits",
      "plan",
      "always-proceed",
    ]);
    for (const id of ["acceptEdits", "manual", "bypassPermissions"]) {
      expect(launchPermissionTable("grok").some((row) => row.id === id)).toBe(false);
      expect(launchPermissionTable("agy").some((row) => row.id === id)).toBe(false);
    }
  });

  it("keeps the danger rows dangerous (bypass / never / full access)", () => {
    const danger = (kind: string, id: string) =>
      launchPermissionTable(kind).find((row) => row.id === id)?.danger === true;
    expect(danger("claude", "bypassPermissions")).toBe(true);
    expect(danger("codex", "never")).toBe(true);
    expect(danger("codex", "sandbox:danger-full-access")).toBe(true);
    expect(danger("grok", "always-approve")).toBe(true);
    expect(danger("agy", "always-proceed")).toBe(true);
  });

  it("isLiveReachable matches the same cycle gating", () => {
    expect(isLiveReachable("claude", "plan", "manual")).toBe(true);
    expect(isLiveReachable("claude", "dontAsk", "manual")).toBe(false);
    expect(isLiveReachable("claude", "bypassPermissions", "manual")).toBe(false);
    expect(isLiveReachable("claude", "bypassPermissions", "bypassPermissions")).toBe(true);
    expect(isLiveReachable("grok", "acceptEdits", undefined)).toBe(false);
  });
});
