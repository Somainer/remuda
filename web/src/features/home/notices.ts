import { useCallback, useEffect, useRef, useState, useSyncExternalStore } from "react";
import { rest } from "../../lib/api";
import type { components } from "../../lib/api.generated";
import { hubStore } from "../../lib/store";
import type { Observation } from "../../types/observation";
import { lastAssistantNodeId, replyNodesByTurn } from "../session/remembered";

/**
 * D-060 §11.5 「告诉你」 (cb-owner-notify): what Main (or the owner's own
 * device) told the owner with `remuda owner notify`.
 *
 * - The home ④ queue shows the UNACKNOWLEDGED notices (home.md §4.1 kind 4,
 *   never counted in 「需要你」). One shared store serves `/m` and `/sessions`:
 *   it reads `GET /v1/owner/notices?unacked=1` once per settled list refresh
 *   (the 2 s poll the home already runs) while a queue is mounted, and emits
 *   only when the list changed, so a quiet poll re-renders nothing.
 * - Main's transcript shows each chapter's notices, acknowledged or not,
 *   under the reply of the turn that wrote them (the Hub records the turn).
 * - 「知道了」 is `POST /v1/owner/notices/{id}/ack`: owner-wide, every device,
 *   idempotent. No optimistic removal (home.md §4.4): the card leaves when
 *   the Hub has answered.
 */

export type OwnerNotice = components["schemas"]["OwnerNotice"];

function noticePath(query: string): string {
  return `/v1/owner/notices${query}`;
}

async function listNotices(query: string, signal?: AbortSignal): Promise<OwnerNotice[]> {
  const page = await rest<{ items?: OwnerNotice[] }>(noticePath(query), signal ? { signal } : {});
  return Array.isArray(page?.items) ? page.items : [];
}

/**
 * The most notices one list returns (the Hub's cap). The ④ read asks for all
 * of them: the default page of 50 would silently hide the oldest waiting
 * cards behind it.
 */
export const NOTICE_LIST_MAX = 200;

/** The notices still waiting for 知道了, newest first. */
export function listUnackedNotices(signal?: AbortSignal): Promise<OwnerNotice[]> {
  return listNotices(`?unacked=1&limit=${NOTICE_LIST_MAX}`, signal);
}

/** One chapter's notices, acknowledged or not, newest first. */
export function listChapterNotices(instanceId: string, signal?: AbortSignal): Promise<OwnerNotice[]> {
  return listNotices(`?instanceId=${encodeURIComponent(instanceId)}&limit=${NOTICE_LIST_MAX}`, signal);
}

/**
 * Call `listener` after every SUCCESSFUL list refresh of the hub store (the
 * 2 s poll and every other refresh); a failed one calls nothing, so a Hub
 * that is down is not asked twice per failure.
 */
export function onSuccessfulRefresh(listener: () => void): () => void {
  let last = hubStore.getRefreshHealth().lastOkAt;
  return hubStore.subscribeRefreshHealth(() => {
    const at = hubStore.getRefreshHealth().lastOkAt;
    if (at === null || at === last) return;
    last = at;
    listener();
  });
}

/** Acknowledge one notice on every device (idempotent). */
export async function ackNotice(noticeId: string): Promise<OwnerNotice> {
  const body = await rest<{ notice: OwnerNotice }>(
    `/v1/owner/notices/${encodeURIComponent(noticeId)}/ack`,
    { method: "POST" },
  );
  return body.notice;
}

/** Who a notice speaks for, in the card label: Main, the Hub, or the owner. */
export function noticeVoice(notice: Pick<OwnerNotice, "origin">): string {
  if (notice.origin === "hub") return "Hub";
  if (notice.origin === "human") return "备忘";
  return "Main";
}

function sameNotices(a: readonly OwnerNotice[], b: readonly OwnerNotice[]): boolean {
  if (a.length !== b.length) return false;
  return a.every(
    (notice, index) =>
      notice.noticeId === b[index].noticeId
      && notice.ackedAt === b[index].ackedAt
      && notice.title === b[index].title
      && notice.body === b[index].body,
  );
}

