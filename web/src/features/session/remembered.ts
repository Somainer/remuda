/**
 * 「已记住：<title>」 rows under a turn's reply (D-060 §12.4, cb-memory-ui).
 *
 * The Hub records the journal seq of the writer's latest `turn_started` for
 * every memory write. This module joins those rows back onto the transcript:
 * each turn's reply is the LAST assistant message of that turn, and the rows
 * of the write's turn hang under it. It also owns the fetch, which rides the
 * transcript's existing refresh (one request per journal batch, never a timer
 * per row).
 */
import { useEffect, useRef, useState } from "react";
import type { Observation } from "../../types/observation";
import { nativeLifecycle } from "./live/payloadGuard";
import { listMemoryWrites, type MemoryWriteItem } from "../memory/api";

function seqOf(ev: Observation): bigint | null {
  try {
    return BigInt(ev.seq as string);
  } catch {
    return null;
  }
}

function seqCompare(a: Observation, b: Observation): number {
  const left = seqOf(a);
  const right = seqOf(b);
  if (left === null || right === null) return 0;
  return left < right ? -1 : left > right ? 1 : 0;
}

/**
 * The assembler's stable id for each message event: the `nodeId` of the
 * group's earliest revision, where a group is every message event linked by
 * `messageId` OR `nodeId` (protocol §5.2 — a native rename moves the
 * messageId, and a close can carry a fresh node id).
 */
function stableNodeIds(events: readonly Observation[]): Map<Observation, string> {
  const parent = new Map<string, string>();
  const find = (key: string): string => {
    let root = key;
    while (parent.get(root) !== undefined && parent.get(root) !== root) root = parent.get(root)!;
    parent.set(key, root);
    return root;
  };
  const union = (a: string, b: string) => {
    const ra = find(a);
    const rb = find(b);
    if (ra !== rb) parent.set(ra, rb);
  };
  const keysOf = new Map<Observation, string[]>();
  for (const ev of events) {
    if (ev.kind !== "message") continue;
    const payload = ev.payload as { messageId?: unknown; nodeId?: unknown };
    const keys: string[] = [];
    if (typeof payload.messageId === "string" && payload.messageId) keys.push(`m:${payload.messageId}`);
    if (typeof payload.nodeId === "string" && payload.nodeId) keys.push(`n:${payload.nodeId}`);
    if (!keys.length) continue;
    keysOf.set(ev, keys);
    for (const key of keys) if (!parent.has(key)) parent.set(key, key);
    for (const key of keys.slice(1)) union(keys[0], key);
  }

  // Root → (nodeId of the group's earliest-seq event).
  const earliest = new Map<string, { seq: bigint; nodeId: string }>();
  for (const [ev, keys] of keysOf) {
    const seq = seqOf(ev);
    const nodeId = (ev.payload as { nodeId?: unknown }).nodeId;
    if (seq === null || typeof nodeId !== "string" || !nodeId) continue;
    const root = find(keys[0]);
    const current = earliest.get(root);
    if (!current || seq < current.seq) earliest.set(root, { seq, nodeId });
  }

  const out = new Map<Observation, string>();
  for (const [ev, keys] of keysOf) {
    const found = earliest.get(find(keys[0]));
    if (found) out.set(ev, found.nodeId);
  }
  return out;
}

/**
 * Turn `turn_started` seq → the transcript node id of that turn's reply: the
 * last assistant message before the turn's `result` (or the next start). A
 * turn with no assistant message gets no placement.
 */
export function replyNodesByTurn(events: readonly Observation[]): Map<string, string> {
  const stable = stableNodeIds(events);
  const ordered = [...events].sort(seqCompare);
  const replies = new Map<string, { seq: bigint; nodeId: string }>();
  let turn: bigint | null = null;
  for (const ev of ordered) {
    const seq = seqOf(ev);
    if (seq === null) continue;
    const lifecycle = nativeLifecycle(ev);
    if (lifecycle && lifecycle.topic === "turn") {
      if (lifecycle.nativeName === "turn_started") turn = seq;
      else if (lifecycle.nativeName === "result") turn = null;
      continue;
    }
    if (ev.kind !== "message" || turn === null) continue;
    const role = (ev.payload as { role?: unknown }).role;
    if (role !== "assistant") continue;
    const nodeId = stable.get(ev);
    if (!nodeId) continue;
    const key = turn.toString();
    const previous = replies.get(key);
    if (!previous || seq > previous.seq) replies.set(key, { seq, nodeId });
  }
  const out = new Map<string, string>();
  for (const [key, reply] of replies) out.set(key, reply.nodeId);
  return out;
}

