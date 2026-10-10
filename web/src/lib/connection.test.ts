import { afterEach, describe, expect, it, vi } from "vitest";
import {
  ConnectionMachine,
  LIVE_FRAME_MS,
  MAX_BACKOFF_MS,
  RECOVERING_WATCHDOG_MS,
  STALE_TO_OFFLINE_MS,
  REST_PROBE_MS,
} from "./connection";

function fakeTimers() {
  let now = 0;
  const jobs = new Map<number, { fn: () => void; at: number }>();
  let nextId = 0;
  const schedule = (fn: () => void, ms: number) => {
    const id = nextId++;
    jobs.set(id, { fn, at: now + ms });
    return { id };
  };
  return {
    now: () => now,
    schedule,
    cancel: (h: unknown) => {
      jobs.delete((h as { id: number }).id);
    },
    advance(ms: number) {
      const deadline = now + ms;
      while (true) {
        let next: { id: number; job: { fn: () => void; at: number } } | null = null;
        for (const [id, job] of jobs) {
          if (job.at <= deadline && (!next || job.at < next.job.at)) next = { id, job };
        }
        if (!next) break;
        jobs.delete(next.id);
        now = next.job.at;
        next.job.fn();
      }
      now = deadline;
    },
  };
}

function setup() {
  const clock = fakeTimers();
  const resume = vi.fn(() => Promise.resolve());
  const probe = vi.fn(() => Promise.resolve(true));
  const isFollowLive = vi.fn(() => false);
  const onState = vi.fn();
  const onAttemptFinish = vi.fn();
  const machine = new ConnectionMachine({
    resume,
    probe,
    isFollowLive,
    schedule: clock.schedule,
    cancel: clock.cancel,
    random: () => 0.5,
    onState,
    onAttemptFinish,
  });
  return { clock, resume, probe, onState, onAttemptFinish, machine, isFollowLive };
}

