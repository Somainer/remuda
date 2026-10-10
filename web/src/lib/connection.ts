/**
 * D-055 / hub-resilience §5 client connection state machine — the single
 * source of truth for the phone↔Hub link.
 *
 * States (hub-resilience §5.2, client names):
 *  - live: follow socket open and a frame received within LIVE_FRAME_MS.
 *  - stale: socket open but silent past LIVE_FRAME_MS, one REST probe
 *    timeout, OR a resume whose follow could not open while REST stayed
 *    reachable. No banner; the top dot only; REST drives delivery.
 *  - offline: socket closed, navigator offline, or a reconnect attempt
 *    failed with REST also unreachable. Banner; writes go to the outbox.
 *  - recovering: a reconnect is open and its seq catch-up / outbox flush is
 *    running. A watchdog guarantees it can never stay here.
 *
 * The machine never lies: a failed catch-up ends in stale/offline, never a
 * forced "live" (the bug this replaces). Every state has an exit.
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
  | /** The resume action finished. Only accepted for the current attempt. */
    { type: "resumeAttempt"; ok: boolean; attemptId: number };

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
   * Called exactly once when an in-flight resume attempt is closed while still
   * bound to the CURRENT generation: `why: "ok"` on success, `"failed"` when the
   * resume action rejected, `"watchdog"` when the 20 s watchdog gave up on a
   * hung resume. The store uses it to settle post-delivery debt the attempt
   * owed: a failed/hung follow must not leave an accepted row at 已受理 while
   * REST works. Never called for a superseded (stale generation) attempt — the
   * bind-away path owns that instance's debt.
   */
  onAttemptFinish?: (info: { gen: number; attemptId: number; why: "ok" | "failed" | "watchdog" }) => void;
  /**
   * Whether the follow link is genuinely usable RIGHT NOW: the socket is OPEN
   * AND a frame was received within LIVE_FRAME_MS. Foreground/online resume
   * from a cached "live" trusts the link only when this is true; an OPEN but
   * silent, or a closed, socket forces reopen.
   */
  isFollowLive: () => boolean;
  schedule?: Scheduler;
  cancel?: ScheduleCancel;
  /** Injectable clock (tests with fake timers pass their clock's now()). */
  now?: () => number;
  random?: () => number;
  log?: (msg: string) => void;
  onState?: (state: ConnectionState, detail?: { pendingCount?: number }) => void;
};

type TimerName = "stale" | "offline" | "reconnect" | "watchdog" | "probe" | "bind";

export class ConnectionMachine {
  state: ConnectionState = "offline";
  /** Consecutive failed/ongoing attempts; reset on any real recovery. */
  private attempt = 0;
  private timers = new Map<TimerName, unknown>();
  private resumeInFlight = false;
  /**
   * Set by the browser `offline` arm while a resume is in flight: the attempt
   * bookkeeping is retired so an `online` resume is not coalesced away, yet
   * the attempt's own rejection still owns the failure settlement (gate 8).
   */
  private resumeRetiredOffline = false;
  /**
   * Set when the follow socket closes/errors WHILE a resume attempt owns it.
   * The close itself does not force a state change (the attempt settles), but
   * a subsequent frame must not certify live over a DEAD socket, and the
   * attempt's failure arm treats the link as a real loss. Cleared on the next
   * socket open (gate 8: snapshot frame → live, then socket closed, then REST
   * catch-up failed used to keep false 已连接 for up to 40 s).
   */
  private closedDuringResume = false;
  /**
   * Monotonic id of the current resume attempt. A completion (or watchdog)
   * from a timed-out attempt carries a stale id and is ignored: the watchdog
   * fires attempt A, B starts, A resolves late — A must never consume B.
   */
  private resumeAttemptId = 0;
  /**
   * Binding generation of the CURRENT session binding and of the attempt in
   * flight. The store bumps {@link latestBindGen} whenever it binds the
   * machine to a DIFFERENT journal (navigation A → B): an attempt armed for
   * an older generation is superseded — its late completion must never
   * certify/fail the newly bound session.
   */
  private latestBindGen = 0;
  private resumeBindGen = 0;
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
    // Gate 9 item 1: the stale-entry deadline must not survive stale itself.
    // Clearing it ONLY on live leaked it across offline/recovering: a
    // REST-proven link that cycled stale→offline→recovering→stale computed
    // offlineMs=0, went immediately offline and storm-reconnected. It is a
    // bound on ONE quiet-stale episode, seeded on entry, discarded the moment
    // the machine leaves stale.
    if (next !== "stale") this.staleDeadlineAt = 0;
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
   * Bootstrap over REST SUCCEEDED (hello + list), but no follow socket exists
   * yet (the caller is on the session list / new-session page), so there is no
   * frame stream to watchdog: report live without arming the frame timer. The
   * live claim for an ACTIVE session is enforced separately — followBound()
   * starts a frame deadline the moment a session's follow socket opens, so a
   * frozen transcript can never sit behind 已连接.
   */
  bootstrapLive() {
    this.attempt = 0;
    this.clearTimers("stale", "offline", "probe", "reconnect", "watchdog", "bind");
    this.setState("live");
  }

