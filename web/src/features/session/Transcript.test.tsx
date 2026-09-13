import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { buildLongObservations } from "../../fixtures/session/longEvents";
import type { Id } from "../../types/wire";
import { Transcript } from "./Transcript";

describe("Transcript", () => {
  it("fills an empty completed message from delayed history without remounting its bubble", () => {
    const base = buildLongObservations({ instanceId: "ins_stream", journalId: "obj_stream", hostId: "hst_1", count: 1 })[0];
    if (base.kind !== "message") throw new Error("expected a message fixture");
    const open = { ...base, payload: { ...base.payload, status: "streaming" as const, blocks: [{ type: "text" as const, text: "hello from web hub" }] } };
    const close = {
      ...base, eventId: "evt_close", seq: "2",
      payload: { ...base.payload, messageId: "renamed", operation: "close" as const, revision: "2", baseRevision: "1", blocks: [] },
    };
    const { rerender } = render(<Transcript events={[close]} compact={false} />);
    const bubble = screen.getByTestId("message");
    expect(bubble).toHaveTextContent("You");
    rerender(<Transcript events={[close, open]} compact={false} />);
    expect(screen.getAllByTestId("message")).toHaveLength(1);
    expect(screen.getByTestId("message")).toBe(bubble);
    expect(bubble).toHaveTextContent("hello from web hub");
  });

  it("updates the same assistant bubble while stream revisions arrive", () => {
    const base = buildLongObservations({ instanceId: "ins_stream", journalId: "obj_stream", hostId: "hst_1", count: 1 })[0];
    if (base.kind !== "message") throw new Error("expected a message fixture");
    const open = { ...base, payload: { ...base.payload, role: "assistant" as const, status: "streaming" as const, blocks: [{ type: "text" as const, text: "我" }] } };
    const append = {
      ...open, eventId: "evt_append", seq: "2",
      payload: { ...open.payload, operation: "append" as const, revision: "2", baseRevision: "1", targetBlock: 0, blocks: [{ type: "text" as const, text: "先看看" }] },
    };
    const close = {
      ...open, eventId: "evt_close", seq: "3",
      payload: { ...open.payload, operation: "close" as const, revision: "3", baseRevision: "2", status: "complete" as const, blocks: [{ type: "text" as const, text: "我先看看" }] },
    };
    const { rerender } = render(<Transcript events={[open]} compact={false} />);
    const bubble = screen.getByTestId("message");
    expect(bubble).toHaveTextContent("我");
    rerender(<Transcript events={[open, append]} compact={false} />);
    expect(screen.getAllByTestId("message")).toHaveLength(1);
    expect(screen.getByTestId("message")).toBe(bubble);
    expect(bubble).toHaveTextContent("我先看看");
    rerender(<Transcript events={[open, append, close]} compact={false} />);
    expect(screen.getAllByTestId("message")).toHaveLength(1);
    expect(screen.getByTestId("message")).toBe(bubble);
    expect(bubble).toHaveTextContent("我先看看");
  });

  it("virtualizes a 2000-event fixture", () => {
    const events = buildLongObservations({
      instanceId: "ins_long" as Id,
      journalId: "obj_long" as Id,
      hostId: "hst_1" as Id,
      count: 2000,
    });
    render(<Transcript events={events} compact={false} />);
    expect(screen.getByTestId("transcript")).toBeTruthy();
    expect(screen.getByTestId("transcript-scroller")).toBeTruthy();
    expect(screen.getAllByTestId("transcript-row").length).toBeLessThan(80);
    expect(screen.getAllByTestId("transcript-row").length).toBeGreaterThan(0);
  });

  it("moves the active turn with j/k", async () => {
    const user = userEvent.setup();
    const events = buildLongObservations({
      instanceId: "ins_long" as Id,
      journalId: "obj_long" as Id,
      hostId: "hst_1" as Id,
      count: 8,
    });
    const { container } = render(<Transcript events={events} compact={false} />);
    await user.keyboard("j");
    expect(container.querySelector('[data-turn-active="1"]')).toBeTruthy();
    await user.keyboard("k");
    expect(container.querySelector("[data-anchor]")).toBeTruthy();
  });
});
