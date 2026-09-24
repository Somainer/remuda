import { useLayoutEffect } from "react";
import { useLocation, useNavigate } from "react-router-dom";
import { isTypingTarget } from "../lib/keyboardScope";
import { switchSlots, tabSlots } from "../lib/sessionSlots";
import { spaceStore } from "../features/spaces/store";
import type { useSpaceWorkbench } from "../features/spaces/useSpaceWorkbench";

type Workbench = ReturnType<typeof useSpaceWorkbench>;

/**
 * Desktop workbench chords, moved verbatim out of Shell. ⌘/Ctrl+B folds the
 * sidebar (`prefs.collapsed`) on every desktop route; ⌘[ / ⌘] walk Spaces.
 *
 * ⌘/Ctrl+1..9 always numbers what is on screen (D-053 §10): on /s/* it is the
 * tab strip (`tabSlots()` — the bound Task's tabs or the Space tabs), on
 * /sessions it is the list badges (`switchSlots()`, one shared ordering).
 */
export function useWorkbenchKeys({
  mobile,
  onSessions,
  onNew,
  workbench,
}: {
  mobile: boolean;
  onSessions: boolean;
  onNew: boolean;
  workbench: Workbench;
}): void {
  const navigate = useNavigate();
  const location = useLocation();
  useLayoutEffect(() => {
    if (mobile) return;
    const switching = onSessions && !onNew;
    // The strip belongs to /s/* only; everywhere session-scoped digits are the
    // /sessions list badges.
    const onTabRoute = location.pathname.startsWith("/s/");
    const onKey = (event: KeyboardEvent) => {
      if (!(event.metaKey || event.ctrlKey) || event.altKey || event.isComposing || event.repeat) return;
      // Never steal a chord from the composer, a form field or an attached
      // terminal. QuickFind's ⌘K shares this guard, so a digit can never
      // switch tabs while the finder is open and owns the keystroke.
      if (isTypingTarget(event.target)) return;
      if (event.key.toLowerCase() === "b") {
        event.preventDefault();
        spaceStore.setCollapsed(!workbench.prefs.collapsed);
      } else if (!switching) {
        return;
      } else if (/^[1-9]$/.test(event.key)) {
        const digit = Number(event.key) - 1;
        if (onTabRoute) {
          // Exactly the strip's rendered order (task set or Space tabs).
          const tab = tabSlots(workbench.tabs)[digit];
          if (!tab) return;
          event.preventDefault();
          // A task tab's selection/dismissal is recorded on its owning Space.
          spaceStore.selectTab(workbench.ownerOf(tab), tab.id);
          navigate(`/s/${tab.id}`);
        } else {
          if (!workbench.active) return;
          // Same ordered list SessionList numbers its badges from: the visible
          // tabs of the active Space, dismissal-filtered, capped at nine.
          const tab = switchSlots(workbench.active, workbench.prefs)[digit];
          if (!tab) return;
          event.preventDefault();
          spaceStore.selectTab(workbench.active.id, tab.id);
          navigate(`/s/${tab.id}`);
        }
      } else if (event.code === "BracketLeft" || event.code === "BracketRight") {
        if (!workbench.spaces.length) return;
        event.preventDefault();
        const index = workbench.spaces.findIndex((s) => s.id === workbench.active?.id);
        const step = event.code === "BracketLeft" ? -1 : 1;
        workbench.select(workbench.spaces[(index + step + workbench.spaces.length) % workbench.spaces.length]);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [mobile, onSessions, onNew, workbench, navigate, location.pathname]);
}
