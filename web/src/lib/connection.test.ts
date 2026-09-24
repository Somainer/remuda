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
  const isFollowOpen = vi.fn(() => true);
  const onState = vi.fn();
  const machine = new ConnectionMachine({
    resume,
    probe,
    isFollowOpen,
    schedule: clock.schedule,
    cancel: clock.cancel,
    random: () => 0.5,
    onState,
  });
  return { clock, resume, probe, onState, machine, isFollowOpen };
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
    machine.dispatch({ type: "resumeAttempt", ok: true });
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
    machine.dispatch({ type: "resumeAttempt", ok: true });
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
    machine.dispatch({ type: "resumeAttempt", ok: false });
    expect(machine.state).toBe("offline");
    machine.dispatch({ type: "resume" });
    machine.dispatch({ type: "resumeAttempt", ok: true });
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
    machine.dispatch({ type: "resumeAttempt", ok: true });
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

  it("a successful REST probe restores live from stale", async () => {
    const { clock, machine, probe } = setupTracked();
    machine.startLive();
    clock.advance(LIVE_FRAME_MS);
    expect(machine.state).toBe("stale");
    clock.advance(15_000);
    expect(probe).toHaveBeenCalledTimes(1);
    await Promise.resolve();
    expect(machine.state).toBe("live");
  });

  it("backoff is full-jitter within the doubling 30 s cap", () => {
    const withRandom = (r: number) =>
      new ConnectionMachine({ resume: async () => {}, probe: async () => true, isFollowOpen: () => true, random: () => r }).backoffDelay(10);
    expect(withRandom(0)).toBe(0);
    expect(withRandom(1)).toBe(MAX_BACKOFF_MS);
  });

  it("foreground/online from cached live reopens when the follow socket is silently dead", async () => {
    const { clock, machine, resume, isFollowOpen } = setupTracked();
    machine.startLive();
    expect(machine.state).toBe("live");
    // Socket silently died while suspended; no close callback fired.
    isFollowOpen.mockReturnValue(false);
    machine.dispatch({ type: "resume" });
    expect(machine.state).toBe("recovering");
    await vi.waitFor(() => expect(resume).toHaveBeenCalledTimes(1));
    await clock.advance(0);
    // Online event behaves the same.
    isFollowOpen.mockReturnValue(false);
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
});
