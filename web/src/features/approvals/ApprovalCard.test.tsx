import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { describe, expect, it, vi } from "vitest";
import { ApprovalCard, DecisionCard, type DecisionView } from "./ApprovalCard";
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

/* ------------------------------------------------------------------ */
/* Unified DecisionCard: the one card the desktop centre and compact   */
/* inbox both render (ui-spec §2.5).                                   */
/* ------------------------------------------------------------------ */

function cnItem(overrides: Partial<Interaction> = {}): Interaction {
  return {
    id: "int_1",
    instanceId: "ins_1",
    runId: null,
    hostId: "hst_1",
    kind: "approval",
    requestVersion: "1",
    state: "pending",
    blocking: true,
    answerable: true,
    carrier: "harness-hook",
    request: {
      kind: "approval",
      title: "Bash",
      description: "rm -rf /tmp/coord-media",
      toolCallId: null,
      actionRef: "act_1",
      options: [
        { id: "allow-once", label: "允许一次", effect: "allow-once", nativeValueRef: "act_1" },
        { id: "allow-session", label: "始终允许 (acceptEdits)", effect: "allow-session", nativeValueRef: "act_1" },
        { id: "deny", label: "拒绝", effect: "deny", nativeValueRef: "act_1" },
      ],
      requestedPermissionsRef: null,
      inputDigest: "sha256:ab",
    },
    deadline: { state: "unknown", reason: "none", evidenceEventIds: [] },
    deadlineSource: "none",
    answer: { state: "not-applicable" },
    delivery: "not-sent",
    resolution: { state: "not-applicable" },
    ...overrides,
  } as Interaction;
}

function viewFor(item: Interaction, over: Partial<DecisionView> = {}): DecisionView {
  return {
    key: item.id,
    sig: `sig:${item.id}`,
    item,
    uiState: "pending",
    focused: false,
    timeLabel: "12:04",
    hostLabel: "bolt",
    workspaceLabel: "sfe-root",
    instanceKind: "claude",
    ...over,
  };
}

function renderCard(view: DecisionView, mode: "desktop" | "compact" = "desktop", onRespond = vi.fn()) {
  return render(
    <MemoryRouter>
      <DecisionCard view={view} mode={mode} onRespond={onRespond} />
    </MemoryRouter>,
  );
}

describe("DecisionCard unified across desktop and compact", () => {
  it("renders the same approval-row with raw option labels in both modes", () => {
    for (const mode of ["desktop", "compact"] as const) {
      const view = viewFor(cnItem());
      const { unmount } = renderCard(view, mode);
      expect(screen.getByTestId("approval-row")).toBeInTheDocument();
      expect(screen.getByRole("button", { name: "允许一次" })).toBeInTheDocument();
      expect(screen.getByRole("button", { name: "始终允许 (acceptEdits)" })).toBeInTheDocument();
      expect(screen.getByRole("button", { name: "拒绝" })).toBeInTheDocument();
      expect(screen.getByRole("link", { name: "打开会话" })).toBeInTheDocument();
      unmount();
    }
  });

  it("submits the harness option verbatim with the shown digest", async () => {
    const onRespond = vi.fn();
    renderCard(viewFor(cnItem()), "desktop", onRespond);
    await userEvent.click(screen.getByRole("button", { name: "允许一次" }));
    expect(onRespond).toHaveBeenCalledWith(
      expect.objectContaining({ id: "int_1" }),
      { kind: "approval", optionId: "allow-once", inputDigest: "sha256:ab" },
    );
  });

  it("never renders confidence/risk/session-boundary wording and restates the grant scope", () => {
    renderCard(viewFor(cnItem()));
    expect(screen.queryByText("置信度")).toBeNull();
    expect(screen.queryByText("风险")).toBeNull();
    expect(screen.queryByText(/本会话/)).toBeNull();
    // The allow-session grant is restated in the harness's own scope words.
    expect(screen.getByText("按 harness 建议的范围持续允许")).toBeInTheDocument();
  });

  it("renders an empty deadline as an em dash", () => {
    renderCard(viewFor(cnItem()));
    expect(screen.getByTestId("approval-deadline")).toHaveTextContent("—");
  });

  it("disables the controls and shows 已提交 · 等待确认 while answering", () => {
    renderCard(viewFor(cnItem(), { uiState: "answering" }));
    const submitting = screen.getByTestId("approval-submitting");
    expect(submitting).toBeDisabled();
    expect(submitting).toHaveTextContent("已提交 · 等待确认");
    // The option buttons unmount, so no second answer can be POSTed.
    expect(screen.queryByRole("button", { name: "允许一次" })).toBeNull();
    expect(screen.queryByRole("button", { name: "拒绝" })).toBeNull();
  });

  it("disables every option and names the pause when the host is offline", () => {
    renderCard(viewFor(cnItem(), { uiState: "paused" }));
    for (const name of ["允许一次", "始终允许 (acceptEdits)", "拒绝"]) {
      expect(screen.getByRole("button", { name })).toBeDisabled();
    }
    expect(screen.getByTestId("m-inbox-paused")).toHaveTextContent("主机离线，交互暂停");
  });

  it("answers hook questions inline on desktop but via 去回答 on compact", () => {
    const question = cnItem({
      kind: "question",
      carrier: "harness-hook",
      request: {
        kind: "question",
        title: "AskUserQuestion",
        fields: [
          {
            id: "q0",
            title: "下一步",
            description: "下一步",
            input: "single-select",
            required: true,
            options: [{ id: "a", label: "A" }],
            allowFreeText: false,
            sensitive: false,
          },
        ],
      },
    });

    const desktop = renderCard(viewFor(question), "desktop");
    expect(desktop.getByTestId("question-form")).toBeInTheDocument();
    expect(desktop.queryByRole("link", { name: "去回答" })).toBeNull();
    desktop.unmount();

    const compact = renderCard(viewFor(question), "compact");
    expect(compact.queryByTestId("question-form")).toBeNull();
    const go = compact.getByRole("link", { name: "去回答" });
    expect(go).toHaveAttribute("href", "/s/ins_1");
  });
});
