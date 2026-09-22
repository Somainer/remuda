/**
 * c-perfaudit: report the terminal renderer that ACTUALLY took effect
 * (webgl / canvas / dom) onto the shared profiler channel.
 *
 * `attachTerminalRenderer` picks the first addon that loads, but a WebGL
 * context can be lost later (GPU reset, driver fallback); the addon disposes
 * itself on context loss and xterm keeps painting with whatever is left, which
 * can silently degrade a session from GPU to DOM rendering. TerminalView
 * calls `probeRendererSelection` once when the addon settles and
 * `probeWebglContextLoss` from the addon's loss callback, so the perf report
 * shows both the initial renderer and every subsequent degradation.
 *
 * When profiling is off (`?profile` absent) every function here is a no-op
 * return — no state, no observers.
 */

import { reportProbe, profilingEnabled } from "../../../lib/profileFlags";
import type { TerminalRenderer } from "./renderer";

/** Record the renderer the terminal settled on at attach time. */
export function probeRendererSelection(renderer: TerminalRenderer): void {
  if (!profilingEnabled) return;
  reportProbe("terminal-renderer", renderer);
}

/**
 * Record a WebGL context loss (the hidden-degradation signal). Returns a
 * cleanup hook the caller can wire — though context loss is terminal for the
 * addon, keeping the function signature uniform with other probes.
 */
export function probeWebglContextLoss(): void {
  if (!profilingEnabled) return;
  reportProbe("terminal-renderer-context-loss", "webgl");
}
