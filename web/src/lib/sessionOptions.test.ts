import { describe, expect, it } from "vitest";
import {
  DELEGATION_OPTIONS,
  PERMISSION_OPTIONS,
  YOLO_HINT,
  PTY_YOLO_FLAGS,
  ptyYoloHint,
  normalizeDelegation,
  normalizePermissionMode,
  providerProfileForDelegation,
} from "./sessionOptions";

describe("sessionOptions", () => {
  it("maps yolo / dontAsk onto bypassPermissions", () => {
    expect(normalizePermissionMode("bypassPermissions")).toBe("bypassPermissions");
    expect(normalizePermissionMode("dontAsk")).toBe("bypassPermissions");
    expect(normalizePermissionMode("manual")).toBe("manual");
    expect(PERMISSION_OPTIONS.some((o) => o.id === "bypassPermissions" && o.label === "全自动")).toBe(true);
    expect(YOLO_HINT.toLowerCase()).toContain("bypasspermissions");
  });

  it("defaults delegation to native none, not a named gateway vendor", () => {
    expect(normalizeDelegation(undefined)).toBe("none");
    expect(normalizeDelegation("gateway")).toBe("gateway");
    expect(providerProfileForDelegation("none")).toBe("none");
    expect(providerProfileForDelegation("gateway")).toBe("gateway");
    expect(DELEGATION_OPTIONS.map((o) => o.id)).toEqual(["none", "gateway"]);
    expect(DELEGATION_OPTIONS.some((o) => /astergate/i.test(o.label))).toBe(false);
  });

  it("exposes generic-pty yolo preset flags as a hint", () => {
    expect(PTY_YOLO_FLAGS.codex).toContain("bypass-approvals");
    expect(PTY_YOLO_FLAGS.grok).toBe("--always-approve");
    expect(ptyYoloHint("grok")).toContain("--always-approve");
    expect(ptyYoloHint("codex")).toContain("generic-pty");
  });
});
