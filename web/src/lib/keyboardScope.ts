/**
 * Where an app-level keyboard shortcut must keep its hands off.
 *
 * A shortcut that fires while the user is typing steals the keystroke from the
 * text they are writing; one that fires over an attached terminal steals it
 * from the native process, which owns Escape, Tab and every control byte.
 *
 * This is the guard currently inlined at `app/Shell.tsx:54`. It lives here so
 * the filter panel, the focus trap and the cross-space finder all test the same
 * selector instead of each re-deriving it.
 *
 * TODO(batch C, owns Shell.tsx): replace the inline
 * `target.closest('input, textarea, select, [contenteditable="true"], .xterm')`
 * in `Shell.tsx` with `isTypingTarget(event.target)`.
 */
export const TYPING_SELECTOR = 'input, textarea, select, [contenteditable="true"], .xterm';

/** The attached-terminal subset: xterm owns its own key handling outright. */
export const TERMINAL_SELECTOR = ".xterm";

/**
 * True when the event target is a text-entry surface or an attached terminal,
 * i.e. when an app-level shortcut must not act on the keystroke.
 */
export function isTypingTarget(target: EventTarget | null): boolean {
  return target instanceof Element && target.closest(TYPING_SELECTOR) != null;
}

/**
 * True when the event target sits inside an attached terminal.
 *
 * Overlays use this rather than {@link isTypingTarget}: a focus trap still has
 * to cycle Tab and close on Escape when the user is in an ordinary text input
 * inside the panel, but it must stay entirely out of the way of a terminal,
 * which needs those same keys delivered as bytes to the native process.
 */
export function isTerminalTarget(target: EventTarget | null): boolean {
  return target instanceof Element && target.closest(TERMINAL_SELECTOR) != null;
}

/**
 * True when *focus itself* currently sits in a text-entry surface or terminal.
 *
 * Event-target guards only cover the key being pressed; the hold-to-reveal
 * modifier hook also has to hide its badges the moment the user clicks into
 * the composer while the modifier is still physically down.
 */
export function isTypingFocusActive(doc: Document = document): boolean {
  return isTypingTarget(doc.activeElement);
}
