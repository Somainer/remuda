import { StrictMode, act, type ReactNode } from "react";
import { renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Host, Instance } from "../../types/instance";
import type { Interaction } from "../../types/interaction";
import type { HubState } from "../../lib/store";
import { known, unknownKnowledge } from "../../types/wire";

/**
 * c-ghostbadge round 2: the badge count must cross a deadline on the CLOCK,
 * with no store emission — the persistent PhoneShell badge used to stay at 1
 * while a freshly mounted inbox already showed 0.
 */
const useHubMock = vi.hoisted(() => ({ useHub: vi.fn() }));
vi.mock("../../lib/store", () => ({ useHub: useHubMock.useHub }));

import { useInboxPendingCount } from "./useInboxPendingCount";
import { __resetInboxClockForTest, syncInboxDeadlineClock } from "./inboxClock";

const T0 = "2026-09-20T10:00:00.000Z";

function instance(id: string): Instance {
  return {
    id,
    hostId: "hst_1",
    workspaceId: "wsp_1",
    lifecycle: "running",
    activity: known("working"),
    connectivity: "connected",
  } as Instance;
}

function host(id: string): Host {
  return { id, state: "online" } as Host;
}

function approval(id: string, deadlineISO: string | null): Interaction {
  return {
    id,
    instanceId: "ins_1",
    hostId: "hst_1",
    kind: "approval",
    state: "pending",
    blocking: true,
    answerable: true,
    carrier: "claude-control",
    request: {
      kind: "approval",
      title: "Bash",
      description: "echo e2e",
      toolCallId: null,
      actionRef: "act_1",
      options: [],
      requestedPermissionsRef: null,
      inputDigest: "sha256:abab",
    },
    deadline: deadlineISO ? known(deadlineISO) : unknownKnowledge("none"),
    deadlineSource: deadlineISO ? "runtime-policy" : "none",
  } as unknown as Interaction;
}

function hubWith(interactions: Interaction[]): HubState {
  return {
    interactions,
    instances: [instance("ins_1")],
    hosts: [host("hst_1")],
    answering: {},
  } as unknown as HubState;
}

function wrapper(children: { children: ReactNode }) {
  return <StrictMode>{children.children}</StrictMode>;
}

describe("useInboxPendingCount deadline clock", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(Date.parse(T0));
    __resetInboxClockForTest(Date.parse(T0));
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.clearAllMocks();
  });

  it("flips 1 -> 0 when the known deadline crosses, with unchanged store slices", () => {
    const now = Date.parse(T0);
    vi.setSystemTime(now);
    const card = approval("int_1", new Date(now + 100).toISOString());
    useHubMock.useHub.mockReturnValue(hubWith([card]));
    // Bootstrap the shared clock at the fake "now" (the mounted badge effect
    // does this on a real page); it arms the timer at the deadline.
    syncInboxDeadlineClock([card], now);

    const { result } = renderHook(() => useInboxPendingCount(), { wrapper });
    expect(result.current).toBe(1);

    // No store emission happens; advance time across the deadline (the
    // clock schedules strictly past it, so advance a few ms extra).
    act(() => {
      vi.advanceTimersByTime(110);
    });
    expect(result.current).toBe(0);
  });

  it("keeps counting an unknown-deadline pending card across time", () => {
    const now = Date.parse(T0);
    vi.setSystemTime(now);
    const card = approval("int_2", null);
    useHubMock.useHub.mockReturnValue(hubWith([card]));
    syncInboxDeadlineClock([card], now);

    const { result } = renderHook(() => useInboxPendingCount(), { wrapper });
    expect(result.current).toBe(1);
    act(() => {
      vi.advanceTimersByTime(60_000);
    });
    expect(result.current).toBe(1);
  });
});
