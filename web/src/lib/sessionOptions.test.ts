import { describe, expect, it } from "vitest";
import {
  DELEGATION_OPTIONS,
  PERMISSION_OPTIONS,
  YOLO_HINT,
  PTY_YOLO_FLAGS,
  ptyYoloHint,
  claudeHostAuth,
  claudeProviderHint,
  normalizeDelegation,
  normalizePermissionMode,
  providerProfileForDelegation,
} from "./sessionOptions";

describe("sessionOptions", () => {
  it("keeps dontAsk and bypassPermissions as distinct modes", () => {
    expect(normalizePermissionMode("bypassPermissions")).toBe("bypassPermissions");
    expect(normalizePermissionMode("dontAsk")).toBe("dontAsk");
    expect(normalizePermissionMode("manual")).toBe("manual");
    expect(PERMISSION_OPTIONS.map((o) => o.id)).toEqual(["manual", "acceptEdits", "dontAsk", "bypassPermissions"]);
    expect(PERMISSION_OPTIONS.some((o) => o.id === "dontAsk" && o.label === "全自动")).toBe(true);
    expect(PERMISSION_OPTIONS.some((o) => o.id === "bypassPermissions" && o.label === "绕过全部")).toBe(true);
    expect(YOLO_HINT).toMatch(/审批/);
  });

  it("defaults delegation to native none, not a named gateway vendor", () => {
    expect(normalizeDelegation(undefined)).toBe("none");
    expect(normalizeDelegation("gateway")).toBe("gateway");
    expect(providerProfileForDelegation("none")).toBe("none");
    expect(providerProfileForDelegation("gateway")).toBe("gateway");
    expect(providerProfileForDelegation("gateway", "pvp_01993ab0-0000-7000-8000-000000000010")).toBe(
      "pvp_01993ab0-0000-7000-8000-000000000010",
    );
    expect(DELEGATION_OPTIONS.map((o) => o.id)).toEqual(["none", "gateway"]);
    expect(DELEGATION_OPTIONS.some((o) => /astergate/i.test(o.label))).toBe(false);
  });

  it("hints when the host has no Claude login or native gateway", () => {
    expect(claudeHostAuth([{ kind: "claude", auth: "gateway-native" }])).toBe("gateway-native");
    expect(claudeHostAuth([{ kind: "claude", auth: "logged_in" }])).toBe("logged_in");
    expect(claudeHostAuth([{ kind: "claude", auth: "logged_out" }])).toBe("none");
    expect(claudeProviderHint("claude", [{ kind: "claude", auth: "logged_out" }])).toBe(
      "此主机未配置 Claude 登录/网关，请选择 Provider",
    );
    expect(claudeProviderHint("claude", [{ kind: "claude", auth: "logged_in" }])).toBeNull();
    expect(claudeProviderHint("codex", [{ kind: "claude", auth: "logged_out" }])).toBeNull();
  });

  it("exposes generic-pty yolo preset flags as a hint", () => {
    expect(PTY_YOLO_FLAGS.codex).toContain("bypass-approvals");
    expect(PTY_YOLO_FLAGS.grok).toBe("--always-approve");
    expect(ptyYoloHint("grok")).toContain("--always-approve");
    expect(ptyYoloHint("codex")).toContain("generic-pty");
  });
});
