import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { ApprovalCard } from "./ApprovalCard";
import type { Interaction } from "../../types/interaction";

/// A hook-carried approval as the Node builds it from a real
/// `PermissionRequest` (D-028 §4.4; shapes measured in
/// docs/design/evidence/native-pty-5.md).
function hookApproval(overrides: Partial<Interaction> = {}): Interaction {
  return {
    id: "int_0199a1f0-0000-7000-8000-000000000000",
    instanceId: "ins_0199a1f0-0000-7000-8000-000000000001",
    runId: null,
    hostId: "hst_0199a1f0-0000-7000-8000-000000000002",
    kind: "approval",
    requestVersion: "1",
    state: "pending",
    blocking: true,
    answerable: true,
    carrier: "harness-hook",
    request: {
      kind: "approval",
      title: "Write",
      // The real tool input, not a screen scrape of it.
      description: "/tmp/probe.txt",
      toolCallId: null,
      actionRef: "obj_0199a1f0-0000-7000-8000-000000000003",
      options: [
        { id: "allow-once", label: "Allow once", effect: "allow-once", nativeValueRef: "obj_0199a1f0-0000-7000-8000-000000000004" },
        { id: "allow-always-0", label: "Always allow (acceptEdits)", effect: "allow-session", nativeValueRef: "obj_0199a1f0-0000-7000-8000-000000000005" },
        { id: "deny", label: "Deny", effect: "deny", nativeValueRef: "obj_0199a1f0-0000-7000-8000-000000000006" },
      ],
      requestedPermissionsRef: null,
      inputDigest: `sha256:${"a".repeat(64)}`,
    },
    deadline: { known: true, value: "2026-09-14T00:15:00.000Z" },
    deadlineSource: "runtime-policy",
    answer: { known: false, reason: "pending", evidenceEventIds: [] },
    delivery: "not-sent",
    resolution: { known: false, reason: "pending", evidenceEventIds: [] },
    ...overrides,
  } as Interaction;
}

describe("ApprovalCard with a hook-carried approval", () => {
  it("shows the real tool input rather than a screen scrape", () => {
    render(<ApprovalCard interaction={hookApproval()} onRespond={() => {}} />);
    expect(screen.getByText("/tmp/probe.txt")).toBeInTheDocument();
    expect(screen.getByText(/Write/)).toBeInTheDocument();
  });

  it("offers the always-allow button the harness suggested", async () => {
    // The button exists only because the PermissionRequest carried a
    // permission_suggestion; a request without one must not grow a grant
    // Remuda cannot honour.
    const onRespond = vi.fn();
    render(<ApprovalCard interaction={hookApproval()} onRespond={onRespond} />);
    await userEvent.click(screen.getByRole("button", { name: "Always allow (acceptEdits)" }));
    expect(onRespond).toHaveBeenCalledWith({
      kind: "approval",
      optionId: "allow-always-0",
      inputDigest: `sha256:${"a".repeat(64)}`,
    });
  });

  it("sends the option the human pressed, with the digest it was shown", async () => {
    // The digest is what stops a stale answer being replayed onto a request
    // whose input changed underneath it.
    const onRespond = vi.fn();
    render(<ApprovalCard interaction={hookApproval()} onRespond={onRespond} />);
    await userEvent.click(screen.getByRole("button", { name: "Allow once" }));
    expect(onRespond).toHaveBeenCalledWith({
      kind: "approval",
      optionId: "allow-once",
      inputDigest: `sha256:${"a".repeat(64)}`,
    });
    await userEvent.click(screen.getByRole("button", { name: "Deny" }));
    expect(onRespond).toHaveBeenLastCalledWith({
      kind: "approval",
      optionId: "deny",
      inputDigest: `sha256:${"a".repeat(64)}`,
    });
  });

  it("renders no always-allow button when the harness suggested nothing", () => {
    const request = hookApproval().request;
    if (request.kind !== "approval") throw new Error("fixture must be an approval");
    const interaction = hookApproval({
      request: {
        ...request,
        options: request.options.filter((option) => option.id !== "allow-always-0"),
      },
    });
    render(<ApprovalCard interaction={interaction} onRespond={() => {}} />);
    expect(screen.queryByRole("button", { name: /Always allow/ })).toBeNull();
    expect(screen.getByRole("button", { name: "Allow once" })).toBeInTheDocument();
  });

  it("cannot be answered once it is no longer answerable", () => {
    // A hook that stopped listening leaves a card nobody can act on; the
    // buttons have to say so rather than silently doing nothing.
    render(
      <ApprovalCard interaction={hookApproval({ answerable: false })} onRespond={() => {}} />,
    );
    for (const name of ["Allow once", "Always allow (acceptEdits)", "Deny"]) {
      expect(screen.getByRole("button", { name })).toBeDisabled();
    }
  });

  it("cannot be answered twice while a decision is in flight", () => {
    render(<ApprovalCard interaction={hookApproval()} busy onRespond={() => {}} />);
    expect(screen.getByRole("button", { name: "Allow once" })).toBeDisabled();
  });
});
