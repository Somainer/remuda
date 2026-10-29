import { useEffect, useState } from "react";
import { api } from "../../lib/api";
import type { Instance } from "../../types/instance";
import type { Id, U64 } from "../../types/wire";
import { readMainTail, type MainTail } from "./mainStatus";

/** Delay before an unreadable Main tail is read again (doubles, capped). */
export const TAIL_RETRY_MS = 5_000;
const TAIL_RETRY_MAX_MS = 60_000;
/** Journal rows read for line 2 (same bounded window as the list phrases). */
const MAIN_TAIL = 64;

/**
 * Whether line 2 knows what the chapter's tail holds: `pending` before the
 * first answer, `error` while it cannot be read. Only `ok` may say "no reply".
 */
export type TailRead = "pending" | "ok" | "error";

/**
 * The bounded journal tail behind the Main card's line 2, re-read when the
 * chapter's durableSeq moves (the 2 s instance poll carries it). A failed
 * read keeps the last good tail of the same chapter, reports `error`, and is
 * retried on a bounded backoff until it answers or the chapter moves on.
 */
export function useMainTail(instance: Instance | null): {
  tail: MainTail | null;
  carried: string | null;
  read: TailRead;
} {
  const id = instance?.id ?? "";
  const journalId = instance?.journalId ?? "";
  const seq = String(instance?.durableSeq ?? "0");
  const key = `${id}@${seq}`;
  const [good, setGood] = useState<{ id: string; tail: MainTail } | null>(null);
  const [read, setRead] = useState<{ key: string; read: TailRead }>({ key: "", read: "pending" });
  const [retry, setRetry] = useState<{ key: string; n: number }>({ key: "", n: 0 });
  // The newest reply seen on ANY chapter of this card: a fresh chapter has
  // no reply of its own yet, and the conversation is one across chapters.
  const [carried, setCarried] = useState<string | null>(null);
  const attempt = retry.key === key ? retry.n : 0;

  useEffect(() => {
    if (!id || !journalId) return;
    let cancelled = false;
    let timer: number | undefined;
    const from = Math.max(0, Number(seq) - MAIN_TAIL);
    api
      .eventsRead({ journalId: journalId as Id, afterSeq: String(from) as U64, limit: MAIN_TAIL })
      .then((page) => {
        if (cancelled) return;
        const tail = readMainTail(page.events);
        setGood({ id, tail });
        setRead({ key, read: "ok" });
        if (tail.reply) setCarried(tail.reply.text);
      })
      .catch(() => {
        if (cancelled) return;
        setRead({ key, read: "error" });
        const delay = Math.min(TAIL_RETRY_MS * 2 ** attempt, TAIL_RETRY_MAX_MS);
        timer = window.setTimeout(() => setRetry({ key, n: attempt + 1 }), delay);
      });
    return () => {
      cancelled = true;
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [id, journalId, seq, key, attempt]);

  const tail = good && good.id === id ? good.tail : null;
  // A chapter whose tail was read before keeps it while a newer read fails;
  // the read state only matters when there is nothing to show.
  const state: TailRead = read.key === key ? read.read : tail ? "ok" : "pending";
  return { tail, carried, read: state };
}
