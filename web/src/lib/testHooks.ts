/**
 * Compile-time, test-only switch for the hub e2e debug handle
 * (c-unboundresume r4 item 1). The Vite config `define`s
 * `__REMUDA_TEST_HOOKS__` as a literal boolean: `false` for the normal
 * production build (so the whole installation path is dead-code eliminated
 * and the `__remudaHub` token does not exist in web/dist) and `true` only for
 * the dedicated e2e artifact built with `vite build --mode e2e`
 * (web/dist-e2e). There is deliberately NO runtime/localStorage/URL
 * enablement — production pages cannot publish the handle by setting
 * anything.
 */
declare const __REMUDA_TEST_HOOKS__: boolean;

export const TEST_HOOKS_ENABLED: boolean = import.meta.env.DEV || __REMUDA_TEST_HOOKS__;
