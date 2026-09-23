/**
 * c-perfaudit opt-in frontend profiling.
 *
 * Enabled ONLY when the page URL carries `?profile=1`. When disabled the
 * module does not construct a PerformanceObserver, touch `window`, or keep any
 * buffers — `profileRegion` is one direct function call — so production
 * behaviour and timing are unchanged. The perf Playwright scenarios
 * (`web/tests/perf/scenarios.perf.ts`) read the collected report from
 * `window.__remudaPerf`.
 *
 * Collected:
 *  - Long Tasks (>50 ms) via `PerformanceObserver({ entryTypes: ["longtask"] })`,
 *    tagged with the scenario active while they ran and, when one of the
 *    instrumented regions was executing inside the task, the region label and
 *    its instrumented `file:line`.
 *  - Scenario intervals (`markScenario`) and generic probes (`reportProbe`,
 *    e.g. the terminal renderer — see features/session/tty/rendererProbe.ts).
 *
 * The Long Task attribution browsers expose does not include a JS call stack,
 * so the stack-top `file:line` comes from the region that executed inside the
 * task. Long tasks covered by no instrumented region report `region: null`
 * plus the observer attribution container — that gap is reported honestly
 * rather than guessed.
 */

export type ProfileRegionSample = {
  label: string;
  start: number;
  end: number;
  /** First non-profiler stack frame at the region's instrumented call site. */
  stackTop: string | null;
};

export type ProfileScenarioInterval = {
  name: string;
  start: number;
  end: number | null;
};

export type ProfileLongTask = {
  startTime: number;
  duration: number;
  scenario: string | null;
  region: { label: string; stackTop: string | null } | null;
  /** PerformanceLongTaskAttribution container name/src, host-stripped. */
  container: string | null;
};

export type ProfileProbe = {
  kind: string;
  value: unknown;
  at: number;
};

export type ProfileReport = {
  enabled: boolean;
  startedAt: number;
  userAgent: string;
  scenario: string | null;
  scenarios: ProfileScenarioInterval[];
  longTasks: ProfileLongTask[];
  regions: ProfileRegionSample[];
  probes: ProfileProbe[];
};

export type RemudaPerfApi = {
  readonly enabled: true;
  markScenario: (name: string | null) => void;
  reportProbe: (kind: string, value: unknown) => void;
  getReport: () => ProfileReport;
  reset: () => void;
};

declare global {
  interface Window {
    __remudaPerf?: RemudaPerfApi;
  }
}

