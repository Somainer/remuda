import type { Instance } from "../types/instance";

/**
 * home.md §5.3: true for a session an Agent DISPATCHED, the only kind of row
 * the board hides as a child, the ① rank tap reveals, and that wakes its
 * parent (§3.1 f).
 *
 * `parent` alone does not say that. A D-026 resume writes `parentInstanceId`
 * as the resumed session, and a continuation inherits its chapter's parent.
 * The Hub's creator stamp belongs to the chain ROOT, because every resume or
 * continuation inherits its predecessor's, so `agent` means "dispatched"
 * exactly. A row from a Hub that predates the stamp falls back to "has a
 * parent and is not a resume".
 */
export function dispatchedChild(
  instance: Pick<Instance, "parent" | "creatorOrigin" | "resumedFrom">,
): boolean {
  if (instance.parent == null) return false;
  if (instance.creatorOrigin) return instance.creatorOrigin === "agent";
  return instance.resumedFrom == null;
}
