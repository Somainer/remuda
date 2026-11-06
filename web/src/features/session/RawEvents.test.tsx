import { act, render, screen } from "@testing-library/react";
import { useState } from "react";
import { describe, expect, it } from "vitest";
import type { Observation } from "../../types/generated";
import type { EarlierWindow } from "../../lib/journal";
import { FOCUS_MAX_EARLIER_PAGES, RawEvents } from "./RawEvents";

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

  it("names a seq older than the loaded rows when it cannot page", () => {
    render(<RawEvents events={events.slice(100)} focusSeq="12" />);
    expect(screen.getByTestId("raw-events-focus-missing").textContent).toBe("seq 12 不在已加载的记录里。");
  });

  it("without a focus nothing is opened", () => {
    render(<RawEvents events={events} />);
    expect(screen.queryByTestId("raw-event-json")).toBeNull();
  });
});

/**
 * cb-search r2: a hit below the loaded window pages earlier windows until
 * its row is loaded (the store prepends them to `events`), then focuses it.
 */
type Page = { rows: Observation[]; end: boolean } | null;

/** The test owns the page queue; the component only asks for the next one. */
function queue(pages: Page[]) {
  const log = { count: 0 };
  const next = (): Page => {
    const page = pages[log.count] ?? null;
    log.count += 1;
    return page;
  };
  return { log, next };
}

function Paged({ initial, next, focusSeq }: { initial: Observation[]; next: () => Page; focusSeq: string }) {
  const [rows, setRows] = useState(initial);
  const loadEarlier = async (): Promise<EarlierWindow | null> => {
    const page = next();
    if (!page) return null;
    setRows((held) => [...page.rows, ...held]);
    return { prepended: page.rows.length > 0, end: page.end, floor: page.rows[0]?.seq ?? null };
  };
  return <RawEvents events={rows} focusSeq={focusSeq} loadEarlier={loadEarlier} />;
}

describe("RawEvents focus below the loaded window", () => {
  const all = Array.from({ length: 300 }, (_, i) => event(i + 1));

  it("pages earlier windows until the row is loaded, then focuses it", async () => {
    const { log: calls, next } = queue([
      { rows: all.slice(100, 200), end: false },
      { rows: all.slice(0, 100), end: true },
    ]);
    render(<Paged initial={all.slice(200)} next={next} focusSeq="12" />);
    // While paging, the tab says so instead of naming the row missing.
    expect(screen.getByTestId("raw-events-focus-paging").textContent).toContain("seq 12");
    const row = await screen.findByText((_, el) => el?.getAttribute("data-seq") === "12" && el.getAttribute("aria-current") === "true");
    expect(row).toBeTruthy();
    expect(calls.count).toBe(2);
    // The envelope opens in the focus effect's own commit.
    expect((await screen.findByTestId("raw-event-json")).textContent).toContain("evt_12");
    expect(screen.queryByTestId("raw-events-focus-missing")).toBeNull();
    expect(screen.queryByTestId("raw-events-focus-paging")).toBeNull();
  });

  it("names the row missing only once the history is exhausted", async () => {
    const { log: calls, next } = queue([{ rows: all.slice(150, 200), end: true }]);
    render(<Paged initial={all.slice(200)} next={next} focusSeq="12" />);
    expect(await screen.findByTestId("raw-events-focus-missing")).toBeTruthy();
    expect(calls.count).toBe(1);
    expect(screen.queryByTestId("raw-events-focus-paging")).toBeNull();
  });

  it("stops after a bounded number of pages that never reach the row", async () => {
    // Every page re-serves nothing new and never ends.
    const { log: calls, next } = queue(
      Array.from({ length: FOCUS_MAX_EARLIER_PAGES + 10 }, () => ({ rows: [], end: false })),
    );
    render(<Paged initial={all.slice(200)} next={next} focusSeq="12" />);
    expect(await screen.findByTestId("raw-events-focus-missing")).toBeTruthy();
    await act(async () => {
      await Promise.resolve();
    });
    expect(calls.count).toBe(FOCUS_MAX_EARLIER_PAGES);
  });

  it("does not page for a seq above the loaded window", async () => {
    const { log: calls, next } = queue([]);
    render(<Paged initial={all.slice(0, 100)} next={next} focusSeq="250" />);
    expect(screen.getByTestId("raw-events-focus-missing")).toBeTruthy();
    await act(async () => {
      await Promise.resolve();
    });
    expect(calls.count).toBe(0);
  });
});