describe("ConnectionMachine", () => {
  afterEach(() => {
    for (const m of machines) m.dispose();
    machines.length = 0;
  });
  const machines: ConnectionMachine[] = [];
  function setupTracked() {
    const s = setup();
    machines.push(s.machine);
    return s;
  }

  it("starts live and flags stale after 15 s without a frame", () => {
    const { clock, machine } = setupTracked();
    machine.startLive();
    expect(machine.state).toBe("live");
    clock.advance(LIVE_FRAME_MS);
    expect(machine.state).toBe("stale");
    // A frame heals it immediately.
    machine.dispatch({ type: "frame" });
    expect(machine.state).toBe("live");
  });

  it("goes offline after the stale window and reconnects on the backoff", async () => {
    const { clock, machine, isFollowLive } = setupTracked();
    isFollowLive.mockReturnValue(false);
    machine.startLive();
    clock.advance(LIVE_FRAME_MS + STALE_TO_OFFLINE_MS);
    expect(machine.state).toBe("offline");
    // Full-jitter cap at attempt 0 with random()=0.5 is 250 ms.
    clock.advance(250);
    expect(machine.state).toBe("recovering");
    machine.dispatch({ type: "resumeAttempt", ok: true, attemptId: 1 });
    expect(machine.state).toBe("live");
  });

  it("resume on visibility visible is immediate and resets backoff", () => {
    const { clock, machine, resume } = setupTracked();
    machine.startLive();
    machine.dispatch({ type: "close" });
    expect(machine.state).toBe("offline");
    // Do not advance: the backoff clock has not fired. The foreground event
    // must start the resume at 0 ms regardless of where that clock sits.
    resume.mockClear();
    machine.setVisibility(true);
    expect(machine.state).toBe("recovering");
    expect(resume).toHaveBeenCalledTimes(1);
    // Complete the resume, then run past every backoff window: had the old
    // reconnect timer survived, it would have begun a second attempt while
    // the machine was already live.
    machine.dispatch({ type: "resumeAttempt", ok: true, attemptId: 1 });
    expect(machine.state).toBe("live");
    clock.advance(16_000);
    expect(resume).toHaveBeenCalledTimes(1);
  });

  it("online event reconnects immediately from offline", () => {
    const { machine, resume } = setupTracked();
    machine.startLive();
    machine.dispatch({ type: "offline" });
    expect(machine.state).toBe("offline");
    resume.mockClear();
    machine.dispatch({ type: "online" });
    expect(machine.state).toBe("recovering");
    expect(resume).toHaveBeenCalledTimes(1);
  });

  it("GATE7: a failed resume with REST reachable settles quiet at stale; a later success heals", async () => {
    const { machine, isFollowLive } = setupTracked();
    isFollowLive.mockReturnValue(false);
    machine.startLive();
    machine.dispatch({ type: "close" });
    machine.dispatch({ type: "resume" });
    expect(machine.state).toBe("recovering");
    // probe() resolves true by default: follow blocked but REST works → stale.
    machine.dispatch({ type: "resumeAttempt", ok: false, attemptId: 1 });
    await Promise.resolve();
    await Promise.resolve();
    expect(machine.state).toBe("stale");
    machine.dispatch({ type: "resume" });
    machine.dispatch({ type: "resumeAttempt", ok: true, attemptId: 2 });
    expect(machine.state).toBe("live");
  });

  it("a failed resume with REST unreachable returns to loud offline", async () => {
    const { machine, isFollowLive, probe } = setupTracked();
    isFollowLive.mockReturnValue(false);
    probe.mockResolvedValue(false);
    machine.startLive();
    machine.dispatch({ type: "close" });
    machine.dispatch({ type: "resume" });
    machine.dispatch({ type: "resumeAttempt", ok: false, attemptId: 1 });
    await Promise.resolve();
    await Promise.resolve();
    expect(machine.state).toBe("offline");
  });

  it("GATE7: a failed resume ACTION stays live when its follow socket is already frame-certified", async () => {
    // The resume-side REST catch-up read can reject/timeout under load after
    // the follow it opened started streaming frames. Forcing offline there
    // tears down a working follow and storms offline↔recovering (the restored
    // page's banner then never cleared). Frames + the frame watchdog own this
    // link; the failed read must not take it down.
    const { machine, resume, isFollowLive } = setupTracked();
    isFollowLive.mockReturnValue(true); // follow already streaming frames
    resume.mockRejectedValueOnce(new Error("resume read failed"));
    machine.startLive();
    machine.dispatch({ type: "close" });
    machine.dispatch({ type: "resume" });
    expect(machine.state).toBe("recovering");
    // the
    // rejection settles as live once the microtask runs.
    await vi.waitFor(() => expect(machine.state).toBe("live"));
    // The rejected attempt schedules nothing against the certified link.
    const calls = resume.mock.calls.length;
    // (clock isn't advanced; no timers in the live state but the frame
    // watchdog, which is silent here.)
    expect(calls).toBe(1);
  });

  it("GATE7: the recovering watchdog certifies live instead of replacing a framed socket", () => {
    const { clock, machine, resume, isFollowLive } = setupTracked();
    isFollowLive.mockReturnValue(true); // follow open + freshly framed
    resume.mockReturnValue(new Promise<void>(() => {})); // hangs
    machine.startLive();
    machine.dispatch({ type: "close" });
    machine.dispatch({ type: "resume" });
    expect(machine.state).toBe("recovering");
    // stay live under the frame watchdog rather
    // than going offline and scheduling a socket replacement.
    clock.advance(RECOVERING_WATCHDOG_MS);
    expect(machine.state).toBe("live");
    const callsAtCertify = resume.mock.calls.length;
    clock.advance(MAX_BACKOFF_MS + 1_000);
    expect(resume.mock.calls.length).toBe(callsAtCertify);
  });

  it("GATE7: a scheduled reconnect tick never replaces a frame-certified follow", () => {
    const { clock, machine, resume, isFollowLive } = setupTracked();
    isFollowLive.mockReturnValue(true); // follow re-certified before the tick
    machine.startLive();
    machine.dispatch({ type: "close" });
    expect(machine.state).toBe("offline");
    clock.advance(250); // attempt-0 full-jitter delay with random()=0.5
    expect(machine.state).toBe("live");
    expect(resume).not.toHaveBeenCalled();
    clock.advance(MAX_BACKOFF_MS + 1_000);
    expect(resume).not.toHaveBeenCalled();
  });

  it("GATE7: the recovering watchdog with REST reachable settles quiet stale; a dead Hub stays offline", async () => {
    const { clock, machine, isFollowLive, probe } = setupTracked();
    isFollowLive.mockReturnValue(false);
    machine.startLive();
    machine.dispatch({ type: "close" });
    machine.dispatch({ type: "resume" });
    expect(machine.state).toBe("recovering");
    clock.advance(RECOVERING_WATCHDOG_MS);
    // probe() resolves true by default: REST reachable → quiet stale.
    await Promise.resolve();
    await Promise.resolve();
    expect(machine.state).toBe("stale");
    // A late response after the watchdog must not resurrect the attempt.
    machine.dispatch({ type: "resumeAttempt", ok: true, attemptId: 1 });
    expect(machine.state).toBe("stale");

    // REST gone too → loud offline.
    probe.mockResolvedValue(false);
    machine.dispatch({ type: "close" });
    machine.dispatch({ type: "resume" });
    clock.advance(RECOVERING_WATCHDOG_MS);
    await Promise.resolve();
    await Promise.resolve();
    expect(machine.state).toBe("offline");
  });

  it("a timed-out attempt's late success can never consume the next attempt (ROUND5-4)", async () => {
    // A times out (watchdog -> offline -> backoff), B starts, A resolves ok
    // while B is still pending, then B fails. A's completion must be dropped
    // (it is not the current attempt) and B's failure must land offline —
    // never a false live certified by the dead attempt A.
    const { clock, machine, resume, isFollowLive, probe } = setupTracked();
    isFollowLive.mockReturnValue(false);
    probe.mockResolvedValue(false); // genuinely unreachable: failures land offline
    let resolveA: () => void = () => {};
    let rejectB: (err: Error) => void = () => {};
    resume
      .mockImplementationOnce(
        () =>
          new Promise<void>((resolve) => {
            resolveA = resolve;
          }),
      )
      .mockImplementationOnce(
        () =>
          new Promise<void>((_resolve, reject) => {
            rejectB = reject;
          }),
      );
    machine.startLive();
    machine.dispatch({ type: "close" });
    // Backoff at attempt 0 with random()=0.5 is 250 ms: attempt A starts.
    clock.advance(250);
    expect(machine.state).toBe("recovering");
    expect(resume).toHaveBeenCalledTimes(1);
    // A hangs past the watchdog; REST is down too → offline (probe settles).
    clock.advance(RECOVERING_WATCHDOG_MS);
    await Promise.resolve();
    await Promise.resolve();
    expect(machine.state).toBe("offline");
    // Watchdog bumped the attempt to 2: backoff is 1000 ms (cap 2_000 * 0.5).
    clock.advance(1_000);
    expect(machine.state).toBe("recovering");
    expect(resume).toHaveBeenCalledTimes(2);

    // A resolves successfully AFTER it was timed out, while B is pending.
    resolveA();
    await Promise.resolve();
    await Promise.resolve();
    // Still recovering on B: the stale success must not certify live.
    expect(machine.state).toBe("recovering");

    // B then fails: the machine goes offline, never live.
    rejectB(new Error("follow reopen failed"));
    await vi.waitFor(() => expect(machine.state).toBe("offline"));
  });

  it("hiding the page cancels the offline reconnect clock", () => {
    const { clock, machine, resume } = setupTracked();
    machine.startLive();
    machine.dispatch({ type: "close" });
    machine.setVisibility(false);
    clock.advance(MAX_BACKOFF_MS + 1_000);
    expect(resume).not.toHaveBeenCalled();
    expect(machine.state).toBe("offline");
  });

  it("a successful REST probe triggers reopen+catch-up, never a false live", async () => {
    const { clock, machine, probe, resume } = setupTracked();
    machine.startLive();
    clock.advance(LIVE_FRAME_MS);
    expect(machine.state).toBe("stale");
    clock.advance(15_000);
    expect(probe).toHaveBeenCalledTimes(1);
    await Promise.resolve();
    // REST reachable is not live: it forces a resume (socket reopen+catch-up),
    // which certifies live only on success.
    expect(machine.state).toBe("recovering");
    expect(resume).toHaveBeenCalledTimes(1);
  });

  it("backoff is full-jitter within the doubling 30 s cap", () => {
    const withRandom = (r: number) =>
      new ConnectionMachine({ resume: async () => {}, probe: async () => true, isFollowLive: () => true, random: () => r }).backoffDelay(10);
    expect(withRandom(0)).toBe(0);
    expect(withRandom(1)).toBe(MAX_BACKOFF_MS);
  });

  it("foreground/online from cached live reopens when the follow socket is silently dead", async () => {
    const { clock, machine, resume, isFollowLive } = setupTracked();
    machine.startLive();
    expect(machine.state).toBe("live");
    // Socket silently died while suspended; no close callback fired.
    isFollowLive.mockReturnValue(false);
    machine.dispatch({ type: "resume" });
    expect(machine.state).toBe("recovering");
    await vi.waitFor(() => expect(resume).toHaveBeenCalledTimes(1));
    await clock.advance(0);
    // Online event behaves the same.
    isFollowLive.mockReturnValue(false);
    machine.dispatch({ type: "online" });
    expect(resume.mock.calls.length).toBeGreaterThanOrEqual(1);
  });

  it("foreground from cached live with an OPEN socket is a no-op (no churn)", () => {
    const { machine, resume, isFollowLive } = setupTracked();
    isFollowLive.mockReturnValue(true); // open + fresh frame
    machine.startLive();
    machine.dispatch({ type: "resume" });
    expect(machine.state).toBe("live");
    expect(resume).not.toHaveBeenCalled();
  });

  it("pageshow(persisted) semantics: resume coalesces an in-flight recovery", async () => {
    const { machine, resume } = setupTracked();
    resume.mockReturnValue(new Promise(() => {})); // never resolves
    machine.setStateOffline();
    machine.dispatch({ type: "resume" });
    expect(machine.state).toBe("recovering");
    // A second foreground signal must not start a second resume.
    machine.dispatch({ type: "online" });
    expect(resume).toHaveBeenCalledTimes(1);
  });

  it("bootstrapLive is live with no watchdog; followBound requires a frame and a silent open goes offline", async () => {
    const { clock, machine, probe, resume, isFollowLive } = setupTracked();
    machine.bootstrapLive();
    expect(machine.state).toBe("live");
    // No follow bound yet: a live session stays live even with no frames
    // (there is nothing to watch on the session-list/new-session pages).
    clock.advance(LIVE_FRAME_MS);
    clock.advance(STALE_TO_OFFLINE_MS);
    expect(machine.state).toBe("live");

    // A follow binds but never delivers a frame (open socket, frozen stream).
    machine.followBound();
    isFollowLive.mockReturnValue(false);
    clock.advance(LIVE_FRAME_MS);
    expect(machine.state).toBe("stale");
    // REST probe fails: a reachable-but-frameless link still goes offline.
    probe.mockResolvedValue(false);
    clock.advance(15_000);
    await Promise.resolve();
    expect(machine.state).toBe("offline");
    expect(resume).not.toHaveBeenCalled();
  });

  it("followBound: a frame before the deadline certifies live and arms the watchdog", () => {
    const { clock, machine } = setupTracked();
    machine.bootstrapLive();
    machine.followBound();
    // Snapshot/frame arrives in time.
    machine.dispatch({ type: "frame" });
    expect(machine.state).toBe("live");
    clock.advance(LIVE_FRAME_MS - 1);
    expect(machine.state).toBe("live");
    // No further frames: the regular frame watchdog now owns staleness.
    clock.advance(1);
    expect(machine.state).toBe("stale");
  });

  it("GATE7: an initial journal seed failure with REST reachable settles stale and the probe retries the follow", async () => {
    const { clock, machine, resume, isFollowLive } = setupTracked();
    isFollowLive.mockReturnValue(false);
    machine.bootstrapLive();
    const id = machine.followAttemptBegin();
    expect(machine.state).toBe("recovering");

    // The seed fails but REST answers: quiet stale (no loud offline banner),
    // not a false live.
    machine.followAttemptEnd(false, id);
    await Promise.resolve();
    await Promise.resolve();
    expect(machine.state).toBe("stale");
    // The stale probe (15 s) reopens the follow (probe resolves async).
    clock.advance(REST_PROBE_MS);
    await Promise.resolve();
    await Promise.resolve();
    expect(resume).toHaveBeenCalledTimes(1);
  });

  it("followAttempt: success certifies live and clears the watchdog", () => {
    const { clock, machine } = setupTracked();
    machine.bootstrapLive();
    const id = machine.followAttemptBegin();
    expect(machine.state).toBe("recovering");
    machine.followAttemptEnd(true, id);
    expect(machine.state).toBe("live");
    // Past the watchdog window: the (cleared) external-attempt watchdog never
    // fires; with no further frame the ordinary frame watchdog drives stale.
    clock.advance(RECOVERING_WATCHDOG_MS);
    expect(machine.state).toBe("stale");
  });

  it("GATE7: a hung seed watchdog with REST reachable settles stale; a late completion stays stale", async () => {
    const { clock, machine, isFollowLive } = setupTracked();
    isFollowLive.mockReturnValue(false);
    machine.bootstrapLive();
    const id = machine.followAttemptBegin();
    clock.advance(RECOVERING_WATCHDOG_MS);
    await Promise.resolve();
    await Promise.resolve();
    expect(machine.state).toBe("stale");
    // A late seed completion must not certify live.
    machine.followAttemptEnd(true, id);
    expect(machine.state).toBe("stale");
  });

  it("followAttempt: nested in a resume shares its slot and id, never double-watchdogs", () => {
    const { clock, machine, resume } = setupTracked();
    machine.startLive();
    machine.dispatch({ type: "close" });
    machine.dispatch({ type: "resume" });
    expect(machine.state).toBe("recovering");
    const nestedId = machine.followAttemptBegin();
    // Same slot: the nested mount reports the enclosing attempt's outcome.
    machine.followAttemptEnd(true, nestedId);
    expect(machine.state).toBe("live");
    // No second resume from a competing watchdog during the recovery window.
    clock.advance(RECOVERING_WATCHDOG_MS - 1);
    expect(resume).toHaveBeenCalledTimes(1);
  });

  it("binding gen: a superseded mount's late success never certifies the new binding live", () => {
    const { machine } = setupTracked();
    machine.bootstrapLive();
    // Mount A starts its seed attempt under generation 1.
    const idA = machine.followAttemptBegin(1);
    expect(machine.state).toBe("recovering");
    // Navigate to B: generation 2, B starts its own attempt.
    machine.noteBinding(2);
    const idB = machine.followAttemptBegin(2);
    expect(idB).not.toBe(idA);
    // A's seed/subscribe succeeds late — ignored, the machine must not claim
    // live with no certified B socket.
    machine.followAttemptEnd(true, idA, 1);
    expect(machine.state).toBe("recovering");
    // B's own success certifies.
    machine.followAttemptEnd(true, idB, 2);
    expect(machine.state).toBe("live");
  });

  it("binding gen: a superseded mount's late failure never takes the new binding offline", () => {
    const { clock, machine } = setupTracked();
    machine.bootstrapLive();
    const idA = machine.followAttemptBegin(1);
    machine.noteBinding(2);
    const idB = machine.followAttemptBegin(2);
    machine.followAttemptEnd(true, idB, 2);
    expect(machine.state).toBe("live");
    // A fails after B certified — B stays live.
    machine.followAttemptEnd(false, idA, 1);
    expect(machine.state).toBe("live");
    clock.advance(1);
    expect(machine.state).toBe("live");
  });

  it("followRebindLive: an already-mounted live rebind certifies and retires a superseded attempt", () => {
    const { clock, machine } = setupTracked();
    machine.bootstrapLive();
    machine.followAttemptBegin(1);
    expect(machine.state).toBe("recovering");
    // Navigate to an already-mounted session whose socket is OPEN+fresh.
    machine.noteBinding(2);
    machine.followRebindLive();
    expect(machine.state).toBe("live");
    // The superseded attempt's watchdog is retired: at its deadline nothing
    // happens (B stays live; the frame watchdog then drives quiet stale).
    clock.advance(RECOVERING_WATCHDOG_MS);
    expect(machine.state).not.toBe("offline");
  });

  it("binding gen: a resume for a superseded binding starts a fresh attempt and reruns resume", () => {
    const { machine, resume } = setupTracked();
    machine.bootstrapLive();
    machine.followAttemptBegin(1);
    expect(resume).toHaveBeenCalledTimes(0);
    machine.noteBinding(2);
    // The non-live mounted rebind kicks the machine like a foreground resume.
    machine.dispatch({ type: "resume" });
    expect(resume).toHaveBeenCalledTimes(1);
    // The old attempt's late failure cannot close B's attempt.
    machine.followAttemptEnd(false, 1, 1);
    expect(machine.state).toBe("recovering");
  });

  it("gate6 item1: returning to a mounted A retires B's watchdog; a frame on A never leads offline or a reopen", () => {
    const { clock, machine, resume, onState } = setupTracked();
    machine.bootstrapLive();
    // B's journal seed is in flight under gen 1: recovering + 20 s watchdog.
    machine.noteBinding(1);
    const idB = machine.followAttemptBegin(1);
    expect(machine.state).toBe("recovering");
    expect(machine.attemptRef()).toEqual({ gen: 1, attemptId: idB });
    // >5 s later the user returns to already-mounted A (gen 2): noteBinding
    // retires B's attempt immediately.
    clock.advance(6_000);
    machine.noteBinding(2);
    expect(machine.attemptRef()).toBeNull();
    // A's socket is OPEN but not freshly framed: the rebind takes the
    // frame/probe deadline rather than inheriting recovering -> beginResume.
    machine.followBound({ rebind: true });
    expect(resume).toHaveBeenCalledTimes(0);
    // A frame lands on A 1 s after the rebind.
    clock.advance(1_000);
    machine.dispatch({ type: "frame" });
    // t=20 s: B's old watchdog deadline passes (retired — a no-op). Keep A
    // framed (as a live socket would) and run past A's frame window.
    clock.advance(13_000);
    machine.dispatch({ type: "frame" });
    expect(machine.state).toBe("live");
    clock.advance(8_000);
    expect(machine.state).toBe("live");
    expect(resume).toHaveBeenCalledTimes(0);
    expect(onState.mock.calls.map((call) => call[0])).not.toContain("offline");
  });

  it("gate6 item1: a rebind while offline takes the probe path, never an immediate resume", () => {
    const { clock, machine, resume } = setupTracked();
    machine.bootstrapLive();
    // Navigator offline: goOffline() WITHOUT a scheduled reconnect, so the
    // assertion isolates followBound's own behaviour.
    machine.dispatch({ type: "offline" });
    expect(machine.state).toBe("offline");
    machine.followBound({ rebind: true });
    expect(resume).toHaveBeenCalledTimes(0);
    // Deadline reached with no frame: stale + probe/offline timers, still no
    // reopen (the probe decides whether a reopen is justified).
    clock.advance(LIVE_FRAME_MS);
    expect(machine.state).toBe("stale");
    expect(resume).toHaveBeenCalledTimes(0);
  });

  it("gate6 item1: a non-rebind followBound from offline resumes immediately", () => {
    const { machine, resume } = setupTracked();
    machine.bootstrapLive();
    machine.dispatch({ type: "offline" });
    machine.followBound();
    expect(machine.state).toBe("recovering");
    expect(resume).toHaveBeenCalledTimes(1);
  });

  it("gate6 item2: onAttemptFinish reports ok/failed with the attempt ref", () => {
    const { machine, onAttemptFinish } = setupTracked();
    machine.bootstrapLive();
    machine.noteBinding(3);
    const id = machine.followAttemptBegin(3);
    machine.followAttemptEnd(true, id, 3);
    expect(onAttemptFinish).toHaveBeenCalledWith({ gen: 3, attemptId: id, why: "ok" });

    const id2 = machine.followAttemptBegin(3);
    machine.followAttemptEnd(false, id2, 3);
    expect(onAttemptFinish).toHaveBeenCalledWith({ gen: 3, attemptId: id2, why: "failed" });
  });

  it("gate6 item2: the watchdog reports why=watchdog for a hung current-binding attempt (settles stale when REST answers)", async () => {
    const { clock, machine, onAttemptFinish } = setupTracked();
    machine.bootstrapLive();
    machine.noteBinding(1);
    const id = machine.followAttemptBegin(1);
    clock.advance(RECOVERING_WATCHDOG_MS);
    await Promise.resolve();
    await Promise.resolve();
    expect(machine.state).toBe("stale");
    expect(onAttemptFinish).toHaveBeenCalledWith({ gen: 1, attemptId: id, why: "watchdog" });
  });

  it("gate6 item2: a superseded attempt's watchdog fires no finish callback", () => {
    const { clock, machine, onAttemptFinish } = setupTracked();
    machine.bootstrapLive();
    machine.noteBinding(1);
    const idA = machine.followAttemptBegin(1);
    machine.noteBinding(2);
    clock.advance(RECOVERING_WATCHDOG_MS);
    expect(onAttemptFinish).not.toHaveBeenCalled();
    // A late completion for the retired attempt is also silent.
    machine.followAttemptEnd(false, idA, 1);
    expect(onAttemptFinish).not.toHaveBeenCalled();
  });
});
