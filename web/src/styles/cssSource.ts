/* ------------------------------------------------------------------ */
/* Read stylesheet sources in node-environment unit tests. Vitest     */
/* stubs CSS imports (even `?raw`) to "", and `src` is typed without  */
/* @types/node on purpose (browser timers), so fs is reached through  */
/* process.getBuiltinModule with just the shape these tests need.     */
/* ------------------------------------------------------------------ */

type Dirent = { name: string; parentPath: string; isFile(): boolean };
type Fs = {
  readFileSync(path: string, encoding: "utf8"): string;
  readdirSync(path: string, options: { withFileTypes: true; recursive: true }): Dirent[];
};
type Url = { fileURLToPath(url: URL | string): string };

const node = (globalThis as unknown as { process: { getBuiltinModule(id: string): unknown } }).process;
const fs = node.getBuiltinModule("node:fs") as Fs;
const url = node.getBuiltinModule("node:url") as Url;

/** Absolute path of `web/src`. */
export const SRC_DIR = url.fileURLToPath(new URL("..", import.meta.url)).replace(/\/+$/, "");

/** Source of a file under `web/src`, by src-relative path. */
export function readSrc(rel: string): string {
  return fs.readFileSync(`${SRC_DIR}/${rel}`, "utf8");
}

/** Every `.css` file under `web/src`, as src-relative paths. */
export function listCss(): string[] {
  return fs
    .readdirSync(SRC_DIR, { withFileTypes: true, recursive: true })
    .filter((e) => e.isFile() && e.name.endsWith(".css"))
    .map((e) => `${e.parentPath}/${e.name}`.slice(SRC_DIR.length + 1));
}
