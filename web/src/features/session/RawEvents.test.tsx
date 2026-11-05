import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { Observation } from "../../types/generated";
import { RawEvents } from "./RawEvents";

/** cb-search: `/s/:id/events?seq=N` lands on row N with its envelope open. */

function event(seq: number): Observation {
  return {
    eventId: `evt_${seq}`,
    seq: String(seq),
    kind: seq % 2 ? "message" : "lifecycle",
    completeness: "structured",
    source: { channel: "stdout", driverKind: "claude-sdk", delivery: "live" },
    payload: { seq },
  } as unknown as Observation;
}

const events = Array.from({ length: 200 }, (_, i) => event(i + 1));

describe("RawEvents focus", () => {
  it("scrolls to the focused seq, marks it and opens its envelope", () => {
    render(<RawEvents events={events} focusSeq="150" />);
    const list = screen.getByTestId("raw-events-list");
    expect(list.scrollTop).toBe(147 * 56);
    const row = screen
      .getAllByTestId("raw-event-row")
      .find((element) => element.getAttribute("data-seq") === "150");
    expect(row?.getAttribute("aria-current")).toBe("true");
    expect(screen.getByTestId("raw-event-json").textContent).toContain("evt_150");
    expect(screen.queryByTestId("raw-events-focus-missing")).toBeNull();
  });

  it("names a seq older than the loaded rows", () => {
    render(<RawEvents events={events.slice(100)} focusSeq="12" />);
    expect(screen.getByTestId("raw-events-focus-missing").textContent).toBe("seq 12 不在已加载的记录里。");
  });

  it("without a focus nothing is opened", () => {
    render(<RawEvents events={events} />);
    expect(screen.queryByTestId("raw-event-json")).toBeNull();
  });
});