  /**
   * A follow socket is being opened for the active session. Require its first
   * frame (the subscribe snapshot or an event) within LIVE_FRAME_MS: a frame
   * dispatches {frame} and takes over via armFrameWatchdog; silence means the
   * open socket is not actually carrying data — live degrades to stale (probe
   * then reopen), recovering retries the resume.
   *
   * `rebind` is set when the store rebounds to an ALREADY-MOUNTED session
   * (navigation back, or a racing mount the user returns to). Such a call must
   * judge the EXISTING socket by the frame/probe deadline alone: the machine
   * may currently sit in a PREVIOUS mount's recovering state, and inheriting
   * it via `wasLive === false` would immediately beginResume() and open a
   * second socket for a session that still has a working one. A genuinely dead
   * socket is still reopened later — the stale-state probe path certifies REST
   * reachability and only then resumes.
   */
  followBound(opts: { rebind?: boolean } = {}) {
    this.clearTimer("bind");
    if (!opts.rebind && this.state === "offline") {
      this.beginResume();
      return;
    }
    // gate 8 items 3 & 5, gate 9 item 2:
    //  - a rebind from LOUD OFFLINE has no REST proof: do nothing (never
    //    enter stale and hide the banner; the offline reconnect path owns
    //    recovery).
    //  - a rebind while quiet stale, with NO in-flight attempt, must RE-ARM
    //    the stale timers: a navigation into a mounted session can happen
    //    after noteBinding retired the previous quiet attempt and cleared the
    //    watchdog (bind was the only timer and the rebind early-return used to
    //    strand stale with zero timers forever).
    //  - a rebind while a resume/quiet reopen is still in flight owns nothing
    //    new: the attempt settles (frame → live, reject → stale re-arms).
    if (opts.rebind && (this.state === "offline" || this.resumeInFlight)) {
      if (this.deps.isFollowLive()) this.armFrameWatchdog();
      return;
    }
    if (opts.rebind && this.state === "stale") {
      this.armStaleProbeTimers();
      return;
    }
    // gate 9 item 3: the REAL reopen path (openFollowSocket → followBound()
    // with NO rebind) can arrive while the machine is quiet stale — the
    // resume's own socket is opening. Skip arming a competing `bind` timer
    // (it would beginResume 15 s later and flash the recovering banner on
    // every blocked-upgrade cycle); the resume watchdog already owns this
    // attempt.
    if (!opts.rebind && this.resumeInFlight) {
      if (this.deps.isFollowLive()) this.armFrameWatchdog();
      return;
    }
    // gate 9 item 3: from quiet stale a bind deadline certifies via the SAME
    // quiet probe path (treat stale as wasLive), never beginResume — the
    // upgrade refusal / snapshot timeout must not produce a loud recovering
    // cycle.
    const wasLive = this.state === "live" || this.state === "stale";
    this.timers.set(
      "bind",
      this.schedule(() => {
        this.timers.delete("bind");
        // A frame landed just in time and already proved the link.
        if (this.state === "live" && this.deps.isFollowLive()) {
          this.armFrameWatchdog();
          return;
        }
        if (wasLive) {
          // OPEN but silent: stale, then the same probe/offline dance the
          // frame watchdog runs (REST reachable triggers reopen+catch-up).
          this.setState("stale");
          this.armStaleProbeTimers();
        } else {
          this.beginResume();
        }
      }, LIVE_FRAME_MS),
    );
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
        // (A frame from a socket that closes later in the same resume is not
        // enough to clear closedDuringResume — only a fresh socket open does.)
        this.attempt = 0;
        if (this.state !== "live") this.setState("live");
        this.armFrameWatchdog();
        return;
      case "open":
        // A fresh socket clears any latched close from its predecessor.
        this.closedDuringResume = false;
        // The socket alone is not "live" until its snapshot/frame arrives; an
        // open while offline starts the recovering catch-up.
        if (this.state === "offline") this.beginResume();
        return;
      case "close":
      case "error":
        // A resume attempt in flight OWNS the decision for this failure:
        // `openFollowSocket`'s socket fails to open and rejects the resume
        // action, whose completion settles via the REST probe. Letting the
        // socket's own close/error force offline here raced that decision
        // (offline in tens of ms, before the probe could answer) and stormed
        // offline↔recovering on every blocked follow upgrade under load
        // (c-reconnfu gate 7). Latch the close so the attempt's failure arm
        // cannot certify live over a dead socket, and so a late frame cannot
        // either (gate 8). The resume watchdog (20 s) guarantees an exit.
        if (this.resumeInFlight) {
          this.closedDuringResume = true;
          return;
        }
        this.goOfflineAndSchedule();
        return;
      case "online":
      case "resume": {
        // Foreground / back-online / manual resume: the cached state can be
        // stale — a socket can die silently while the page is suspended before
        // any close callback fires. Trust "live" ONLY when the socket is open
        // AND recently framed; otherwise reopen (coalesced by beginResume).
        this.attempt = 0;
        if ((this.state === "live" || this.state === "stale") && this.deps.isFollowLive()) {
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
          // REST reachable is NOT proof the follow stream works: a live
          // transcript needs the socket. Reopen + catch up — but QUIETLY while
          // already in stale (REST proven): stay banner-less, success certifies
          // live on the first frame, a blocked upgrade stays stale (c-reconnfu
          // gate 7: a loud recovering blip on every periodic reopen kept the
          // banner mounted under load).
          this.attempt = 0;
          this.quietReopen();
        } else {
          this.goOfflineAndSchedule();
        }
        return;
      case "resumeAttempt": {
        // Accept a completion only for the CURRENT attempt. A late resolution
        // from an attempt the watchdog already timed out (B is now running)
        // must not certify live nor consume B's outcome. The browser
        // `offline` arm (gate 8) retires resumeInFlight but marks the attempt
        // current for its rejection via resumeRetiredOffline.
        if (
          (!this.resumeInFlight && !this.resumeRetiredOffline) ||
          event.attemptId !== this.resumeAttemptId
        ) {
          return;
        }
        const gen = this.resumeBindGen;
        const closed = this.closedDuringResume;
        this.resumeInFlight = false;
        this.resumeRetiredOffline = false;
        // Gate 9 item 4: the latch is consumed by THIS completion — a later
        // healthy framed follow whose read times out must stay live again.
        this.closedDuringResume = false;
        this.clearTimer("watchdog");        if (event.ok) {
          this.attempt = 0;
          this.setState("live");
          this.armFrameWatchdog();
        } else if (!closed && this.deps.isFollowLive()) {
          // The resume ACTION rejected (a resume-side REST catch-up read
          // timed out under load), but the follow socket it opened is still
          // OPEN and proven live by its own frames. Frames are healing the
          // journal and the socket's frame watchdog/close remain the real
          // failure detectors — forcing offline here would tear down the
          // working follow, reopen a replacement, and storm
          // offline↔recovering (c-reconnfu gate 7). Gate 8: the latch also
          // covers the race where the socket closed after its last frame —
          // a dead follow must never certify live over a failed REST read.
          this.attempt = 0;
          this.setState("live");
          this.armFrameWatchdog();
        } else if (this.state === "stale" && !closed) {
          // This was a QUIET reopen out of a REST-proven stale state (see
          // quietReopen): REST already answered, so the failed follow re-open
          // stays quiet stale and the stale probe/offline timers keep
          // ticking — no loud recovering/offline banner blip. Clear the bind
          // deadline the real openFollowSocket path armed (gate 9 item 3):
          // leaving it would beginResume 15 s later and flash recovering even
          // though the attempt already settled to stale.
          this.clearTimer("bind");
          this.attempt = 0;
          this.armStaleProbeTimers();
        } else {
          // Recovering, OR a frame-certified live whose socket has since
          // latched closed (gate 8 item 2): leave the loud recovery state to
          // the REST probe, which settles stale/offline. Drop the latched
          // "live" label first so the settle probe is allowed to act.
          if (closed && this.state === "live") this.setState("recovering");
          this.settleResumeFailureViaProbe(closed);
        }
        // Bind match is defence in depth: noteBinding retires a superseded
        // attempt before its completion can arrive, so only a current-binding
        // finish is reported.
        if (gen === this.latestBindGen) {
          this.deps.onAttemptFinish?.({
            gen,
            attemptId: event.attemptId,
            why: event.ok ? "ok" : "failed",
          });
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
    this.clearTimers("bind", "stale", "offline", "probe", "reconnect");
    this.timers.set(
      "stale",
      this.schedule(() => {
        if (this.state !== "live") return;
        this.setState("stale");
        // One REST probe decides whether the socket is silently dead.
        this.armStaleProbeTimers();
      }, LIVE_FRAME_MS),
    );
  }

  /**
   * Wall-clock deadline after which quiet stale MUST become loud offline.
   * Gate 8 item 8: quiet reopens re-arm the probe/offline timers from zero on
   * every cycle while follow upgrades stay blocked; without a single deadline
   * carried across cycles a Hub that dies right after a good probe kept the
   * banner-less state indefinitely (or 40–70 s), contradicting
   * hub-resilience §5.2. 0 = no deadline in force (a fresh live→stale frame
   * expiry sets one).
   */
  private staleDeadlineAt = 0;

  /**
   * The quiet-state retry timers: probe REST, reopening the follow when it
   * answers, and giving up to loud offline after a window without a probe.
   * The offline deadline is the EARLIER of the fixed STALE_TO_OFFLINE window
   * and any deadline carried from an earlier stale cycle (quiet reopens), so
   * total quiet-stale time is bounded even with repeated blocked upgrades.
   */
  /**
   * The quiet-state retry timers: probe REST, reopening the follow when it
   * answers, and giving up to loud offline at the end of ONE stale episode.
   *
   * Gate 9: the 30 s deadline is carried across re-arms DURING the same stale
   * episode (probe cycles and quiet reopens call this without leaving stale)
   * but re-seeded whenever the machine has actually left stale and returned —
   * setState cleared it then, so a missing value here means a fresh entry.
   */
  private armStaleProbeTimers() {
    // Gate 9 item 1: seed only while IN stale. The quiet-reopen watchdog
    // (state recovering) must not manufacture a deadline that then fires at
    // 0 when the failed reopen lands back in stale.
    if (this.state !== "stale") return;
    const now = this.clockNow();
    if (!this.staleDeadlineAt) {
      this.staleDeadlineAt = now + STALE_TO_OFFLINE_MS;
    }
    const offlineMs = Math.max(0, this.staleDeadlineAt - now);
    this.timers.set(
      "probe",
      this.schedule(() => {
        if (this.state !== "stale") return;
        void this.deps.probe().then((ok) => this.dispatch({ type: "probe", ok }));
      }, REST_PROBE_MS),
    );
    this.timers.set(
      "offline",
      this.schedule(() => {
        if (this.state === "stale") this.goOfflineAndSchedule();
      }, offlineMs),
    );
  }

  private clockNow(): number {
    return this.deps.now ? this.deps.now() : Date.now();
  }

  /**
   * Decide the aftermath of a resume that could not open the follow socket.
   * Restores stay in `recovering` (banner) only for the probe's own duration;
   * a frame-live link is never touched; otherwise REST reachable settles quiet
   * at stale (whether we are currently recovering OR offline — an `offline`
   * browser event can land while the resume is in flight, and dropping that
   * case left the link loud-offline with nothing scheduled once the resume
   * rejected, c-reconnfu gate 8) and unreachable stays loud offline with the
   * reconnect clock armed.
   */
  private settleResumeFailureViaProbe(closed: boolean = false) {
    void this.deps
      .probe()
      .then((ok) => {
        // A newer attempt owns the state.
        if (this.resumeInFlight) return;
        // A genuinely frame-live follow owns itself — but NOT when the live
        // label has no frame backing (isFollowLive false): a resume failure on
        // an unframed socket that nevertheless sits labelled live (an earlier
        // snapshot that was revoked) must settle via the probe rather than
        // hold a false 已连接 (gate 8 item 6 / gate 9 item 4).
        if (!closed && this.deps.isFollowLive()) return;
        // An unframed live (or a latched-closed live) leaves the label for the
        // probe decision; framed-open without a latch stays live above.
        if (this.state === "live") this.setState("recovering");
        if (ok) {
          this.attempt = 0;
          this.setState("stale");
          this.armStaleProbeTimers();
        } else {
          this.goOfflineAndSchedule();
        }
      })
      .catch(() => {
        if (!this.resumeInFlight && !this.deps.isFollowLive() && this.state !== "live") {
          this.goOfflineAndSchedule();
        }
      });
  }

  /** Browser-offline variant: the current attempt's rejection still settles. */
  private retireInFlightForOffline() {
    if (!this.resumeInFlight) return;
    this.resumeInFlight = false;
    this.resumeRetiredOffline = true;
  }

  private goOffline() {
    this.clearTimers("bind", "stale", "offline", "probe", "watchdog");
    this.retireInFlightForOffline();
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
        // AUTONOMOUS retry only: a manual resume/online explicitly demands a
        // reopen and is dispatched through beginResume directly. The backoff,
        // by contrast, must never replace a follow that re-certified live
        // (open + a frame within LIVE_FRAME_MS) while the clock was waiting —
        // doing so closed the working socket and stormed offline↔recovering
        // on a restored page (c-reconnfu gate 7). Certify under the frame
        // watchdog instead; a genuinely dead link is reopened by the frame
        // watchdog's stale/probe path as usual.
        if (this.deps.isFollowLive()) {
          this.attempt = 0;
          this.setState("live");
          this.armFrameWatchdog();
          return;
        }
        this.beginResume();
      }, delay),
    );
  }

