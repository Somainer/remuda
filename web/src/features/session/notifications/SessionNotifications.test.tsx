import { afterEach, describe, expect, it } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import type { Observation } from "../../../types/generated";
import { SessionNotifications } from "./SessionNotifications";
import { __resetDismissedForTests } from "./dismissed";

function notification(seq: number, related: Record<string, string>): Observation {
  return {
    kind: "lifecycle",
    eventId: `ev_${seq}`,
    journalId: "jrn_1",
    instanceId: "ins_1",
    hostId: "hos_1",
    processGeneration: "1",
    runGeneration: "1",
    runId: "run_1",
    seq: String(seq),
    observedAt: "2026-09-18T17:03:01.000Z",
    nativeAt: { state: "not-applicable" },
    source: {} as Observation["source"],
    completeness: "structured",
    rawRef: null,
    evidenceEventIds: [],
    schemaVersion: 1,
    payload: {
      type: "native",
      topic: "hook",
      nativeName: "Notification",
      nativeId: { state: "not-applicable" },
      status: { state: "known", value: "observed" },
      relatedIds: related,
      dataRef: null,
      severity: "info",
      affectsCompletion: false,
    },
  } as unknown as Observation;
}

function instanceExit(seq: number): Observation {
  return {
    kind: "lifecycle",
    eventId: `ev_${seq}`,
    journalId: "jrn_1",
    instanceId: "ins_1",
    hostId: "hos_1",
    processGeneration: "1",
    runGeneration: "1",
    runId: "run_1",
    seq: String(seq),
    observedAt: "2026-09-18T18:03:01.000Z",
    nativeAt: { state: "not-applicable" },
    source: {} as Observation["source"],
    completeness: "structured",
    rawRef: null,
    evidenceEventIds: [],
    schemaVersion: 1,
    payload: {
      type: "entity",
      entityType: "instance",
      entityId: "ins_1",
      state: "exited",
      revision: "2",
      previousState: "ready",
      reasonCode: "explicit-close",
      evidenceEventIds: [],
      entity: {},
    },
  } as unknown as Observation;
}

function interactionEvent(seq: number, kind: "interaction.requested" | "interaction.answered", id = "int_1"): Observation {
  const payload =
    kind === "interaction.requested"
      ? { interaction: { id, state: "pending" } }
      : { interactionId: id };
  return {
    kind,
    eventId: `ev_${seq}`,
    journalId: "jrn_1",
    instanceId: "ins_1",
    hostId: "hos_1",
    processGeneration: "1",
    runGeneration: "1",
    runId: "run_1",
    seq: String(seq),
    observedAt: "2026-09-18T17:10:00.000Z",
    nativeAt: { state: "not-applicable" },
    source: {} as Observation["source"],
    completeness: "structured",
    rawRef: null,
    evidenceEventIds: [],
    schemaVersion: 1,
    payload,
  } as unknown as Observation;
}

describe("SessionNotifications", () => {
  afterEach(() => __resetDismissedForTests());

  it("renders nothing when there are no notifications", () => {
    const { container } = render(<SessionNotifications instanceId="ins" events={[]} />);
    expect(container).toBeEmptyDOMElement();
  });

  it("lists an advisory with its text and time, and dismiss removes it", () => {
    const events = [
      notification(101, { notificationType: "idle_prompt", message: "waiting for your input" }),
    ];
    const { rerender } = render(<SessionNotifications instanceId="ins" events={events} />);
    const rows = screen.getAllByTestId("session-notification");
    expect(rows).toHaveLength(1);
    expect(rows[0]!.textContent).toContain("waiting for your input");
    expect(screen.queryByTestId("notification-toast")).toBeNull();

    fireEvent.click(screen.getByTestId("notification-dismiss"));
    rerender(<SessionNotifications instanceId="ins" events={events} />);
    expect(screen.queryByTestId("session-notification")).toBeNull();
  });

  it("pops a toast only for a notification arriving after mount", () => {
    const { rerender } = render(<SessionNotifications instanceId="ins" events={[]} />);
    expect(screen.queryByTestId("notification-toast")).toBeNull();
    rerender(
      <SessionNotifications
        instanceId="ins"
        events={[notification(202, { message: "a fresh advisory" })]}
      />,
    );
    expect(screen.getByTestId("notification-toast").textContent).toContain("a fresh advisory");
  });

  it("links a permission_prompt notification to the pending dialog while it is open", () => {
    const events = [
      interactionEvent(301, "interaction.requested", "int_perm"),
      notification(303, { notificationType: "permission_prompt", message: "Tool permission" }),
    ];
    render(<SessionNotifications instanceId="ins" events={events} />);
    expect(screen.getByTestId("notification-goto-dialog")).toBeTruthy();
  });

  it("collapses a permission_prompt row once the dialog has been answered", () => {
    const events = [
      interactionEvent(401, "interaction.requested"),
      notification(402, { notificationType: "permission_prompt", message: "Tool permission" }),
      interactionEvent(403, "interaction.answered"),
    ];
    const { container } = render(<SessionNotifications instanceId="ins" events={events} />);
    expect(container).toBeEmptyDOMElement();
  });

  it("shows only the newest row and expands the rest inline with +N", () => {
    const events = [
      notification(501, { message: "first" }),
      notification(502, { message: "second" }),
      notification(503, { message: "third" }),
    ];
    render(<SessionNotifications instanceId="ins" events={events} />);
    expect(screen.getAllByTestId("session-notification")).toHaveLength(1);
    expect(screen.getByTestId("session-notification").textContent).toContain("third");
    const more = screen.getByTestId("notification-older");
    expect(more.textContent).toBe("+2");
    fireEvent.click(more);
    const rows = screen.getAllByTestId("session-notification");
    expect(rows).toHaveLength(3);
    expect(rows[0]!.textContent).toContain("first");
    fireEvent.click(screen.getByTestId("notification-collapse"));
    expect(screen.getAllByTestId("session-notification")).toHaveLength(1);
  });

  it("on an ended session: one muted quiet row, no dialog link, no toast for a new advisory", () => {
    const events = [
      interactionEvent(601, "interaction.requested"),
      notification(602, { notificationType: "permission_prompt", message: "old permission" }),
      notification(603, { notificationType: "idle_prompt", message: "waiting for your input" }),
      instanceExit(604),
    ];
    render(<SessionNotifications instanceId="ins" events={events} />);
    const panel = screen.getByTestId("session-notifications");
    expect(panel.getAttribute("data-settled")).toBe("1");
    const rows = screen.getAllByTestId("session-notification");
    expect(rows).toHaveLength(1);
    expect(rows[0]!.textContent).toContain("waiting for your input");
    expect(rows[0]!.getAttribute("data-settled")).toBe("1");
    expect(screen.queryByTestId("notification-goto-dialog")).toBeNull();
    expect(screen.queryByTestId("notification-toast")).toBeNull();
  });

  it("drops a visible toast when the session ends", () => {
    const { rerender } = render(<SessionNotifications instanceId="ins" events={[]} />);
    rerender(
      <SessionNotifications
        instanceId="ins"
        events={[notification(701, { message: "just a moment ago" })]}
      />,
    );
    expect(screen.getByTestId("notification-toast")).toBeTruthy();
    rerender(
      <SessionNotifications
        instanceId="ins"
        events={[notification(701, { message: "just a moment ago" }), instanceExit(702)]}
      />,
    );
    expect(screen.queryByTestId("notification-toast")).toBeNull();
  });
});
