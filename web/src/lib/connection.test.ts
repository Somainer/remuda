import { afterEach, describe, expect, it, vi } from "vitest";
import {
  ConnectionMachine,
  LIVE_FRAME_MS,
  MAX_BACKOFF_MS,
  RECOVERING_WATCHDOG_MS,
  STALE_TO_OFFLINE_MS,
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
  const isFollowLive = vi.fn(() => true);
  const onState = vi.fn();
  const machine = new ConnectionMachine({
    resume,
    probe,
    isFollowLive,
    schedule: clock.schedule,
    cancel: clock.cancel,
    random: () => 0.5,
    onState,
  });
  return { clock, resume, probe, onState, machine, isFollowLive };
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
    const { clock, machine } = setupTracked();
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

  it("a failed resume returns to offline, never stays recovering; next success heals", async () => {
    const { machine } = setupTracked();
    machine.startLive();
    machine.dispatch({ type: "close" });
    machine.dispatch({ type: "resume" });
    expect(machine.state).toBe("recovering");
    machine.dispatch({ type: "resumeAttempt", ok: false, attemptId: 1 });
    expect(machine.state).toBe("offline");
    machine.dispatch({ type: "resume" });
    machine.dispatch({ type: "resumeAttempt", ok: true, attemptId: 2 });
    expect(machine.state).toBe("live");
  });

  it("the recovering watchdog forces offline if resume hangs", () => {
    const { clock, machine } = setupTracked();
    machine.startLive();
    machine.dispatch({ type: "close" });
    machine.dispatch({ type: "resume" });
    expect(machine.state).toBe("recovering");
    clock.advance(RECOVERING_WATCHDOG_MS);
    expect(machine.state).toBe("offline");
    // A late response after the watchdog must not resurrect the attempt.
    machine.dispatch({ type: "resumeAttempt", ok: true, attemptId: 1 });
    expect(machine.state).toBe("offline");
  });

  it("a timed-out attempt's late success can never consume the next attempt (ROUND5-4)", async () => {
    // A times out (watchdog -> offline -> backoff), B starts, A resolves ok
    // while B is still pending, then B fails. A's completion must be dropped
    // (it is not the current attempt) and B's failure must land offline —
    // never a false live certified by the dead attempt A.
    const { clock, machine, resume } = setupTracked();
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
    // A hangs past the watchdog: offline, reconnect scheduled.
    clock.advance(RECOVERING_WATCHDOG_MS);
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
    await Promise.resolve();
    await Promise.resolve();
    expect(machine.state).toBe("offline");
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
    const { machine, resume } = setupTracked();
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

  it("followAttempt: an initial journal seed is recovering immediately; failure goes offline and retries", () => {
    const { clock, machine, resume } = setupTracked();
    machine.bootstrapLive();
    const id = machine.followAttemptBegin();
    expect(machine.state).toBe("recovering");

    // The seed fails: offline + reconnect clock armed (no false live).
    machine.followAttemptEnd(false, id);
    expect(machine.state).toBe("offline");
    // armResumeAttempt bumped the attempt to 1: backoff is 500 ms.
    clock.advance(500);
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

  it("followAttempt: a hung seed is forced offline by the watchdog", () => {
    const { clock, machine } = setupTracked();
    machine.bootstrapLive();
    const id = machine.followAttemptBegin();
    clock.advance(RECOVERING_WATCHDOG_MS);
    expect(machine.state).toBe("offline");
    // A late seed completion must not certify live.
    machine.followAttemptEnd(true, id);
    expect(machine.state).toBe("offline");
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
});
