/**
 * Structural equality for plain API data (JSON.parse output and the mapped
 * view objects built from it): records, arrays, primitives. Used to preserve
 * OBJECT IDENTITY across polling when a refresh returns equal content, so
 * `useSyncExternalStore` consumers don't re-render on unchanged snapshots.
 *
 * Deliberately NOT a general-purpose deep-equal: no Map/Set/Date/typed-array
 * support (the store never holds those in state), and reference-identical
 * subtrees short-circuit immediately — the common poll case where most rows
 * have already been identity-stabilized.
 */
export function structuralEqual(a: unknown, b: unknown): boolean {
  if (Object.is(a, b)) return true;
  if (typeof a !== "object" || a === null || typeof b !== "object" || b === null) {
    return false;
  }
  if (Array.isArray(a)) {
    if (!Array.isArray(b) || a.length !== b.length) return false;
    for (let i = 0; i < a.length; i += 1) {
      if (!structuralEqual(a[i], b[i])) return false;
    }
    return true;
  }
  if (Array.isArray(b)) return false;
  // Plain records only (JSON data / mapped views). Different prototypes mean
  // different semantics; never compare across them.
  const ra = a as Record<string, unknown>;
  const rb = b as Record<string, unknown>;
  if (Object.getPrototypeOf(ra) !== Object.getPrototypeOf(rb)) return false;
  const keysA = Object.keys(ra);
  const keysB = Object.keys(rb);
  if (keysA.length !== keysB.length) return false;
  for (const key of keysA) {
    if (!Object.prototype.hasOwnProperty.call(rb, key) || !structuralEqual(ra[key], rb[key])) {
      return false;
    }
  }
  return true;
}