/** Strip the origin so reports never carry hostnames/ports/IPs. */
function sourcePath(urlLike: string | null | undefined): string | null {
  if (!urlLike) return null;
  const withoutQuery = urlLike.split("?")[0] ?? urlLike;
  const noProto = withoutQuery.replace(/^[a-z][a-z0-9+.-]*:\/\//, "");
  if (noProto.startsWith("/")) return noProto.replace(/^\/+/, "/"); // file: URL
  // network URL: drop the host[:port] prefix.
  const slash = noProto.indexOf("/");
  return slash >= 0 ? noProto.slice(slash) : noProto;
}

/**
 * First stack frame outside this module. V8 captures the stack at `Error`
 * construction but only formats it lazily on `.stack`, so this is cheap until
 * actually read.
 */
function readStackTop(error: Error): string | null {
  const stack = error.stack;
  if (!stack) return null;
  for (const rawLine of stack.split("\n")) {
    const line = rawLine.trim();
    if (!line.startsWith("at ")) continue;
    const parenthesized = /\((.*?):(\d+):\d+\)\s*$/.exec(line);
    const bare = /^at\s+(.*?):(\d+):\d+\s*$/.exec(line);
    const match = parenthesized ?? bare;
    if (!match) continue;
    const path = sourcePath(match[1]);
    // Skip this module's own frames, but NOT profileFlags.test.ts — the test
    // file shares the prefix.
    if (!path || /\/src\/lib\/profileFlags\.(ts|tsx|js|mjs)$/.test(path)) continue;
    return `${path}:${match[2]}`;
  }
  return null;
}

function profileEnabledFromLocation(): boolean {
  if (typeof window === "undefined" || typeof window.location !== "object") return false;
  try {
    return new URLSearchParams(window.location.search).get("profile") === "1";
  } catch {
    return false;
  }
}

const enabled = profileEnabledFromLocation();

/**
 * Time a synchronous instrumented region. Disabled: zero profiling work —
 * just calls `fn` and returns its result.
 */
export function profileRegion<T>(label: string, fn: () => T): T {
  return ACTIVE ? ACTIVE.runRegion(label, fn) : fn();
}

/** Mark the scenario currently driving load (null = between scenarios). */
export function markProfileScenario(name: string | null): void {
  ACTIVE?.markScenario(name);
}

/** Report an out-of-band fact (terminal renderer, …) onto the same channel. */
export function reportProbe(kind: string, value: unknown): void {
  ACTIVE?.reportProbe(kind, value);
}

export const profilingEnabled: boolean = enabled;

class ActiveProfiler {
  readonly startedAt = typeof performance !== "undefined" ? performance.now() : 0;
  private longTasks: ProfileLongTask[] = [];
  private regions: ProfileRegionSample[] = [];
  private scenarios: ProfileScenarioInterval[] = [];
  private probes: ProfileProbe[] = [];
  private stackTops = new Map<string, string | null>();
  private observer: PerformanceObserver | null = null;

  constructor() {
    if (typeof PerformanceObserver === "function") {
      try {
        this.observer = new PerformanceObserver((list) => {
          for (const entry of list.getEntries()) {
            this.recordLongTask(entry as PerformanceEntry & {
              attribution?: Array<{ containerName?: string; containerSrc?: string }>;
            });
          }
        });
        this.observer.observe({ entryTypes: ["longtask"] });
      } catch {
        // Some engines reject "longtask" (or stub PerformanceObserver); the
        // report simply comes back with an empty long-task list.
        this.observer = null;
      }
    }
  }

  runRegion<T>(label: string, fn: () => T): T {
    const start = performance.now();
    // One Error per invocation, but its .stack is formatted only on the
    // first occurrence of this label and never again afterwards.
    const holder = this.stackTops.has(label) ? null : new Error("profileRegion");
    try {
      return fn();
    } finally {
      const end = performance.now();
      let stackTop = this.stackTops.get(label) ?? null;
      if (holder && !this.stackTops.has(label)) {
        stackTop = readStackTop(holder);
        this.stackTops.set(label, stackTop);
      }
      this.regions.push({ label, start, end, stackTop });
      // Defensive bound: a high-rate region (tty frames) must never let the
      // report grow unbounded. Drop oldest samples past the cap.
      if (this.regions.length > 6000) this.regions.splice(0, this.regions.length - 6000);
    }
  }

  markScenario(name: string | null): void {
    const now = performance.now();
    const open = this.scenarios.find((interval) => interval.end === null);
    if (open) open.end = now;
    if (name) this.scenarios.push({ name, start: now, end: null });
  }

  reportProbe(kind: string, value: unknown): void {
    this.probes.push({ kind, value, at: performance.now() });
  }

  reset(): void {
    this.longTasks = [];
    this.regions = [];
    this.scenarios = [];
    this.probes = [];
  }

  getReport(): ProfileReport {
    const current = [...this.scenarios].reverse().find((interval) => interval.end === null) ?? null;
    return {
      enabled: true,
      startedAt: this.startedAt,
      userAgent: typeof navigator !== "undefined" ? navigator.userAgent : "",
      scenario: current?.name ?? null,
      scenarios: this.scenarios.map((interval) => ({ ...interval })),
      longTasks: this.longTasks.map((task) => ({ ...task, region: task.region ? { ...task.region } : null })),
      regions: this.regions.map((region) => ({ ...region })),
      probes: this.probes.map((probe) => ({ ...probe })),
    };
  }

  private recordLongTask(
    entry: PerformanceEntry & {
      attribution?: Array<{ containerName?: string; containerSrc?: string }>;
    },
  ): void {
    const taskStart = entry.startTime;
    const taskEnd = entry.startTime + entry.duration;
    const scenario = this.scenarioOverlap(taskStart, taskEnd);
    // The region fully contained in the task with the largest duration.
    let best: ProfileRegionSample | null = null;
    for (const region of this.regions) {
      if (region.start + 0.5 < taskStart || region.end - 0.5 > taskEnd) continue;
      if (!best || region.end - region.start > best.end - best.start) best = region;
    }
    const attribution = entry.attribution?.[0];
    const container =
      sourcePath(attribution?.containerSrc) ||
      (attribution?.containerName && attribution.containerName !== "unknown"
        ? attribution.containerName
        : null);
    this.longTasks.push({
      startTime: taskStart,
      duration: entry.duration,
      scenario,
      region: best ? { label: best.label, stackTop: best.stackTop } : null,
      container,
    });
  }

  private scenarioOverlap(start: number, end: number): string | null {
    let match: ProfileScenarioInterval | null = null;
    for (const interval of this.scenarios) {
      const intervalEnd = interval.end ?? Number.POSITIVE_INFINITY;
      if (intervalEnd <= start || interval.start >= end) continue;
      // Prefer the scenario already open when the task started.
      if (interval.start <= start && (!match || match.start < interval.start)) match = interval;
    }
    return match?.name ?? null;
  }
}

const ACTIVE: ActiveProfiler | null = enabled ? new ActiveProfiler() : null;

if (ACTIVE) {
  const api: RemudaPerfApi = {
    enabled: true,
    markScenario: (name) => ACTIVE!.markScenario(name),
    reportProbe: (kind, value) => ACTIVE!.reportProbe(kind, value),
    getReport: () => ACTIVE!.getReport(),
    reset: () => ACTIVE!.reset(),
  };
  window.__remudaPerf = api;
}
