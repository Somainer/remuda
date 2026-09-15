import { useEffect, useState } from "react";
import { isTypingFocusActive } from "./keyboardScope";
import { platformInfo } from "./platform";

/**
 * Tracks whether the platform-primary modifier (⌘ on macOS, Ctrl on
 * Windows/Linux) is currently held — the hold-to-reveal gesture behind the
 * session-switch badges.
 *
 * Robustness rules:
 * - `window` blur clears the gesture. Releasing ⌘ in another window (or over
 *   browser chrome) never delivers keyup to the page; without this the badges
 *   would stay revealed forever.
 * - Key repeat is a no-op: auto-repeated modifier keydowns only re-confirm the
 *   already-held state, never toggle it.
 * - IME composition is ignored (`isComposing` / key "Process" / keyCode 229)
 *   so an input method mid-composition cannot reveal the badges.
 * - Focus inside input/textarea/select/contenteditable/.xterm hides the
 *   gesture even while the modifier is physically down, via
 *   {@link isTypingFocusActive}; the handler that acts on digits applies the
 *   same guard to the event target.
 * - Touch platforms (iOS/iPadOS/Android) report no held-modifier flow and the
 *   hook stays false forever there.
 */
export function useModifierHeld(enabled = true): boolean {
  const platform = platformInfo();
  const supported = enabled && platform.heldModifiersSupported;
  const [held, setHeld] = useState(false);

  useEffect(() => {
    if (!supported) return;
    // The physical state lives in a ref-ish local; the React state is the
    // derived "held and not typing". focus changes recompute without needing
    // to know whether the key is still down.
    let down = false;
    const sync = () => setHeld(down && !isTypingFocusActive());

    const onKeyDown = (event: KeyboardEvent) => {
      if (event.isComposing || event.key === "Process" || event.keyCode === 229) return;
      if (event.key === platform.modifierKey) {
        down = true;
        sync();
      }
    };
    const onKeyUp = (event: KeyboardEvent) => {
      // Clear on either modifier: a chord (⌃⌘) releasing one must settle
      // cleanly, and some browsers report the up generically.
      if (event.key === platform.modifierKey || event.key === "Meta" || event.key === "Control") {
        down = false;
        sync();
      }
    };
    const clear = () => {
      down = false;
      setHeld(false);
    };
    // Clicking into/out of the composer or terminal while the modifier stays
    // held must hide or re-reveal without another keydown. focus/blur do not
    // bubble, so listen in the capture phase.
    const onFocusChange = () => sync();

    window.addEventListener("keydown", onKeyDown);
    window.addEventListener("keyup", onKeyUp);
    window.addEventListener("blur", clear);
    document.addEventListener("focus", onFocusChange, true);
    document.addEventListener("blur", onFocusChange, true);
    return () => {
      window.removeEventListener("keydown", onKeyDown);
      window.removeEventListener("keyup", onKeyUp);
      window.removeEventListener("blur", clear);
      document.removeEventListener("focus", onFocusChange, true);
      document.removeEventListener("blur", onFocusChange, true);
    };
  }, [supported, platform.modifierKey]);

  return supported && held;
}
