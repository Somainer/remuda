import { renderHook } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { hubStore } from "../../lib/store";
import type { Id, U64 } from "../../types/wire";
import { SEEN_RENEW_MS, useSeenReporter } from "./useSeenReporter";

let visibility: DocumentVisibilityState = "visible";

function setVisibility(next: DocumentVisibilityState) {
  visibility = next;
  document.dispatchEvent(new Event("visibilitychange"));
}

beforeEach(() => {
  vi.useFakeTimers();
  visibility = "visible";
  vi.spyOn(document, "visibilityState", "get").mockImplementation(() => visibility);
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

type Props = { id?: Id; seq?: U64; unseen: boolean };

function mount(initial: Props) {
  const markSeen = vi.spyOn(hubStore, "markSeen").mockResolvedValue(null);
  const hook = renderHook((props: Props) => useSeenReporter(props.id, props.seq, props.unseen), {
    initialProps: initial,
  });
  return { markSeen, hook };
}

it("reports the session visible on open, renews while on screen, and releases on hide and leave", () => {
  const { markSeen, hook } = mount({ id: "ins_a" as Id, seq: "4" as U64, unseen: true });
  expect(markSeen).toHaveBeenCalledOnce();
  expect(markSeen).toHaveBeenLastCalledWith("ins_a", { durableSeq: "4", visible: true });

  // Renewals carry the latest durable seq.
  hook.rerender({ id: "ins_a" as Id, seq: "9" as U64, unseen: true });
  markSeen.mockClear();
  vi.advanceTimersByTime(SEEN_RENEW_MS);
  expect(markSeen).toHaveBeenCalledWith("ins_a", { durableSeq: "9", visible: true });

  // Hidden: release once, and no renewals while hidden.
  markSeen.mockClear();
  setVisibility("hidden");
  expect(markSeen).toHaveBeenCalledOnce();
  expect(markSeen).toHaveBeenLastCalledWith("ins_a", { durableSeq: "9", visible: false });
  vi.advanceTimersByTime(SEEN_RENEW_MS * 3);
  expect(markSeen).toHaveBeenCalledOnce();

  // Back on screen: visible again (this also clears a turn that ended meanwhile).
  setVisibility("visible");
  expect(markSeen).toHaveBeenLastCalledWith("ins_a", { durableSeq: "9", visible: true });

  // Unloading the page (close, reload, full navigation) releases it too.
  markSeen.mockClear();
  window.dispatchEvent(new Event("pagehide"));
  expect(markSeen).toHaveBeenCalledOnce();
  expect(markSeen).toHaveBeenLastCalledWith("ins_a", { durableSeq: "9", visible: false });

  // Leaving the route releases the lease.
  markSeen.mockClear();
  hook.unmount();
  expect(markSeen).toHaveBeenCalledOnce();
  expect(markSeen).toHaveBeenLastCalledWith("ins_a", { durableSeq: "9", visible: false });
  vi.advanceTimersByTime(SEEN_RENEW_MS * 2);
  window.dispatchEvent(new Event("pagehide"));
  expect(markSeen).toHaveBeenCalledOnce();
});

it("reports a deep-linked row when it arrives and a turn end that shows up unseen on screen", () => {
  const { markSeen, hook } = mount({ id: "ins_b" as Id, seq: undefined, unseen: false });
  expect(markSeen).not.toHaveBeenCalled();
  hook.rerender({ id: "ins_b" as Id, seq: "3" as U64, unseen: false });
  expect(markSeen).toHaveBeenCalledWith("ins_b", { durableSeq: "3", visible: true });
  markSeen.mockClear();
  hook.rerender({ id: "ins_b" as Id, seq: "6" as U64, unseen: true });
  expect(markSeen).toHaveBeenCalledWith("ins_b", { durableSeq: "6", visible: true });

  // Switching sessions releases the old one (at its own seq) and opens the new one.
  markSeen.mockClear();
  hook.rerender({ id: "ins_c" as Id, seq: "2" as U64, unseen: false });
  expect(markSeen.mock.calls).toEqual([
    ["ins_b", { durableSeq: "6", visible: false }],
    ["ins_c", { durableSeq: "2", visible: true }],
  ]);
});

it("reports nothing without a session or while the page starts hidden", () => {
  visibility = "hidden";
  const { markSeen, hook } = mount({ id: "ins_d" as Id, seq: "5" as U64, unseen: true });
  vi.advanceTimersByTime(SEEN_RENEW_MS * 2);
  expect(markSeen).not.toHaveBeenCalled();
  hook.rerender({ id: undefined, seq: "5" as U64, unseen: true });
  // Leaving ins_d still releases it (best-effort, harmless when none held).
  expect(markSeen).toHaveBeenCalledWith("ins_d", { durableSeq: "5", visible: false });
  markSeen.mockClear();
  setVisibility("visible");
  vi.advanceTimersByTime(SEEN_RENEW_MS * 2);
  expect(markSeen).not.toHaveBeenCalled();
});
