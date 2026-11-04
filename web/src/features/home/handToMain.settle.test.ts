import { describe, expect, it, vi } from "vitest";
import type { LocalBubble } from "../../lib/store";
import type { Id } from "../../types/wire";
import { handToMain, type HandOffStore } from "./handToMain";

/**
 * 交给 Main's receipt is the command's SETTLED outcome, not the outbox
 * enqueue (r2 item 4): accepted only when accepted, queued while undelivered,
 * rejected with the reason, unknown reconciled through GET commands/{id}.
 */

function fakeStore(options: {
  push?: (instanceId: Id, prompt: string) => LocalBubble;
  sendResult?: boolean;
  reconcile?: "sent" | "held" | "rejected" | "unknown";
}) {
  let bubbles: LocalBubble[] = [];
  const listeners = new Set<() => void>();
  const emit = () => {
    for (const listener of [...listeners]) listener();
  };
  const send = vi.fn(async (instanceId: Id, prompt: string) => {
    const pushed = options.push?.(instanceId, prompt);
    if (pushed) {
      bubbles = [...bubbles, pushed];
      emit();
    }
    return options.sendResult ?? true;
  });
  const reconcileSend = vi.fn(async () => options.reconcile ?? "unknown");
  const store: HandOffStore = {
    send,
    bubbles: () => bubbles,
    subscribe: (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    reconcileSend,
  };
  return {
    store,
    send,
    reconcileSend,
    replace(next: LocalBubble) {
      bubbles = bubbles.map((row) => (row.clientRequestId === next.clientRequestId ? next : row));
      emit();
    },
  };
}

function bubble(patch: Partial<LocalBubble> = {}): LocalBubble {
  return {
    clientRequestId: `cl_${Math.random().toString(36).slice(2)}` as Id,
    instanceId: "ins_main" as Id,
    text: "quote",
    commandId: "cmd_1" as Id,
    state: "queued",
    outboxState: "pending",
    createdAt: "2026-11-04T00:00:00.000Z",
    ...patch,
  } as LocalBubble;
}

describe("交给 Main settles the command before reporting it", () => {
  it("reports accepted only when the command is settled as accepted", async () => {
    const fake = fakeStore({ push: () => bubble({ state: "accepted", outboxState: "sent" }) });
    await expect(handToMain(fake.store, "ins_main", "quote", 50)).resolves.toMatchObject({
      kind: "accepted",
      commandId: "cmd_1",
    });
    expect(fake.send).toHaveBeenCalledTimes(1);
  });

  it("reports rejected with the Hub/Node's reason on a rejected row", async () => {
    const fake = fakeStore({
      push: () =>
        bubble({ state: "unknown", outboxState: "rejected", outboxError: "fenced: the chapter was superseded" }),
    });
    await expect(handToMain(fake.store, "ins_main", "quote", 50)).resolves.toMatchObject({
      kind: "rejected",
      reason: "fenced: the chapter was superseded",
    });
  });

  it("does not call a durable pending row accepted; it is queued", async () => {
    const fake = fakeStore({ push: () => bubble({ state: "queued", outboxState: "pending" }) });
    await expect(handToMain(fake.store, "ins_main", "quote", 10)).resolves.toMatchObject({ kind: "queued" });
  });

  it("does not accept a reconciling row: the Hub has no verdict yet (r3)", async () => {
    // store.ts's bubbleFromOutbox projects a reconciling row as
    // {state:"accepted", outboxState:"reconciling"} — the projection is not a
    // verdict, so the card must wait and then offer 核对.
    const fake = fakeStore({
      push: () => bubble({ state: "accepted", outboxState: "reconciling" }),
      reconcile: "unknown",
    });
    await expect(handToMain(fake.store, "ins_main", "quote", 20)).resolves.toMatchObject({ kind: "unknown" });
    expect(fake.reconcileSend).toHaveBeenCalled();
  });

  it("reconciles a reconciling row and accepts only a sent verdict (r3)", async () => {
    const fake = fakeStore({
      push: () => bubble({ state: "accepted", outboxState: "reconciling" }),
      reconcile: "sent",
    });
    await expect(handToMain(fake.store, "ins_main", "quote", 1000)).resolves.toMatchObject({
      kind: "accepted",
      commandId: "cmd_1",
    });
  });

  it("follows a later settlement through the store's subscription", async () => {
    const first = bubble({ state: "queued", outboxState: "pending" });
    const fake = fakeStore({ push: () => first });
    const pending = handToMain(fake.store, "ins_main", "quote", 1000);
    fake.replace({ ...first, state: "accepted", outboxState: "sent" });
    await expect(pending).resolves.toMatchObject({ kind: "accepted" });
  });

  it("reconciles an unknown outcome through GET commands/{id}", async () => {
    const fake = fakeStore({
      push: () => bubble({ state: "unknown", outboxState: "unknown" }),
      reconcile: "rejected",
    });
    await expect(handToMain(fake.store, "ins_main", "quote", 1000)).resolves.toMatchObject({
      kind: "rejected",
      commandId: "cmd_1",
    });
    expect(fake.reconcileSend).toHaveBeenCalledWith("cmd_1");
    // Reconciliation is a read: never a second send.
    expect(fake.send).toHaveBeenCalledTimes(1);
  });

  it("stays unknown when reconciliation cannot decide, without re-sending", async () => {
    const fake = fakeStore({
      push: () => bubble({ state: "unknown", outboxState: "unknown" }),
      reconcile: "unknown",
    });
    await expect(handToMain(fake.store, "ins_main", "quote", 20)).resolves.toMatchObject({ kind: "unknown" });
    expect(fake.send).toHaveBeenCalledTimes(1);
  });

  it("reports a failure that never produced a bubble as rejected", async () => {
    const fake = fakeStore({ sendResult: false, reconcile: "unknown" });
    await expect(handToMain(fake.store, "ins_main", "quote", 10)).resolves.toMatchObject({
      kind: "rejected",
      commandId: null,
    });
  });
});