/**
 * One row per slug per turn (the newest revision wins, so a delete replaces
 * the write in the same turn), attached to the turn's reply node.
 */
export function rememberedByReply(
  events: readonly Observation[],
  writes: readonly MemoryWriteItem[],
): Map<string, MemoryWriteItem[]> {
  const replies = replyNodesByTurn(events);
  const byTurn = new Map<string, Map<string, MemoryWriteItem>>();
  for (const row of writes) {
    if (row.turnSeq === null || row.turnSeq === undefined) continue;
    const key = String(row.turnSeq);
    const rows = byTurn.get(key) ?? new Map<string, MemoryWriteItem>();
    const previous = rows.get(row.slug);
    const newer =
      !previous ||
      row.revision > previous.revision ||
      (row.revision === previous.revision && row.writtenAt >= previous.writtenAt);
    if (newer) rows.set(row.slug, row);
    byTurn.set(key, rows);
  }
  const placed = new Map<string, MemoryWriteItem[]>();
  for (const [turn, rows] of byTurn) {
    const nodeId = replies.get(turn);
    if (!nodeId) continue;
    // The same reply can carry more than one turn only if the journal cut a
    // turn_started; merge instead of dropping the later rows.
    const merged = [...(placed.get(nodeId) ?? []), ...rows.values()];
    merged.sort((a, b) =>
      a.writtenAt < b.writtenAt ? -1 : a.writtenAt > b.writtenAt ? 1 : a.slug < b.slug ? -1 : 1,
    );
    placed.set(nodeId, merged);
  }
  return placed;
}

/**
 * The chapter's write log, refreshed with the transcript: `refreshKey` is the
 * transcript's tail seq, so one journal batch yields at most one request.
 *
 * Pages are keyed by log row id. The first request has no cursor and the Hub
 * answers with the NEWEST page (the turns a transcript is showing); every
 * later request continues from `nextAfterId`, so a single turn with more rows
 * than a page still advances and rows with a NULL turn seq are never stranded
 * behind a turn cursor.
 */
export function useRememberedWrites(instanceId: string, refreshKey: string | undefined): MemoryWriteItem[] {
  const [writes, setWrites] = useState<MemoryWriteItem[]>([]);
  const cursor = useRef<number | null>(null);
  const connection = useRef<string | null>(null);

  useEffect(() => {
    if (connection.current === instanceId) return;
    connection.current = instanceId;
    cursor.current = null;
    setWrites([]);
  }, [instanceId]);

  useEffect(() => {
    if (!instanceId) return;
    const controller = new AbortController();
    void (async () => {
      try {
        const page = await listMemoryWrites(instanceId, cursor.current ?? undefined, controller.signal);
        if (controller.signal.aborted) return;
        setWrites((current) => {
          // One row per slug per turn, newest revision: a rewrite or delete
          // later in the same turn replaces the earlier row.
          const merged = new Map<string, MemoryWriteItem>();
          const key = (row: MemoryWriteItem) => `${row.turnSeq ?? "none"}:${row.slug}`;
          for (const row of [...current, ...page.items]) {
            const previous = merged.get(key(row));
            if (
              !previous ||
              row.revision > previous.revision ||
              (row.revision === previous.revision && row.writtenAt >= previous.writtenAt)
            ) {
              merged.set(key(row), row);
            }
          }
          return [...merged.values()];
        });
        cursor.current = Math.max(cursor.current ?? 0, page.nextAfterId);
      } catch {
        // Quiet: the transcript's next refresh retries; a missing row is a
        // display gap, never an error banner over the conversation.
      }
    })();
    return () => controller.abort();
  }, [instanceId, refreshKey]);

  return writes;
}
