import type { Interaction } from "../../types/interaction";

/**
 * c-ghostbadge round 2: clock-driven deadline invalidation, shared by every
 * 待你处理 counter.
 *
 * `projectInteraction` evaluates the row against a clock: a row whose known
 * deadline elapses projects "expired" even though NO store slice changed. A
 * memo keyed only on store slices would then leave the persistent PhoneShell
 * badge at 1 while a freshly mounted inbox already showed 0. This clock lets
 * queue consumers recompute at the exact moment the soonest known deadline
 * passes — badge, inbox rows and the OS badge flip together, with no store
 * emission.
 *
 * The store owns the current clock instant (`clockNow`), updated only from an
 * effect (each interaction page) and from timer callbacks — never during a
 * React render — so `useSyncExternalStore(getInboxClockNow)` is a pure read.
 *
 * Single timer, armed from the latest interaction page on every store
 * emission (the badge hook effect and the OS-badge subscriber both call
 * {@link syncInboxDeadlineClock}); the timer chain re-arms itself for the
 * following deadline when it fires.
 */
const listeners = new Set<() => void>();
let timer: ReturnType<typeof setTimeout> | null = null;
let armed: readonly Interaction[] = [];
let clockNow: number = Date.now();

function emit(): void {
  for (const listener of [...listeners]) listener();
}

/** Subscribe to deadline-crossing ticks. Returns the unsubscribe handle. */
export function subscribeInboxClock(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** Current clock instant (ms epoch) — the useSyncExternalStore snapshot. */
export function getInboxClockNow(): number {
  return clockNow;
}

/**
 * Test-only: clear the singleton timer and reset the read clock to `at`
 * (default: real now). The clock is module-global and monotonic, so fake
 * timers in one test otherwise leak their instant into the next.
 */
export function __resetInboxClockForTest(at: number = Date.now()): void {
  if (timer !== null) {
    clearTimeout(timer);
    timer = null;
  }
  armed = [];
  listeners.clear();
  clockNow = at;
}

/**
 * (Re)arm the single invalidation timer from the current interactions.
 *
 * `now` is the instant the latest store page was observed; the projection has
 * already been evaluated against it by the caller's re-render, so here it is
 * used ONLY to compute the timer delay — `clockNow` is not reset. Resetting
 * it on every 2 s poll would freeze the read clock.
 */
export function syncInboxDeadlineClock(
  interactions: readonly Interaction[],
  now: number = Date.now(),
): void {
  armed = interactions;
  // Advance the read clock with every observed page (monotonic): the store
  // emission that triggers this sync already re-renders subscribers, and that
  // render must evaluate the projection against THIS page's instant — never a
  // frozen earlier value. Going backward is impossible (the wall clock and
  // fake test clocks are monotonic), so use max defensively.
  clockNow = Math.max(clockNow, now);
  if (timer !== null) {
    clearTimeout(timer);
    timer = null;
  }
  let next = Number.POSITIVE_INFINITY;
  for (const item of interactions) {
    if (item.state !== "pending" || item.deadline.state !== "known") continue;
    const at = Date.parse(item.deadline.value);
    if (Number.isFinite(at) && at >= now) next = Math.min(next, at);
    // A deadline already in the past needs no timer — the current projection
    // (or the next store poll) already excludes it.
  }
  if (!Number.isFinite(next)) return;
  // +2ms reads the wall clock strictly past the stored deadline, so
  // deadlinePassed (at < now) is guaranteed true.
  timer = setTimeout(() => {
    timer = null;
    clockNow = Date.now();
    emit();
    // Re-arm from the same page for the next deadline; the store did not
    // emit, so nobody else would.
    syncInboxDeadlineClock(armed, Date.now());
  }, next - now + 2);
}
