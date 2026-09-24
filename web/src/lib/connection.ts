/**
 * D-055 / hub-resilience §5 client connection state machine — the single
 * source of truth for the phone↔Hub link.
 *
 * States (hub-resilience §5.2, client names):
 *  - live: follow socket open and a frame received within LIVE_FRAME_MS.
 *  - stale: socket open but silent past LIVE_FRAME_MS, or one REST probe
 *    timeout. No banner; the top dot only.
 *  - offline: socket closed, navigator offline, or a reconnect attempt
 *    failed. Banner; writes go to the outbox.
 *  - recovering: a reconnect is open and its seq catch-up / outbox flush is
 *    running. A watchdog guarantees it can never stay here.
 *
 * The machine never lies: a failed catch-up ends in offline, never a forced
 * "live" (the bug this replaces). Every state has an exit.
 */

export type ConnectionState = "live" | "stale" | "offline" | "recovering";

export type ConnectionEvent =
  | { type: "open" }
  | { type: "frame" }
  | { type: "close" }
  | { type: "error" }
  | { type: "online" }
  | { type: "offline" }
  | /** Foreground / pageshow / manual retry: reset backoff, try now. */
    { type: "resume" }
  | /** A REST probe finished; ok=frames reachable. */
    { type: "probe"; ok: boolean }
  | /** The resume action finished. */
    { type: "resumeAttempt"; ok: boolean };

export const LIVE_FRAME_MS = 15_000;
export const STALE_TO_OFFLINE_MS = 30_000;
export const RECOVERING_WATCHDOG_MS = 20_000;
export const BASE_BACKOFF_MS = 500;
export const MAX_BACKOFF_MS = 30_000;
export const REST_PROBE_MS = 15_000;

export type Scheduler = (fn: () => void, ms: number) => unknown;
export type ScheduleCancel = (handle: unknown) => void;

export type MachineDeps = {
  /**
   * The complete resume action: reopen the follow socket, catch up by seq,
   * flush the outbox. The machine calls this; it reports back with
   * `resumeAttempt {ok}`.
   */
  resume: () => Promise<void>;
  /** Lightweight REST probe used while the socket looks silently stale. */
  probe: () => Promise<boolean>;
  /**
   * Whether the follow WebSocket is currently OPEN. Foreground/online resume
   * from a cached "live" trusts the socket only when this is true; a silently
   * closed socket (no close event delivered during suspension) forces reopen.
   */
  isFollowOpen: () => boolean;
  schedule?: Scheduler;
  cancel?: ScheduleCancel;
  random?: () => number;
  log?: (msg: string) => void;
  onState?: (state: ConnectionState, detail?: { pendingCount?: number }) => void;
};

type TimerName = "stale" | "offline" | "reconnect" | "watchdog" | "probe";

export class ConnectionMachine {
  state: ConnectionState = "offline";
  /** Consecutive failed/ongoing attempts; reset on any real recovery. */
  private attempt = 0;
  private timers = new Map<TimerName, unknown>();
  private resumeInFlight = false;
  private readonly schedule: Scheduler;
  private readonly cancel: ScheduleCancel;
  private readonly random: () => number;
  private readonly deps: MachineDeps;

  constructor(deps: MachineDeps) {
    this.deps = deps;
    this.schedule =
      deps.schedule ??
      ((fn, ms) => {
        const h = setTimeout(fn, ms);
        return h as unknown;
      });
    this.cancel =
      deps.cancel ??
      ((h) => {
        clearTimeout(h as ReturnType<typeof setTimeout>);
      });
    this.random = deps.random ?? Math.random;
  }

  /** Full-jitter exponential backoff: U(0, min(max, base*2^n)). */
  backoffDelay(n: number): number {
    const cap = Math.min(MAX_BACKOFF_MS, BASE_BACKOFF_MS * 2 ** n);
    return Math.floor(this.random() * cap);
  }

  private setState(next: ConnectionState, detail?: { pendingCount?: number }) {
    if (next === this.state) return;
    this.state = next;
    this.deps.onState?.(next, detail);
  }

  private clearTimer(name: TimerName) {
    const h = this.timers.get(name);
    if (h !== undefined) this.cancel(h);
    this.timers.delete(name);
  }

  private clearTimers(...names: TimerName[]) {
    for (const n of names) this.clearTimer(n);
  }

  /** Start with an optimistic live assumption (bootstrap just succeeded). */
  startLive() {
    this.attempt = 0;
    this.armFrameWatchdog();
    this.setState("live");
  }

