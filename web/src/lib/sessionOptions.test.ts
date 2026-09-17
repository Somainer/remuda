import { describe, expect, it } from "vitest";
import {
  DELEGATION_OPTIONS,
  YOLO_HINT,
  PTY_YOLO_FLAGS,
  ptyYoloHint,
  ptyYoloChipLabel,
  claudeHostAuth,
  claudeProviderHint,
  normalizeDelegation,
  normalizePermissionMode,
  launchPermissionTable,
  providerLaunchHint,
  providerProfileForDelegation,
} from "./sessionOptions";

describe("sessionOptions", () => {
  it("lists the six real Claude modes and maps native/legacy words", () => {
    expect(launchPermissionTable("claude").map((o) => o.id)).toEqual([
      "manual",
      "acceptEdits",
      "plan",
      "auto",
      "bypassPermissions",
      "dontAsk",
    ]);
    expect(normalizePermissionMode("claude", "default")).toBe("manual");
    expect(normalizePermissionMode("claude", "bypass")).toBe("bypassPermissions");
    expect(normalizePermissionMode("claude", "acceptEdits")).toBe("acceptEdits");
    expect(normalizePermissionMode("claude", "plan")).toBe("plan");
    expect(normalizePermissionMode("claude", "auto")).toBe("auto");
    expect(normalizePermissionMode("claude", "dontAsk")).toBe("dontAsk");
    expect(normalizePermissionMode("claude", undefined)).toBe("manual");
    expect(YOLO_HINT).toMatch(/审批/);
  });

  it("exposes codex/grok/agy native permission sets", () => {
    const codex = launchPermissionTable("codex").map((o) => o.id);
    expect(codex).toContain("untrusted");
    expect(codex).toContain("on-request");
    expect(codex).toContain("never");
    expect(codex).toContain("sandbox:read-only");
    expect(codex).toContain("sandbox:workspace-write");
    expect(codex).toContain("sandbox:danger-full-access");
    expect(launchPermissionTable("grok").map((o) => o.id)).toEqual([
      "native-prompt",
      "auto",
      "always-approve",
    ]);
    expect(launchPermissionTable("agy").map((o) => o.id)).toEqual([
      "native",
      "accept-edits",
      "plan",
      "always-proceed",
    ]);
  });

  it("defaults create to follow-host, not a named gateway vendor", () => {
    expect(normalizeDelegation(undefined)).toBe("host");
    expect(normalizeDelegation("gateway")).toBe("gateway");
    expect(normalizeDelegation("none")).toBe("none");
    expect(providerProfileForDelegation("host")).toBeUndefined();
    expect(providerProfileForDelegation("none")).toBe("none");
    expect(providerProfileForDelegation("gateway")).toBe("gateway");
    expect(providerProfileForDelegation("gateway", "pvp_01993ab0-0000-7000-8000-000000000010")).toBe(
      "pvp_01993ab0-0000-7000-8000-000000000010",
    );
    expect(DELEGATION_OPTIONS.map((o) => o.id)).toEqual(["host", "none", "gateway"]);
    expect(DELEGATION_OPTIONS.some((o) => /astergate/i.test(o.label))).toBe(false);
    expect(DELEGATION_OPTIONS.map((o) => o.label).join(" ")).not.toContain("自动");
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

  it("previews Hub resolution as a New Session hint", () => {
    const uni = { id: "pvp_u", name: "uni-gw", scope: "universal", defaultGateway: true };
    expect(
      providerLaunchHint({
        kind: "claude",
        binding: "auto",
        cli: [{ kind: "claude", auth: "logged_out" }],
        profiles: [uni],
        delegation: "host",
      }),
    ).toBe("将使用 uni-gw (universal)");
    expect(
      providerLaunchHint({
        kind: "claude",
        binding: "native",
        cli: [{ kind: "claude", auth: "logged_out" }],
        profiles: [uni],
        delegation: "host",
      }),
    ).toBe("使用主机原生登录");
    expect(
      providerLaunchHint({
        kind: "claude",
        binding: "auto",
        cli: [{ kind: "claude", auth: "logged_out" }],
        profiles: [],
        delegation: "host",
      }),
    ).toBe("此主机未配置 Claude 登录/网关，请选择 Provider");
  });

  it("exposes generic-pty yolo preset flags as a hint", () => {
    expect(PTY_YOLO_FLAGS.codex).toContain("bypass-approvals");
    expect(PTY_YOLO_FLAGS.grok).toBe("--always-approve");
    expect(ptyYoloHint("grok")).toContain("--always-approve");
    expect(ptyYoloHint("codex")).toContain("generic-pty");
    expect(ptyYoloChipLabel("grok")).toBe("always-approve");
    expect(ptyYoloChipLabel("codex")).toBe("bypass");
    expect(ptyYoloChipLabel("agy")).toBe("bypass");
  });
});
