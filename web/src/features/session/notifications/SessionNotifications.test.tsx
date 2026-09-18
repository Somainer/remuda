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

  it("links a permission_prompt notification to the pending dialog", () => {
    const events = [
      notification(303, { notificationType: "permission_prompt", message: "Tool permission" }),
    ];
    render(<SessionNotifications instanceId="ins" events={events} />);
    expect(screen.getByTestId("notification-goto-dialog")).toBeTruthy();
  });
});