  /**
   * Begin offline (bootstrap failed to reach the Hub with a device session
   * still present): schedule the reconnect loop immediately and run the frame
   * watchdog off the machine's own retry/probe rhythm.
   */
  setStateOffline() {
    this.attempt = 0;
    this.clearTimers("stale", "offline", "probe", "reconnect", "watchdog");
    this.setState("offline");
    this.scheduleReconnect();
  }

  dispatch(event: ConnectionEvent) {
    switch (event.type) {
      case "frame":
        // A frame proves the link: live, backoff reset, watchdog re-armed.
        this.attempt = 0;
        if (this.state !== "live") this.setState("live");
        this.armFrameWatchdog();
        return;
      case "open":
        // The socket alone is not "live" until its snapshot/frame arrives; an
        // open while offline starts the recovering catch-up.
        if (this.state === "offline") this.beginResume();
        return;
      case "close":
      case "error":
        this.goOfflineAndSchedule();
        return;
      case "online":
      case "resume": {
        // Foreground / back-online / manual resume: zero the backoff. The
        // cached state may be stale — a socket can die silently while the
        // page is suspended before a close callback fires — so trust "live"
        // only if the follow socket is actually OPEN; otherwise reopen (an
        // in-flight recovery is coalesced by beginResume).
        this.attempt = 0;
        if ((this.state === "live" || this.state === "stale") && this.deps.isFollowOpen()) {
          this.armFrameWatchdog();
          return;
        }
        this.beginResume();
        return;
      }
      case "offline":
        this.goOffline();
        return;
      case "probe":
        if (this.state !== "stale") return;
        if (event.ok) {
          this.attempt = 0;
          this.setState("live");
          this.armFrameWatchdog();
        } else {
          this.goOfflineAndSchedule();
        }
        return;
      case "resumeAttempt": {
        if (!this.resumeInFlight) return;
        this.resumeInFlight = false;
        this.clearTimer("watchdog");
        if (event.ok) {
          this.attempt = 0;
          this.setState("live");
          this.armFrameWatchdog();
        } else {
          this.goOfflineAndSchedule();
        }
        return;
      }
    }
  }

  /**
   * Pause the reconnect clock while the page is hidden (no background churn);
   * resume immediately on return.
   */
  setVisibility(visible: boolean) {
    if (visible) this.dispatch({ type: "resume" });
    else if (this.state === "offline") this.clearTimer("reconnect");
  }

  private armFrameWatchdog() {
    this.clearTimers("stale", "offline", "probe", "reconnect");
    this.timers.set(
      "stale",
      this.schedule(() => {
        if (this.state !== "live") return;
        this.setState("stale");
        // One REST probe decides whether the socket is silently dead.
        this.timers.set(
          "probe",
          this.schedule(() => {
            if (this.state !== "stale") return;
            void this.deps.probe().then((ok) => this.dispatch({ type: "probe", ok }));
          }, REST_PROBE_MS),
        );
        // No frame for another window: offline regardless of the probe.
        this.timers.set(
          "offline",
          this.schedule(() => {
            if (this.state === "stale") this.goOfflineAndSchedule();
          }, STALE_TO_OFFLINE_MS),
        );
      }, LIVE_FRAME_MS),
    );
  }

  private goOffline() {
    this.clearTimers("stale", "offline", "probe", "watchdog");
    if (this.state !== "offline") this.setState("offline");
  }

  private goOfflineAndSchedule() {
    this.goOffline();
    this.scheduleReconnect();
  }

  private scheduleReconnect() {
    this.clearTimer("reconnect");
    const n = this.attempt;
    const delay = this.backoffDelay(n);
    this.deps.log?.(`reconnect attempt ${n + 1} in ${delay} ms`);
    this.timers.set(
      "reconnect",
      this.schedule(() => {
        this.timers.delete("reconnect");
        this.beginResume();
      }, delay),
    );
  }

  private beginResume() {
    if (this.resumeInFlight) return;
    this.clearTimers("stale", "offline", "probe", "reconnect");
    this.setState("recovering");
    this.resumeInFlight = true;
    // The resume can never strand us in recovering: the watchdog forces the
    // attempt closed, and the promise also reports success/failure.
    this.timers.set(
      "watchdog",
      this.schedule(() => {
        if (!this.resumeInFlight) return;
        this.resumeInFlight = false;
        this.attempt += 1;
        this.goOfflineAndSchedule();
      }, RECOVERING_WATCHDOG_MS),
    );
    this.attempt += 1;
    void this.deps
      .resume()
      .then(() => this.dispatch({ type: "resumeAttempt", ok: true }))
      .catch(() => this.dispatch({ type: "resumeAttempt", ok: false }));
  }

  dispose() {
    for (const h of this.timers.values()) this.cancel(h);
    this.timers.clear();
  }
}
