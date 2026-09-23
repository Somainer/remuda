import { Profiler, type ProfilerOnRenderCallback, type ReactNode } from "react";
import { profilingEnabled, reportProbe } from "../lib/profileFlags";

/**
 * Report React commit durations of a shared base control onto the perf
 * channel as `commit:<name>`. Only wraps when `?profile=1` is on; otherwise it
 * renders the children with no Profiler in the tree.
 */
const onRender: ProfilerOnRenderCallback = (id, phase, actualDuration) => {
  reportProbe(`commit:${id}`, { phase, actualDuration });
};

export function CommitProbe({ name, children }: { name: string; children: ReactNode }) {
  if (!profilingEnabled) return children;
  return (
    <Profiler id={name} onRender={onRender}>
      {children}
    </Profiler>
  );
}
