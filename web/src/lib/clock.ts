/**
 * Injectable wall clock for the home freshness rules (home.md §8 新鲜度).
 *
 * The store stamps every refresh outcome with {@link nowMs} and
 * `useHomeFreshness()` evaluates the 10 s stale and 5 s banner thresholds
 * against the same clock. Tests install a deterministic clock and advance it
 * from fake timers, so the product constants are asserted without sleeping on
 * wall-clock milliseconds.
 */
let clock: () => number = () => Date.now();

/** Current clock time in milliseconds. */
export function nowMs(): number {
  return clock();
}

/**
 * Install a test clock; pass null to restore the wall clock. Returns the
 * restore function so a test can put the wall clock back in afterEach.
 */
export function setClock(next: (() => number) | null): () => void {
  const previous = clock;
  clock = next ?? (() => Date.now());
  return () => {
    clock = previous;
  };
}