  /**
   * Reopen the follow WITHOUT leaving quiet `stale`: REST is already proven by
   * the probe that triggered this, so the UI must not show recovering/offline.
   * A frame certifies live (the resume action resolves ok); a blocked/dead
   * upgrade rejects and the failure arm keeps stale. The 20 s watchdog still
   * bounds the attempt.
   */
  private quietReopen() {
    if (this.resumeInFlight) return;
    const attemptId = this.armResumeAttemptQuiet(this.latestBindGen);
    void this.deps
      .resume()
      .then(() => this.dispatch({ type: "resumeAttempt", ok: true, attemptId }))
      .catch(() => this.dispatch({ type: "resumeAttempt", ok: false, attemptId }));
  }

  /** beginResume variant that keeps a REST-proven stale state banner-less. */
  private armResumeAttemptQuiet(gen: number): number {
    // Gate 9 item 5: keep the stale-entry `offline` deadline armed across the
    // quiet reopen (real bound ≈ 15 s probe + read + 20 s watchdog; dropping
    // it left quiet stale unbounded when upgrades stayed blocked). Only the
    // stale/probe/reconnect timers are replaced; the watchdog bounds THIS
    // attempt, and on settle armStaleProbeTimers re-arms everything using the
    // carried deadline.
    this.clearTimers("stale", "probe", "reconnect");
    // NOTE: no setState("recovering") — stay in quiet stale.
    this.closedDuringResume = false; // gate 9 item 4: fresh attempt, clean latch
    this.resumeInFlight = true;
    this.resumeBindGen = gen;
    const attemptId = ++this.resumeAttemptId;
    this.timers.set(
      "watchdog",
      this.schedule(() => {
        if (
          !this.resumeInFlight ||
          this.resumeAttemptId !== attemptId ||
          this.resumeBindGen !== this.latestBindGen
        ) {
          return;
        }
        this.resumeInFlight = false;
        this.attempt = 0;
        this.armStaleProbeTimers();
        this.deps.onAttemptFinish?.({ gen, attemptId, why: "watchdog" });
      }, RECOVERING_WATCHDOG_MS),
    );
    return attemptId;
  }

