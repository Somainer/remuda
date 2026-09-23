import { act } from "react";
import { useState } from "react";
import { fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { useIncrementalLimit } from "./useIncrementalLimit";

/**
 * Component-level regression for the shrink bug: a free-text answer draft is
 * component state (like QuestionForm / ElicitationCard keep). If answering
 * another card reset the progressive limit, later cards unmounted and their
 * drafts vanished. The card must stay mounted on a plain shrink.
 */

// Deterministic rAF queue (same stub the hook unit test uses).
const pending = new Map<number, FrameRequestCallback>();
let nextHandle = 1;
vi.stubGlobal(
  "requestAnimationFrame",
  vi.fn((cb: FrameRequestCallback) => {
    const handle = nextHandle++;
    pending.set(handle, cb);
    return handle;
  }),
);
vi.stubGlobal(
  "cancelAnimationFrame",
  vi.fn((handle: number) => {
    pending.delete(handle);
  }),
);

function drainFrames() {
  while (pending.size > 0) {
    const handle = pending.keys().next().value as number;
    const cb = pending.get(handle)!;
    pending.delete(handle);
    act(() => cb(0));
  }
}

afterEach(() => {
  pending.clear();
  nextHandle = 1;
});

function DraftCard({ id }: { id: string }) {
  // Local draft state: lost if the card unmounts, exactly like the real
  // QuestionForm/ElicitationCard answer draft.
  const [draft, setDraft] = useState("");
  return (
    <input
      aria-label={`draft ${id}`}
      value={draft}
      onChange={(event) => setDraft(event.target.value)}
    />
  );
}

function FloodList({ items, resetKey = "all" }: { items: string[]; resetKey?: string }) {
  const limit = useIncrementalLimit(items.length, { step: 12, resetKey });
  return (
    <div>
      {items.slice(0, limit).map((id) => (
        <DraftCard key={id} id={id} />
      ))}
    </div>
  );
}

describe("useIncrementalLimit draft survival", () => {
  it("keeps a draft typed into card 15 when another card is answered", () => {
    const twenty = Array.from({ length: 20 }, (_, i) => `itx_${i + 1}`);
    const { rerender } = render(<FloodList items={twenty} />);
    drainFrames();
    expect(screen.getByLabelText("draft itx_20")).toBeInTheDocument();

    const card15 = screen.getByLabelText("draft itx_15");
    fireEvent.change(card15, { target: { value: "half-written free-text answer" } });
    expect((card15 as HTMLInputElement).value).toBe("half-written free-text answer");

    // A DIFFERENT card (itx_5) is answered and leaves the queue: 19 remain.
    const nineteen = twenty.filter((id) => id !== "itx_5");
    rerender(<FloodList items={nineteen} />);

    const card15After = screen.getByLabelText("draft itx_15") as HTMLInputElement;
    expect(card15After.value).toBe("half-written free-text answer");
  });

  it("restarting on a filter change is allowed to drop the old drafts", () => {
    const items = Array.from({ length: 20 }, (_, i) => `itx_${i + 1}`);
    const { rerender } = render(<FloodList items={items} resetKey="all" />);
    drainFrames();
    fireEvent.change(screen.getByLabelText("draft itx_15"), {
      target: { value: "draft under all-filter" },
    });

    // Filter switch: a genuinely different set, slicing restarts from 12.
    rerender(<FloodList items={items} resetKey="approval" />);
    expect(screen.queryByLabelText("draft itx_15")).not.toBeInTheDocument();
  });
});
