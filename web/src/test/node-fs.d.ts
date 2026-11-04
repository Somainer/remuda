// The app program deliberately does not include @types/node (its globals
// would shadow the DOM timer types the app relies on). Tests that read the
// shared repo files (scripts/ci/…) need exactly one Node builtin and one
// module-runner field, so declare them here rather than widening every app
// file's types.
declare module "node:fs" {
  export function readFileSync(path: URL | string, encoding: "utf8"): string;
}

interface ImportMeta {
  /**
   * The module's own directory on disk. Vitest's module runner provides it
   * (import.meta.url there is an http://localhost/@fs/… URL, so a relative
   * URL cannot reach a file to read).
   */
  readonly dirname: string;
}
