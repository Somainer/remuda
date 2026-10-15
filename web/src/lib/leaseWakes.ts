import type { Id } from "../types/wire";
import type { OutboxRecord } from "./outbox";

/**
 * c-reconnfu gate 20 — the SINGLE, owner-agnostic lease-wake policy.
 *
 * Three gates of per-path wake bookkeeping (gates 17→18→19) each closed one
 * hole only to expose another path where a deliverable row lost its only wake.
 * This table is the ONE authority every snapshot-holding path consults; the
 * caller (Outbox.reconcileLeaseWakes) enforces, for EVERY durable row:
 *
 *  - "deliver":  the row is deliverable NOW (lease-less pending, or an
 *                inflight lease that is expired/stealable). Exactly one of
 *                {an in-flight guarded delivery, an armed delivery wake} must
 *                cover it.
 *  - "deadline": an inflight row with a LIVE lease (foreign OR same-owner — a
 *                cloned sibling's live claim is indistinguishable here) this
 *                document is not itself POSTing: one wake at the lease
 *                deadline.
 *  - "held":     the Hub holds the row (host offline). Its bounded
 *                same-id held-retry timer is the ONLY allowed wake; a
 *                lease/deadline wake must never re-POST it early.
 *  - "quiet":    terminal (sent/done/rejected/unknown/reconciling), missing,
 *                or THIS document's own in-flight POST: nothing armed.
 *
 * The policy is intentionally owner-blind: a live lease is waited out whether
 * its owner is another tab or a cloned document sharing this persisted owner,
 * and an expired lease is stealable by whoever holds the delivery lock. The
 * Web Lock / durable lease mutex (plus active-delivery tracking) is what keeps
 * two deliverers off one row.
 */
export type LeaseWakeDecisionKind = "deliver" | "deadline" | "held" | "quiet";

export type LeaseWakeDecision = {
  commandId: Id;
  instanceId: Id;
  kind: LeaseWakeDecisionKind;
  /** The live lease, present exactly when kind === "deadline". */
  lease?: { owner: Id; until: number };
};

export interface LeaseWakePolicyCtx {
  /** Epoch ms the deadline/expiry judgement is made at. */
  now: number;
  /** Whether THIS document is actively POSTing the command right now. */
  isActiveDelivery: (commandId: Id) => boolean;
}

/** Decide the single recovery obligation for one authoritative durable row. */
export function leaseWakeDecision(rec: OutboxRecord, ctx: LeaseWakePolicyCtx): LeaseWakeDecision {
  const base = { commandId: rec.commandId, instanceId: rec.instanceId };
  // Held rows belong to their held-retry clock, never to a lease wake — even
  // though isDeliverable() includes them (a NORMAL user/online flush still
  // re-forwards a held row; the wake-triggered guarded flush does not).
  if (rec.state === "held") return { ...base, kind: "held" };
  if (rec.state === "pending") {
    return ctx.isActiveDelivery(rec.commandId)
      ? { ...base, kind: "quiet" }
      : { ...base, kind: "deliver" };
  }
  if (rec.state === "inflight") {
    // Our own live POST is the in-flight guarded delivery: no second wake.
    if (ctx.isActiveDelivery(rec.commandId)) return { ...base, kind: "quiet" };
    if (!rec.lease) return { ...base, kind: "deliver" };
    if (rec.lease.until <= ctx.now) return { ...base, kind: "deliver" };
    return { ...base, kind: "deadline", lease: { owner: rec.lease.owner, until: rec.lease.until } };
  }
  // sent / done / rejected / unknown / reconciling.
  return { ...base, kind: "quiet" };
}

/** Apply the policy to a whole authoritative durable snapshot. */
export function reconcileLeaseWakes(
  rows: readonly OutboxRecord[],
  ctx: LeaseWakePolicyCtx,
): LeaseWakeDecision[] {
  return rows.map((rec) => leaseWakeDecision(rec, ctx));
}