  private beginResume() {
    // Coalesce only an attempt for the CURRENT binding: one armed for a mount
    // the user navigated away from is superseded and replaced wholesale.
    if (this.resumeInFlight && this.resumeBindGen === this.latestBindGen) return;
    const attemptId = this.armResumeAttempt();
    void this.deps
      .resume()
      .then(() => this.dispatch({ type: "resumeAttempt", ok: true, attemptId }))
      .catch(() => this.dispatch({ type: "resumeAttempt", ok: false, attemptId }));
  }

  /**
   * Store hook: report the generation of the current session binding. The
   * store bumps it on every bind to a DIFFERENT journal (navigation A → B).
   * Once advanced, an attempt armed for an older generation is superseded.
   *
   * The supersession is RETIRED here, not merely remembered: an attempt for
   * the journal the user left keeps its 20 s watchdog armed, and a frame on
   * the session returned to flips state to live without clearing that
   * watchdog (`armFrameWatchdog` owns only the frame timers). Without the
   * retire, the stale watchdog later fires and takes the RETURNED session
   * offline, reopening a second socket. The store's bind-away handler is the
   * other half — it settles the abandoned instance's own debt over REST.
   */
  noteBinding(gen: number) {
    if (gen === this.latestBindGen) return;
    this.latestBindGen = gen;
    if (this.resumeInFlight && this.resumeBindGen !== gen) {
      this.resumeInFlight = false;
      this.clearTimer("watchdog");
    }
  }

