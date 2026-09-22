import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
/**
 * c-perfaudit: the terminal renderer probe only reports onto the profiler
 * channel when profiling is enabled (?profile=1); otherwise every call is a
 * silent no-op.
 */

function setSearch(search: string) {
  window.history.pushState({}, "", `/${search}`);
}

async function importProbe() {
  return import("./rendererProbe");
}

describe("rendererProbe (default off)", () => {
  beforeEach(() => {
    setSearch("");
    const ctor = vi.fn();
    vi.stubGlobal("PerformanceObserver", ctor);
  });

  afterEach(() => {
    vi.resetModules();
    vi.unstubAllGlobals();
    delete (window as unknown as { __remudaPerf?: unknown }).__remudaPerf;
    setSearch("");
  });

  it("reports nothing and exposes no profiler API", async () => {
    const mod = await importProbe();
    expect(() => mod.probeRendererSelection("webgl")).not.toThrow();
    expect(() => mod.probeWebglContextLoss()).not.toThrow();
    expect(window.__remudaPerf).toBeUndefined();
  });
});

describe("rendererProbe (?profile=1)", () => {
  beforeEach(() => {
    setSearch("?profile=1");
    // Real PerformanceObserver is constructable; use a function stub.
    vi.stubGlobal(
      "PerformanceObserver",
      vi.fn(function () {
        return { observe: vi.fn(), disconnect: vi.fn() };
      }),
    );
  });

  afterEach(() => {
    vi.resetModules();
    vi.unstubAllGlobals();
    delete (window as unknown as { __remudaPerf?: unknown }).__remudaPerf;
    setSearch("");
  });

  it("reports the selected renderer and WebGL context loss on the shared channel", async () => {
    const mod = await importProbe();
    mod.probeRendererSelection("canvas");
    mod.probeRendererSelection("webgl");
    mod.probeWebglContextLoss();

    const report = window.__remudaPerf!.getReport();
    expect(report.probes.map((probe) => probe.kind)).toEqual([
      "terminal-renderer",
      "terminal-renderer",
      "terminal-renderer-context-loss",
    ]);
    expect(report.probes[1]!.value).toBe("webgl");
    expect(report.probes[2]!.value).toBe("webgl");
  });
});
