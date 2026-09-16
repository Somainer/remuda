import type { TtyProgress } from "./client";
import css from "./TerminalView.module.css";

/**
 * Thin `OSC 9;4` progress bar for the terminal header (native-config, 2026-09-16).
 *
 * The Node's emulator parses ConEmu progress sequences the harness emits and
 * the `tty.mode` notice (the same channel that carries alt-screen) delivers
 * edges: indeterminate while a turn runs, a filled percent for determinate
 * work, error/paused tints, and an explicit `done`/null hides the bar.
 */
export function TtyProgressBar({ progress }: { progress: TtyProgress | null }) {
  if (!progress) return null;
  const percent =
    typeof progress.percent === "number"
      ? Math.min(100, Math.max(0, progress.percent))
      : undefined;
  const width = percent === undefined ? undefined : `${percent}%`;
  return (
    <div
      className={css.progressTrack}
      data-testid="tty-progress-bar"
      data-progress-state={progress.state}
      role="progressbar"
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={progress.state === "percent" ? percent : undefined}
      aria-valuetext={
        progress.state === "percent" && percent !== undefined
          ? `${percent}%`
          : progress.state === "error"
            ? "error"
            : progress.state === "paused"
              ? "paused"
              : "working"
      }
    >
      <div
        className={css.progressFill}
        data-progress-state={progress.state}
        style={width ? { width } : undefined}
      />
    </div>
  );
}
