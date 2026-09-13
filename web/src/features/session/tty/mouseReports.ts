/**
 * Mouse-report plumbing for the embedded xterm.
 *
 * Two problems this solves:
 *
 * 1. Sticky DECSET tracking. An app that enables `?1000h`/`?1006h` and dies
 *    without the matching DECRST leaves the emulator in tracking mode forever
 *    — and the attach snapshot replays the `?1000h`, so a page reload does not
 *    clear it. While tracking is on, xterm cancels local wheel scrolling and
 *    ships every wheel tick to the PTY, which lands as junk on a shell prompt.
 * 2. `keys` (local-input) mode used to set `disableStdin`, which gates mouse
 *    reports in CoreService alongside keystrokes — so a narrow viewport lost
 *    clicks and wheel reports entirely.
 *
 * Both are handled by filtering at the `onData` boundary instead of through
 * `disableStdin`, so the keyboard and the mouse can be gated independently.
 */

/** DECRST for every mouse-tracking mode xterm can be left in, plus SGR encoding. */
export const MOUSE_TRACKING_RESET = "[?1000l[?1002l[?1003l[?1006l";

/**
 * X10 (`CSI M b x y`) and SGR (`CSI < b ; x ; y M|m`) mouse reports, plus the
 * urxvt (`CSI b ; x ; y M`) encoding. These are the only payloads xterm emits
 * from pointer input; everything else on `onData` came from the keyboard.
 */
const MOUSE_REPORT = /^\[(?:M[\s\S]{3}|<\d+;\d+;\d+[Mm]|\d+;\d+;\d+M)$/;

export function isMouseReport(data: string): boolean {
  return MOUSE_REPORT.test(data);
}

export type InputGate = {
  /** Keyboard/paste bytes may reach the PTY. */
  keyboard: boolean;
  /** Pointer reports may reach the PTY. */
  mouse: boolean;
};

/**
 * Decide whether one `onData` payload is allowed through.
 * Mouse reports and keystrokes are gated separately so `keys` mode can keep
 * the pointer alive while the dock owns the keyboard.
 */
export function allowInput(data: string, gate: InputGate): boolean {
  return isMouseReport(data) ? gate.mouse : gate.keyboard;
}

export function inputGate({
  directInput,
  frozen,
  mouseReports,
}: {
  directInput: boolean;
  frozen: boolean;
  mouseReports: boolean;
}): InputGate {
  if (frozen) return { keyboard: false, mouse: false };
  return { keyboard: directInput, mouse: mouseReports };
}

/** The remote app is asking for pointer reports. */
export function trackingActive(mouseMode: string): boolean {
  return mouseMode !== "none";
}

/**
 * Local wheel scrolling is ours to do whenever xterm would not scroll: the app
 * has tracking on (so xterm cancels the wheel and reports instead) but the
 * user has switched reporting off.
 *
 * Except in the alternate screen. A full-screen TUI (`?1049h`) has no
 * scrollback worth showing — the rows above the viewport belong to the shell
 * the TUI will hand the terminal back to — so scrolling locally drags the user
 * away from the only frame that matters and out of sync with an application
 * that is still repainting in place. Whatever the TUI does with the wheel is
 * the right behaviour, including nothing.
 *
 * `altScreen` comes from the Node's attach report (D-028 §4.6), not from
 * watching for `?1049h` in the byte stream: an attach mid-session never sees
 * the DECSET that put the terminal there, and the repaint snapshot deliberately
 * replays only the current state. `undefined` means the Node did not report —
 * an older Node, or a carrier that cannot know — and the pre-D-028 behaviour is
 * kept rather than guessed at either way.
 */
export function localWheelWanted({
  mouseMode,
  mouseReports,
  altScreen,
}: {
  mouseMode: string;
  mouseReports: boolean;
  altScreen?: boolean;
}): boolean {
  if (altScreen) return false;
  return trackingActive(mouseMode) && !mouseReports;
}
