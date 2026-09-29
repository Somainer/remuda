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
 *
 * c-ghostbadge round 3: EVERY clock advance notifies subscribers. A resync
 * cancels the pending timer, and its observed instant can already have crossed
 * the deadline that timer was about to tick for (a poll landing inside the
 * +2 ms slack, or a delayed event loop) — without an emit that flip is lost
 * forever, leaving a stuck badge. The timer is then always re-armed against the
 * NEW clock for the next deadline at/after it.
 */
const listeners = new Set<() => void>();
let timer: ReturnType<typeof setTimeout> | null = null;
let armed: readonly Interaction[] = [];
let clockNow: number = Date.now();

/**
 * Independent DISPLAY tick for relative time labels (c-perffu r3). Equal
 * quiet polls no longer emit store notifications and the deadline timer
 * arms only when a known deadline exists, so without this tick 「刚刚/Nm」
 * froze on an idle inbox. 15 s is finer than the 45 s 刚刚→1m boundary and
 * the subsequent 60 s minute buckets, so every label boundary is observed
 * (a 30 s cadence could skip 1m). It lives only while a clock subscriber
 * (badge/inbox rows) is mounted; deadline invalidation below is unchanged.
 */
const DISPLAY_TICK_MS = 15_000;
let displayTimer: ReturnType<typeof setInterval> | null = null;

function startDisplayClock(): void {
  if (displayTimer !== null) return;
  displayTimer = setInterval(() => {
    clockNow = Date.now();
    emit();
  }, DISPLAY_TICK_MS);
}

function stopDisplayClockIfIdle(): void {
  if (listeners.size === 0 && displayTimer !== null) {
    clearInterval(displayTimer);
    displayTimer = null;
  }
}

function emit(): void {
  for (const listener of [...listeners]) listener();
}

/**
 * Subscribe to deadline-crossing ticks AND the independent display tick.
 * Returns the unsubscribe handle.
 */
export function subscribeInboxClock(listener: () => void): () => void {
  listeners.add(listener);
  startDisplayClock();
  return () => {
    listeners.delete(listener);
    stopDisplayClockIfIdle();
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
  if (displayTimer !== null) {
    clearInterval(displayTimer);
    displayTimer = null;
  }
  armed = [];
  listeners.clear();
  clockNow = at;
}

/**
 * (Re)arm the single invalidation timer from the current interactions.
 *
 * `now` is the instant the latest store page was observed. It advances the
 * read clock (monotonic max — never reset outright, or a 2 s poll landing
 * on the same instant would freeze the clock) and, on every actual advance,
 * notifies subscribers; the caller's store emission does not cover the
 * deadline that THIS page already crossed. The timer is then always re-armed
 * against the new clock for the next deadline at/after it.
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
  const previous = clockNow;
  clockNow = Math.max(clockNow, now);
  // Every advance MUST emit: this call cancels the pending timer below, and
  // `now` may already be past the deadline that timer was about to tick for
  // (a poll landing inside the timer slack, or a delayed event loop). The clock
  // emit is then the ONLY notification subscribers get that the projection
  // flipped — staying silent leaves the persistent badge stuck.
  if (clockNow !== previous) emit();
  // Always re-arm against the NEW clock: the previous timer (even if it was
  // aimed at the same deadline) is cancelled and must be replaced.
  if (timer !== null) {
    clearTimeout(timer);
    timer = null;
  }
  let next = Number.POSITIVE_INFINITY;
  for (const item of interactions) {
    if (item.state !== "pending" || item.deadline.state !== "known") continue;
    const at = Date.parse(item.deadline.value);
    if (Number.isFinite(at) && at >= clockNow) next = Math.min(next, at);
    // A deadline before clockNow is already expired: the advance above just
    // notified subscribers of its flip. Arming for it would fire late or
    // steal the slot from the following deadline.
  }
  if (!Number.isFinite(next)) return;
  // +2ms reads the wall clock strictly past the stored deadline, so
  // deadlinePassed (at < now) is guaranteed true.
  timer = setTimeout(() => {
    timer = null;
    const at = Date.now();
    clockNow = at;
    emit();
    // Re-arm from the same page for the next deadline; the store did not
    // emit, so nobody else would. Pass the fired instant: clockNow already
    // equals it, so this re-sync stays quiet and only re-arms.
    syncInboxDeadlineClock(armed, at);
  }, next - clockNow + 2);
}
