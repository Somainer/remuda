import { useEffect, useState } from "react";
import { api } from "../../lib/api";
import type { MainEntry } from "../../lib/store";
import type { Instance } from "../../types/instance";
import type { Id, U64 } from "../../types/wire";
import { readMainTail, type MainTail } from "./mainStatus";

/** Delay before an unreadable Main tail is read again (doubles, capped). */
export const TAIL_RETRY_MS = 5_000;
const TAIL_RETRY_MAX_MS = 60_000;
/** Journal rows read for line 2 (same bounded window as the list phrases). */
const MAIN_TAIL = 64;
/** Earlier chapters read, newest first, to find a reply after a reload. */
const EARLIER_CHAPTERS = 3;

/**
 * Whether line 2 knows what the conversation's last reply is: `pending`
 * before the first answer, `error` while it cannot be read. Only `ok` may say
 * "no reply".
 */
export type TailRead = "pending" | "ok" | "error";

type CarriedReply = { text: string; generation: number; seq: string };

/**
 * The newest reply known per Main LINEAGE (r2 item 2). One conversation runs
 * across chapters, so a fresh chapter with no reply of its own shows the last
 * one; a different lineage never sees it. Lives for the page, so a remount
 * keeps it without reading again; after a reload it is derived again from
 * the earlier chapters' journals.
 */
const carriedReplies = new Map<string, CarriedReply>();

function newer(a: CarriedReply, b: CarriedReply): boolean {
  if (a.generation !== b.generation) return a.generation > b.generation;
  try {
    return BigInt(a.seq) > BigInt(b.seq);
  } catch {
    return Number(a.seq) > Number(b.seq);
  }
}

function noteReply(lineageId: string, reply: CarriedReply): boolean {
  const current = carriedReplies.get(lineageId);
  if (current && !newer(reply, current)) return false;
  carriedReplies.set(lineageId, reply);
  return true;
}

/** Test-only: forget every carried reply (a page reload). */
export function resetMainRepliesForTest(): void {
  carriedReplies.clear();
}

function readTail(row: Pick<Instance, "journalId" | "durableSeq">): Promise<MainTail> {
  const from = Math.max(0, Number(row.durableSeq ?? "0") - MAIN_TAIL);
  return api
    .eventsRead({ journalId: row.journalId as Id, afterSeq: String(from) as U64, limit: MAIN_TAIL })
    .then((page) => readMainTail(page.events));
}

function backoff(attempt: number): number {
  return Math.min(TAIL_RETRY_MS * 2 ** attempt, TAIL_RETRY_MAX_MS);
}

/**
 * Line 2's source: the bounded journal tail of Main's current chapter,
 * re-read when its durableSeq moves (the 2 s instance poll carries it), plus
 * the reply carried for the lineage. When the current chapter has no reply
 * and nothing is carried yet (a reload after a resume), up to three earlier
 * chapters of the same lineage are read, newest first. A failed read keeps
 * the last good tail, reports `error`, and retries on a bounded backoff.
 */
export function useMainTail(
  entry: MainEntry,
  rows: readonly Instance[],
): { tail: MainTail | null; carried: string | null; read: TailRead } {
  const instance = entry.instance;
  const lineageId = entry.lineageId;
  const id = instance.id;
  const journalId = instance.journalId ?? "";
  const seq = String(instance.durableSeq ?? "0");
  const key = `${id}@${seq}`;
  const [good, setGood] = useState<{ id: string; tail: MainTail } | null>(null);
  const [read, setRead] = useState<{ key: string; read: TailRead }>({ key: "", read: "pending" });
  const [retry, setRetry] = useState<{ key: string; n: number }>({ key: "", n: 0 });
  // Bumped when the carried cache changes, so the card re-renders.
  const [, setCarriedVersion] = useState(0);
  const [earlierRead, setEarlierRead] = useState<{ key: string; read: TailRead }>({ key: "", read: "pending" });
  const [earlierRetry, setEarlierRetry] = useState<{ key: string; n: number }>({ key: "", n: 0 });
  const attempt = retry.key === key ? retry.n : 0;

  useEffect(() => {
    if (!journalId) return;
    let cancelled = false;
    let timer: number | undefined;
    readTail({ journalId: journalId as Id, durableSeq: seq as U64 })
      .then((tail) => {
        if (cancelled) return;
        setGood({ id, tail });
        setRead({ key, read: "ok" });
        if (tail.reply && noteReply(lineageId, { text: tail.reply.text, generation: entry.generation, seq: tail.reply.seq })) {
          setCarriedVersion((v) => v + 1);
        }
      })
      .catch(() => {
        if (cancelled) return;
        setRead({ key, read: "error" });
        timer = window.setTimeout(() => setRetry({ key, n: attempt + 1 }), backoff(attempt));
      });
    return () => {
      cancelled = true;
      if (timer !== undefined) window.clearTimeout(timer);
    };
    // entry.generation is fixed for a chapter id, which `key` already names.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [lineageId, id, journalId, seq, key, attempt]);

  const tail = good && good.id === id ? good.tail : null;
  const carried = carriedReplies.get(lineageId)?.text ?? null;
  const currentRead: TailRead = read.key === key ? read.read : tail ? "ok" : "pending";

  // Earlier chapters of this lineage the instance list holds, newest first.
  const earlier = rows
    .filter((row) => row.id !== id && entry.chapterIds.includes(row.id) && row.journalId)
    .sort((a, b) => (b.generation ?? 1) - (a.generation ?? 1))
    .slice(0, EARLIER_CHAPTERS);
  const earlierKey = `${lineageId}|${id}|${earlier.map((row) => `${row.id}@${row.durableSeq}`).join(",")}`;
  const needEarlier = currentRead === "ok" && !tail?.reply && carried === null && earlier.length > 0;
  const earlierAttempt = earlierRetry.key === earlierKey ? earlierRetry.n : 0;

  useEffect(() => {
    if (!needEarlier) return;
    let cancelled = false;
    let timer: number | undefined;
    void (async () => {
      try {
        for (const row of earlier) {
          const found = await readTail(row);
          if (cancelled) return;
          if (found.reply) {
            noteReply(lineageId, { text: found.reply.text, generation: row.generation ?? 1, seq: found.reply.seq });
            setCarriedVersion((v) => v + 1);
            break;
          }
        }
        if (!cancelled) setEarlierRead({ key: earlierKey, read: "ok" });
      } catch {
        if (cancelled) return;
        setEarlierRead({ key: earlierKey, read: "error" });
        timer = window.setTimeout(() => setEarlierRetry({ key: earlierKey, n: earlierAttempt + 1 }), backoff(earlierAttempt));
      }
    })();
    return () => {
      cancelled = true;
      if (timer !== undefined) window.clearTimeout(timer);
    };
    // `earlierKey` names the rows read; `earlier` is rebuilt every render.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [needEarlier, earlierKey, earlierAttempt, lineageId]);

  // "No reply" is only known once the earlier chapters have been read too.
  const readState: TailRead =
    currentRead !== "ok" || !needEarlier ? currentRead : earlierRead.key === earlierKey ? earlierRead.read : "pending";
  return { tail, carried, read: readState };
}
