import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
/**
 * c-perfaudit: the profiler is opt-in via `?profile=1`. The default (no flag)
 * module must keep every existing behaviour — no PerformanceObserver, no
 * window global, and the region wrapper just calls the function.
 */

// jsdom loads modules against the test document URL; pushState changes the
// query before a fresh dynamic import so the module-singleton flag is read
// under the URL we want.
async function importProfileFlags() {
  return import("./profileFlags");
}

function setSearch(search: string) {
  window.history.pushState({}, "", `/${search}`);
}

describe("profileFlags (default off)", () => {
  beforeEach(() => {
    setSearch("");
  });

  afterEach(() => {
    vi.resetModules();
    delete (window as unknown as { __remudaPerf?: unknown }).__remudaPerf;
  });

  it("constructs no PerformanceObserver and exposes no window API by default", async () => {
    const ctor = vi.fn();
    vi.stubGlobal("PerformanceObserver", ctor);
    const mod = await importProfileFlags();
    expect(mod.profilingEnabled).toBe(false);
    expect(ctor).not.toHaveBeenCalled();
    expect(window.__remudaPerf).toBeUndefined();
    vi.unstubAllGlobals();
  });

  it("profileRegion is a passthrough with no recorded samples when off", async () => {
    const ctor = vi.fn();
    vi.stubGlobal("PerformanceObserver", ctor);
    const mod = await importProfileFlags();
    const fn = vi.fn((x: number) => x + 1);
    expect(mod.profileRegion("anything", () => fn(41))).toBe(42);
    expect(fn).toHaveBeenCalledOnce();
    // Scenario marks and probes are silent no-ops.
    mod.markProfileScenario("A");
    mod.markProfileScenario(null);
    mod.reportProbe("kind", "v");
    expect(ctor).not.toHaveBeenCalled();
    vi.unstubAllGlobals();
  });
});

describe("profileFlags (?profile=1)", () => {
  type CapturedObserver = {
    callback: (list: { getEntries: () => PerformanceEntry[] }) => void;
  };

  let observers: CapturedObserver[];

  beforeEach(() => {
    setSearch("?profile=1");
    observers = [];
    // NOTE: a real PerformanceObserver is invoked with `new`, and an arrow
    // function is not constructible — the stub must be a `function` or class.
    const ctor = vi.fn(function (this: unknown, callback: CapturedObserver["callback"]) {
      const observer: CapturedObserver = { callback };
      observers.push(observer);
      return {
        observe: vi.fn(),
        disconnect: vi.fn(),
      };
    });
    vi.stubGlobal("PerformanceObserver", ctor);
  });

  afterEach(() => {
    vi.resetModules();
    vi.unstubAllGlobals();
    delete (window as unknown as { __remudaPerf?: unknown }).__remudaPerf;
    setSearch("");
  });

  function emitLongTask(
    observer: CapturedObserver,
    entry: { startTime: number; duration: number; attribution?: unknown },
  ) {
    observer.callback({
      getEntries: () => [entry as PerformanceEntry],
    });
  }

  it("installs the window API and records scenarios, probes and regions", async () => {
    const mod = await importProfileFlags();
    expect(mod.profilingEnabled).toBe(true);
    expect(window.__remudaPerf?.enabled).toBe(true);

    mod.markProfileScenario("scenario-A");
    const value = mod.profileRegion("region-x", () => "ok");
    expect(value).toBe("ok");
    mod.reportProbe("terminal-renderer", "webgl");
    mod.markProfileScenario(null);

    const report = window.__remudaPerf!.getReport();
    expect(report.enabled).toBe(true);
    expect(report.scenario).toBeNull();
    expect(report.scenarios).toHaveLength(1);
    expect(report.scenarios[0]!.name).toBe("scenario-A");
    expect(report.scenarios[0]!.end).not.toBeNull();
    expect(report.probes).toEqual([
      { kind: "terminal-renderer", value: "webgl", at: expect.any(Number) },
    ]);
    expect(report.regions).toHaveLength(1);
    // The call site is attributed outside the profiler module itself.
    expect(report.regions[0]!.label).toBe("region-x");
    expect(report.regions[0]!.stackTop).toMatch(/profileFlags\.test\.ts:\d+/);
  });

  it("tags long tasks with the active scenario and instrumented region", async () => {
    const mod = await importProfileFlags();
    mod.markProfileScenario("scenario-A");
    // Fake timestamps: a 120ms task containing region-y.
    mod.profileRegion("region-y", () => undefined);
    const region = window.__remudaPerf!.getReport().regions[0]!;
    emitLongTask(observers[0]!, {
      startTime: region.start,
      duration: region.end - region.start + 120,
      attribution: [
        {
          containerName: "iframe",
          containerSrc: "http://127.0.0.1:58889/s/inst-1/tty?profile=1",
        },
      ],
    });

    const report = window.__remudaPerf!.getReport();
    expect(report.longTasks).toHaveLength(1);
    const task = report.longTasks[0]!;
    expect(task.duration).toBeGreaterThan(119.5);
    expect(task.duration).toBeLessThan(120.5);
    expect(task.scenario).toBe("scenario-A");
    expect(task.region?.label).toBe("region-y");
    // Origin is stripped: no host/port survives into the report.
    expect(task.container).toBe("/s/inst-1/tty");
    expect(JSON.stringify(task)).not.toMatch(/127\.0\.0\.1|58889/);
  });

  it("reports unattributed long tasks honestly and resets on demand", async () => {
    await importProfileFlags();
    emitLongTask(observers[0]!, { startTime: 5, duration: 75, attribution: [] });
    const first = window.__remudaPerf!.getReport();
    expect(first.longTasks).toHaveLength(1);
    expect(first.longTasks[0]!.region).toBeNull();
    window.__remudaPerf!.reset();
    const after = window.__remudaPerf!.getReport();
    expect(after.longTasks).toHaveLength(0);
    expect(after.regions).toHaveLength(0);
  });
});