export type NoticeSnapshot = {
  /** Unacknowledged notices, newest first. */
  notices: readonly OwnerNotice[];
  /** A list has been read at least once since the first subscriber. */
  loaded: boolean;
};

/**
 * The unacknowledged notices, shared by every mounted queue. Reads ride the
 * hub store's settled refreshes (no timer of its own); one read in flight at a
 * time; the snapshot identity changes only when the list does.
 */
export class NoticeStore {
  private snapshot: NoticeSnapshot = { notices: [], loaded: false };
  private listeners = new Set<() => void>();
  private inflight: Promise<void> | null = null;
  private again = false;
  private stop: (() => void) | null = null;
  private lastOkAt: number | null = null;
  /**
   * Ids acknowledged from this page. A read that started before the ack
   * committed may still list the notice; it must not paint the card again
   * (ids are never reused, and an acknowledgement is never undone).
   */
  private acked = new Set<string>();

  private readonly read: () => Promise<OwnerNotice[]>;
  private readonly onRefresh: (listener: () => void) => () => void;
  private readonly refreshedAt: () => number | null;

  constructor(
    read: () => Promise<OwnerNotice[]> = () => listUnackedNotices(),
    onRefresh: (listener: () => void) => () => void = (listener) => hubStore.subscribeRefreshHealth(listener),
    refreshedAt: () => number | null = () => hubStore.getRefreshHealth().lastOkAt,
  ) {
    this.read = read;
    this.onRefresh = onRefresh;
    this.refreshedAt = refreshedAt;
  }

  getSnapshot = (): NoticeSnapshot => this.snapshot;

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    if (this.listeners.size === 1) this.start();
    return () => {
      this.listeners.delete(listener);
      if (this.listeners.size === 0) {
        this.stop?.();
        this.stop = null;
      }
    };
  };

  private start() {
    this.stop = this.onRefresh(() => {
      // Only a SUCCESSFUL list refresh reads again: a failing Hub is not
      // asked twice per failure.
      const at = this.refreshedAt();
      if (at === null || at === this.lastOkAt) return;
      this.lastOkAt = at;
      void this.refresh();
    });
    this.lastOkAt = this.refreshedAt();
    void this.refresh();
  }

  private emit(next: NoticeSnapshot) {
    this.snapshot = next;
    for (const listener of [...this.listeners]) listener();
  }

  /** Read the list now (joined with a read already in flight). */
  refresh(): Promise<void> {
    if (this.inflight) {
      this.again = true;
      return this.inflight;
    }
    const run = (async () => {
      try {
        do {
          this.again = false;
          try {
            const notices = (await this.read()).filter((row) => !this.acked.has(row.noticeId));
            if (!this.snapshot.loaded || !sameNotices(notices, this.snapshot.notices)) {
              this.emit({ notices, loaded: true });
            }
          } catch {
            // Quiet: the next settled refresh reads again; the queue keeps the
            // cards it has (the freshness rules grey them when the Hub is gone).
          }
        } while (this.again);
      } finally {
        this.inflight = null;
      }
    })();
    this.inflight = run;
    return run;
  }

  /**
   * 知道了: acknowledge on the Hub, then drop the card. A failure rejects and
   * leaves the card where it is.
   */
  async ack(noticeId: string, send: (id: string) => Promise<OwnerNotice> = ackNotice): Promise<OwnerNotice> {
    const notice = await send(noticeId);
    this.acked.add(noticeId);
    const notices = this.snapshot.notices.filter((row) => row.noticeId !== noticeId);
    if (notices.length !== this.snapshot.notices.length) this.emit({ ...this.snapshot, notices });
    return notice;
  }
}

export const noticeStore = new NoticeStore();

/** The unacknowledged notices for the home queue. */
export function useOwnerNotices(store: NoticeStore = noticeStore): NoticeSnapshot {
  return useSyncExternalStore(store.subscribe, store.getSnapshot, store.getSnapshot);
}

