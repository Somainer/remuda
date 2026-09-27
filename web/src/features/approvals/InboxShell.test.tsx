import { render } from "@testing-library/react";
import { act } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { DepartedList } from "./InboxShell";
import { deriveApprovalRows, type ApprovalRow } from "./approvalRows";
import type { Host, Instance } from "../../types/instance";
import type { Interaction } from "../../types/interaction";
import { known, na, unknownKnowledge, type Id } from "../../types/wire";

/**
 * c-inboxperf / UO-9 round-3: drive the REAL keyed DepartedList the desktop
 * shell renders, fed by the REAL deriveApprovalRows. A 2 s poll produces fresh
 * interaction objects (re-parse) and a new interaction; the probe lives inside
 * the memo boundary, so an unchanged 已离队 row must not commit again.
 */

const T0 = "2026-09-20T10:00:00.000Z";
const T1 = "2026-09-20T11:00:00.000Z";

function inst(id: Id): Instance {
  return {
    revision: "1",
    createdAt: T0,
    updatedAt: T0,
    id,
    hostId: "hst_1",
    workspaceId: "wsp_1",
    kind: "claude",
    driver: "claude-cli",
    lifecycle: "running",
    activity: known("working"),
    connectivity: "connected",
    ownership: "managed",
    nativeRef: {} as Instance["nativeRef"],
    processRef: {} as Instance["processRef"],
    specRevision: "1",
    launchId: unknownKnowledge("none"),
    capabilities: {} as Instance["capabilities"],
    ownerFence: "1",
    activeRunIds: [],
    parent: null,
    journalId: "jrn_1",
    durableSeq: "1",
    exit: na(),
  } as unknown as Instance;
}

const host: Host = {
  revision: "1",
  createdAt: T0,
  updatedAt: T0,
  id: "hst_1",
  label: "host hst_1",
  ownerPrincipalId: "prin_1",
  state: "online",
  transport: { mode: "outbound-wss", endpointRef: "ep_1" },
} as unknown as Host;

function interaction(over: Partial<Interaction> & { id: Id }): Interaction {
  return {
    revision: "1",
    createdAt: T1,
    updatedAt: T1,
    runId: null,
    hostId: "hst_1",
    kind: "approval",
    requestKey: {
      native: { type: "rpc", valueType: "string", value: "tool" },
      processGeneration: "1",
      runGeneration: "1",
      connectionEpoch: "hst_1",
    },
    requestVersion: "1",
    state: "pending",
    blocking: true,
    answerable: false,
    carrier: "claude-control",
    request: {
      kind: "approval",
      title: over.id === "itx_super" ? "Write" : "Bash",
      description: over.id === "itx_super" ? "/tmp/other" : "rm -rf /tmp/old",
      toolCallId: null,
      actionRef: "act_1",
      options: [
        { id: "allow-once", label: "允许一次", effect: "allow-once", nativeValueRef: "act_1" },
        { id: "deny", label: "拒绝", effect: "deny", nativeValueRef: "act_1" },
      ],
      requestedPermissionsRef: null,
      inputDigest: "sha256:ab",
    },
    deadline: unknownKnowledge("none"),
    deadlineSource: "none",
    answer: na(),
    delivery: "not-sent",
    resolution: na(),
    ...over,
  } as Interaction;
}

const expired = interaction({
  id: "itx_exp",
  instanceId: "ins_1",
  state: "expired",
});
const superseded = interaction({
  id: "itx_super",
  instanceId: "ins_1",
  state: "answer-committed",
  answer: known({
    commandId: "cmd_1",
    actor: { principalId: "prin_1", type: "human", deviceId: "dev_other", instanceId: null },
    value: { kind: "approval", optionId: "allow-once", inputDigest: "sha256:ab" },
    committedAt: T1,
  }),
});
const newExpired = interaction({ id: "itx_new", instanceId: "ins_1", state: "expired" });

function departedOf(items: Interaction[]): ApprovalRow[] {
  return deriveApprovalRows(
    {
      interactions: items,
      instances: [inst("ins_1")],
      hosts: [host],
      answering: {},
      deviceId: "dev_1",
      workspaceLabel: () => "sfe-root",
    },
    { kind: "all", hostId: "", workspaceId: "", focus: null },
  ).departed;
}

const flush = () => new Promise<void>((resolve) => setTimeout(resolve, 0));

describe("DepartedList keyed list across a poll", () => {
  afterEach(() => vi.restoreAllMocks());

  it("does not re-render unchanged departed rows when one new interaction arrives", async () => {
    const counts = new Map<string, number>();
    const onRowRender = (id: string) => counts.set(id, (counts.get(id) ?? 0) + 1);
    const workspaceLabel = () => "sfe-root";

    // Initial commit: two departed rows.
    const first = departedOf([expired, superseded]);
    expect(first.map((r) => r.item.id)).toEqual(["itx_exp", "itx_super"]);
    const { rerender, unmount } = render(
      <DepartedList rows={first} workspaceLabel={workspaceLabel} onRowRender={onRowRender} />,
    );
    await act(flush);
    const expMounts = counts.get("itx_exp") ?? 0;
    const superMounts = counts.get("itx_super") ?? 0;
    expect(expMounts).toBeGreaterThan(0);
    expect(superMounts).toBeGreaterThan(0);

    // A 2 s poll: one NEW interaction plus fresh re-parses (deep-equal, new
    // identity) of the two existing interactions.
    const exp2 = JSON.parse(JSON.stringify(expired)) as Interaction;
    const super2 = JSON.parse(JSON.stringify(superseded)) as Interaction;
    const polled = departedOf([newExpired, exp2, super2]);
    expect(polled.map((r) => r.item.id)).toEqual(["itx_new", "itx_exp", "itx_super"]);
    act(() =>
      rerender(<DepartedList rows={polled} workspaceLabel={workspaceLabel} onRowRender={onRowRender} />),
    );
    await act(flush);

    // The new row commits; the two unchanged rows do not render again.
    expect(counts.get("itx_new")).toBeGreaterThan(0);
    expect(counts.get("itx_exp")).toBe(expMounts);
    expect(counts.get("itx_super")).toBe(superMounts);

    // A genuinely changed existing row (now answered elsewhere) DOES commit.
    const superChanged: Interaction = {
      ...JSON.parse(JSON.stringify(superseded)),
      resolution: known({ reason: "native-cleared", eventIds: [] }),
    };
    const changed = departedOf([newExpired, exp2, superChanged]);
    act(() =>
      rerender(<DepartedList rows={changed} workspaceLabel={workspaceLabel} onRowRender={onRowRender} />),
    );
    await act(flush);
    expect(counts.get("itx_super")).toBe(superMounts + 1);

    unmount();
  });
});
