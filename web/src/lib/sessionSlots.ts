import type { Instance } from "../types/instance";
import { visibleTabs, type Space, type SpacePrefs } from "../features/spaces/store";

/**
 * The single ordered list behind ⌘/Ctrl+1–9 session switching.
 *
 * Both the Shell keydown handler (the action) and the SessionList badges (the
 * affordance) resolve digit slots through this one function, so a badge can
 * never promise a session the keypress does not open. The order is exactly
 * the Space tab strip's: Space instances sorted by createdAt then id, with
 * dismissed tabs removed (`visibleTabs`), capped at the nine digit slots.
 */
export const SWITCH_SLOT_COUNT = 9;

export function switchSlots(space: Space | undefined, prefs: SpacePrefs): Instance[] {
  if (!space) return [];
  return visibleTabs(space, prefs).slice(0, SWITCH_SLOT_COUNT);
}

/** 1-based digit slot of an instance in the shared ordering, or 0 if unnumbered. */
export function switchSlotOf(slots: Array<{ id: string }>, instanceId: string): number {
  const index = slots.findIndex((slot) => slot.id === instanceId);
  return index < 0 ? 0 : index + 1;
}