/** What `useChapterNotices` reads with; injectable for tests. */
export type ChapterNoticeSource = {
  read: (instanceId: string, signal: AbortSignal) => Promise<OwnerNotice[]>;
  /** Subscribe to the moments the list should be read again. */
  onRefresh: (listener: () => void) => () => void;
};

const CHAPTER_SOURCE: ChapterNoticeSource = { read: listChapterNotices, onRefresh: onSuccessfulRefresh };

/**
 * One chapter's notices for its transcript, only while `enabled` (a chapter of
 * Main's lineage: no other seat can notify). Read when the transcript grows
 * (`refreshKey` is its tail seq) AND on every successful list poll: a device
 * following Main with its page visible gets no push for a notice Main writes
 * (per-device suppression, main-agent.md §8.4), and writing a notice journals
 * nothing, so the poll is what brings that device the card within one 2 s
 * period.
 *
 * An acknowledgement from the card is folded in at once and survives a read
 * that was already in flight when it landed: that read still lists the notice
 * unacknowledged, and the page's own acknowledgement wins (an ack is never
 * undone). Only the newest read is applied.
 */
export function useChapterNotices(
  instanceId: string,
  enabled: boolean,
  refreshKey: string | undefined,
  source: ChapterNoticeSource = CHAPTER_SOURCE,
): { notices: OwnerNotice[]; acked: (notice: OwnerNotice) => void } {
  const [notices, setNotices] = useState<OwnerNotice[]>([]);
  const [polled, setPolled] = useState(0);
  const owner = useRef<string | null>(null);
  const ackedHere = useRef(new Map<string, OwnerNotice>());
  const latest = useRef(0);
  useEffect(() => {
    if (owner.current === instanceId) return;
    owner.current = instanceId;
    ackedHere.current = new Map();
    setNotices([]);
  }, [instanceId]);
  const { read, onRefresh } = source;
  useEffect(() => {
    if (!enabled) return;
    return onRefresh(() => setPolled((count) => count + 1));
  }, [enabled, onRefresh]);
  useEffect(() => {
    if (!instanceId || !enabled) return;
    const controller = new AbortController();
    const seq = ++latest.current;
    void read(instanceId, controller.signal).then(
      (rows) => {
        if (controller.signal.aborted || seq !== latest.current) return;
        const merged = rows.map((row) => (row.ackedAt ? row : (ackedHere.current.get(row.noticeId) ?? row)));
        setNotices((current) => (sameNotices(merged, current) ? current : merged));
      },
      () => undefined,
    );
    return () => controller.abort();
  }, [instanceId, enabled, refreshKey, polled, read]);
  // Stable, so the transcript rows' memo holds across renders.
  const acked = useCallback((notice: OwnerNotice) => {
    ackedHere.current.set(notice.noticeId, notice);
    setNotices((current) => current.map((row) => (row.noticeId === notice.noticeId ? notice : row)));
  }, []);
  return { notices, acked };
}

/**
 * Place each notice under the reply of the turn that wrote it (the Hub's
 * `turnSeq`), oldest first per reply. A notice whose turn has no reply in the
 * loaded window (written before the turn's first assistant message, the
 * turn is not loaded, or the carrier journals no turn lifecycle) hangs under
 * the newest assistant message, so it is never lost from the conversation;
 * with no assistant message at all it is not placed.
 */
export function noticesByReply(
  events: readonly Observation[],
  notices: readonly OwnerNotice[],
): Map<string, OwnerNotice[]> {
  const placed = new Map<string, OwnerNotice[]>();
  if (!notices.length) return placed;
  const replies = replyNodesByTurn(events);
  const newest = lastAssistantNodeId(events);
  const ordered = [...notices].sort((a, b) =>
    a.createdAt < b.createdAt ? -1 : a.createdAt > b.createdAt ? 1 : a.noticeId < b.noticeId ? -1 : 1,
  );
  for (const notice of ordered) {
    const nodeId =
      (notice.turnSeq !== null && notice.turnSeq !== undefined ? replies.get(String(notice.turnSeq)) : undefined)
      ?? newest;
    if (!nodeId) continue;
    placed.set(nodeId, [...(placed.get(nodeId) ?? []), notice]);
  }
  return placed;
}