  /**
   * Identity of the resume attempt currently owning the link for the BOUND
   * binding, or null when none (idle live, or a just-superseded attempt). The
   * store stamps post-delivery catch-up debt with it so a later failure can
   * tell whether THIS attempt owed the row.
   */
  attemptRef(): { gen: number; attemptId: number } | null {
    if (!this.resumeInFlight || this.resumeBindGen !== this.latestBindGen) return null;
    return { gen: this.resumeBindGen, attemptId: this.resumeAttemptId };
  }

  /**
   * The store rebound to an ALREADY-MOUNTED session whose socket is OPEN and
   * fresh: certify the link against the new binding immediately, closing the
   * attempt a superseded mount still owned (its watchdog becomes a no-op).
   */
  followRebindLive() {
    this.resumeInFlight = false;
    this.attempt = 0;
    this.clearTimers("watchdog", "reconnect");
    this.setState("live");
    this.armFrameWatchdog();
  }

  /**
   * Live hand-off from a mount whose SEED LOST to an already-mounted journal,
   * scoped to the mount's captured binding generation and attempt id. Certifies
   * only when the call still names the CURRENT binding AND its exact in-flight
   * attempt: a deferred duplicate seed from an obsolete navigation (rapid
   * A → B → A → B) must not retire the newest mount's attempt and watchdog
   * (whose later failure would then be ignored, leaving a false live).
   */
  followAttemptLive(attemptId: number, gen: number) {
    if (gen !== this.latestBindGen) return;
    if (!this.resumeInFlight || this.resumeAttemptId !== attemptId) return;
    this.followRebindLive();
  }

