import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { mapInstance } from "../../lib/api";
import { canShowTerminal } from "./tty/gate";
import { hasStructuredSignal } from "./tty/gate";
import { isGenericPty } from "../../lib/status";
import { HerdrClaimBadge, closeVerb, releaseConfirmText } from "./HerdrClaimBadge";
import type { components } from "../../lib/api.generated";

/**
 * D-062: a claimed external herdr pane shows in the ledger and on /s/:id like
 * any instance, with a small 「外部 herdr」 badge naming session · workspace ·
 * tab; it is a terminal (no transcript), and its close is 释放.
 */
function record(extra: Record<string, unknown> = {}): components["schemas"]["InstanceRecord"] {
  return {
    instanceId: "ins_claimed",
    hostId: "hst_1",
    kind: "claude",
    driver: "herdr-claimed",
    lifecycle: "running",
    activity: "idle",
    connectivity: "connected",
    journalId: "obj_1",
    durableSeq: "3",
    configureSeq: 0,
    createdAt: "2026-11-04T00:00:00.000Z",
    updatedAt: "2026-11-04T00:00:00.000Z",
    lineageId: "ins_claimed",
    generation: 1,
    scope: {},
    launchedBy: "user",
    permissionMode: "external",
    herdrClaim: {
      session: "work",
      socketPath: "/run/herdr/sessions/work/herdr.sock",
      workspaceId: "w2",
      tabId: "w2:t19",
      paneId: "w2:p5K",
      agent: "claude",
      process: { pid: 4242, command: "claude" },
      contact: "ok",
    },
    ...extra,
  } as unknown as components["schemas"]["InstanceRecord"];
}

describe("herdr-claimed instances (D-062)", () => {
  it("keeps the driver and the claim ids, and is a terminal without a transcript", () => {
    const instance = mapInstance(record());
    expect(instance.driver).toBe("herdr-claimed");
    expect(instance.launchedBy).toBe("user");
    expect(instance.herdrClaim).toEqual({
      session: "work",
      workspaceId: "w2",
      tabId: "w2:t19",
      paneId: "w2:p5K",
      agent: "claude",
      contact: "ok",
    });
    expect(canShowTerminal(instance)).toBe(true);
    expect(hasStructuredSignal(instance)).toBe(false);
    expect(isGenericPty(instance)).toBe(true);
    const caps = instance.capabilities.capabilities;
    expect(caps["tty-attach"]?.state).toBe("supported");
    expect(caps.resume?.state).toBe("unsupported");
    expect(caps["model-switch"]?.state).toBe("unsupported");
  });

  it("drops a claim on any other driver and a malformed claim", () => {
    expect(mapInstance(record({ driver: "shell-pty" })).herdrClaim).toBeNull();
    expect(mapInstance(record({ herdrClaim: { session: "work" } })).herdrClaim).toBeNull();
  });

  it("renders 外部 herdr with session · workspace · tab, and 失联 on contact loss", () => {
    const { rerender } = render(<HerdrClaimBadge instance={mapInstance(record())} />);
    const badge = screen.getByTestId("herdr-claim-badge");
    expect(badge).toHaveTextContent("外部 herdr");
    expect(badge).toHaveTextContent("work · w2 · w2:t19");
    expect(badge).not.toHaveTextContent("失联");
    expect(badge.getAttribute("title")).toContain("w2:p5K");
    const lost = record();
    (lost as { herdrClaim: { contact: string } }).herdrClaim.contact = "lost";
    rerender(<HerdrClaimBadge instance={mapInstance(lost)} />);
    expect(screen.getByTestId("herdr-claim-badge")).toHaveTextContent("失联");
    expect(screen.getByTestId("herdr-claim-badge").dataset.contact).toBe("lost");
  });

  it("renders nothing for an ordinary instance", () => {
    const { container } = render(<HerdrClaimBadge instance={mapInstance(record({ driver: "claude-pty" }))} />);
    expect(container).toBeEmptyDOMElement();
  });

  it("calls the close 释放 and says the pane keeps running", () => {
    expect(closeVerb({ driver: "herdr-claimed" }, "关闭")).toBe("释放");
    expect(closeVerb({ driver: "claude-pty" }, "关闭")).toBe("关闭");
    expect(releaseConfirmText("x")).toContain("继续运行");
  });
});
