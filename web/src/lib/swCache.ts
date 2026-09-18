/**
 * Derive the runtime service-worker cache name from a build identity.
 *
 * The build id is the git commit when the build env exposes one, else a hash
 * over the emitted asset file names (see vite.config.ts). Folding it into the
 * cache name is what makes the worker's bytes change on every deploy: the
 * browser's byte-compare update check then sees a new worker, and the activate
 * sweep deletes every cache whose name is not the current one, reclaiming the
 * stale shell that stranded installed browsers on a dead cached index.html.
 */
export function cacheNameForBuild(buildId: string): string {
  const clean = buildId.replace(/[^A-Za-z0-9._-]/g, "").slice(0, 40) || "dev";
  return `runtime-shell-${clean}`;
}