  /**
   * Arm the recovering slot + watchdog WITHOUT running the resume action. The
   * INITIAL follow mount (REST journal seed + first subscribe) runs outside
   * resume() but must not leave the machine claiming live with no timer while
   * its seed read is pending: the caller reports the outcome with
   * {@link followAttemptEnd} exactly as resume() would.
   *
   * Returns the attempt id to report back with. When a resume ALREADY owns the
   * slot for the SAME binding (the reconnect resume retried the mount through
   * follow()), its watchdog covers the mount and its id is returned — the
   * nested mount's completion then certifies/fails the enclosing attempt,
   * never arms a second, competing watchdog. An in-flight attempt for an
   * OLDER binding (the user navigated A → B mid-seed) is replaced: B gets a
   * fresh attempt and A's later end is ignored.
   */
  followAttemptBegin(gen: number = this.latestBindGen): number {
    if (this.resumeInFlight && gen === this.resumeBindGen) return this.resumeAttemptId;
    return this.armResumeAttempt(gen);
  }

  /**
   * Report an externally-driven follow mount (see followAttemptBegin). A
   * completion for a superseded binding is dropped — a late A success must not
   * certify B, a late A failure must not take B offline.
   */
  followAttemptEnd(ok: boolean, attemptId: number, gen: number = this.latestBindGen) {
    if (gen !== this.latestBindGen) return;
    this.dispatch({ type: "resumeAttempt", ok, attemptId });
  }

  private armResumeAttempt(gen: number = this.latestBindGen): number {
    this.clearTimers("stale", "offline", "probe", "reconnect", "watchdog");
    this.resumeRetiredOffline = false;
    // Gate 9 item 4: every NEW attempt starts with a clean latch. A close
    // latched by a previous attempt must not poison later healthy resumes.
    this.closedDuringResume = false;
    this.setState("recovering");
    this.resumeInFlight = true;
    this.resumeBindGen = gen;
    const attemptId = ++this.resumeAttemptId;
    // The resume can never strand us in recovering: the watchdog forces the
    // attempt closed, and the promise also reports success/failure. Both carry
    // this id, so once a later attempt owns the slot the stale attempt's
    // watchdog and completion are ignored.
    this.timers.set(
      "watchdog",
      this.schedule(() => {
        // The bind-generation guard is independent of the attempt-id guard:
        // noteBinding retires a superseded attempt, but a late re-entry must
        // never drive the CURRENT session offline from an attempt bound to a
        // journal the user already left.
        if (
          !this.resumeInFlight ||
          this.resumeAttemptId !== attemptId ||
          this.resumeBindGen !== this.latestBindGen
        ) {
          return;
        }
        // A socket already certified live by frames owes no offline
        // transition: the resume action outlived its watchdog but the follow
        // demonstrably works — arm the frame watchdog and stay live instead of
        // replacing the socket (the same offline↔recovering storm as the
        // resumeAttempt failure arm; c-reconnfu gate 7). The exactly-once
        // onAttemptFinish contract still applies: gate 8 item 7 — the store's
        // owed-catch-up fallback keys off this callback.
        if (this.deps.isFollowLive()) {
          this.resumeInFlight = false;
          this.attempt = 0;
          this.setState("live");
          this.armFrameWatchdog();
          this.deps.onAttemptFinish?.({ gen, attemptId, why: "watchdog" });
          return;
        }
        this.resumeInFlight = false;
        this.attempt += 1;
        if (this.state === "stale") {
          // Quiet reopen watchdog: REST was proven; stay quiet stale.
          this.attempt = 0;
          this.armStaleProbeTimers();
        } else {
          // Same REST-reachable-but-follow-blocked decision as a rejected
          // resume action: settle quiet at stale (no banner, no storm) when
          // REST works.
          this.settleResumeFailureViaProbe();
        }
        this.deps.onAttemptFinish?.({ gen, attemptId, why: "watchdog" });
      }, RECOVERING_WATCHDOG_MS),
    );
    this.attempt += 1;
    return attemptId;
  }

  dispose() {
    for (const h of this.timers.values()) this.cancel(h);
    this.timers.clear();
  }
}
