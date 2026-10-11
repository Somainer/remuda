/**
 * Resolve where cargo actually wrote a just-built binary (c-ctxusage r10
 * item 6).
 *
 * The output is NOT always `<target dir>/debug/<name>`: with a default
 * target triple configured (`.cargo/config.toml build.target`, `--target`,
 * `CARGO_BUILD_TARGET`) cargo writes to
 * `<target dir>/<host-triple>/debug/<name>`. The e2e hook builds the binaries
 * itself, so a freshly built binary that is missing from the resolved path is
 * a real hook failure and must surface — never masquerade as the
 * fixture-less-host ENOENT skip.
 *
 * Only pure path selection is exported for unit testing; the metadata probes
 * (`cargo metadata`, `rustc -vV`) run lazily behind {@link resolveBuiltBinary}.
 */
import { execFileSync } from "node:child_process";
import { accessSync } from "node:fs";
import path from "node:path";

export interface BuiltBinary {
  /** Human label used in skip/failure messages. */
  label: string;
  /** Path within the profile dir, e.g. `remuda` or `examples/native_hub_e2e`. */
  relPath: string;
}

/** Binaries the native-usage e2e builds itself when no override is given. */
export const HUB_E2E_BUILT_BINARIES: BuiltBinary[] = [
  { label: "remuda", relPath: "remuda" },
  { label: "fake-harness", relPath: "fake-harness" },
  { label: "native node", relPath: path.join("examples", "native_hub_e2e") },
];

/** Where cargo puts its output, independent of the probed `$CARGO_TARGET_DIR`.
 * `null` means cargo/rustc could not be queried. */
export interface CargoLayout {
  targetDirectory: string;
  hostTriple: string;
}

/** Distinct profile directories a just-built artifact can land in, in probe
 * order. `targetDir` is `$CARGO_TARGET_DIR` (or `<root>/target`); cargo's own
 * `target_directory` from metadata is normally the same value but probed
 * independently, and the host-triple subdir is added for a configured default
 * target. */
export function artifactProfileDirs(
  targetDir: string,
  layout: CargoLayout | null,
): string[] {
  const dirs = [path.join(targetDir, "debug")];
  if (layout) {
    dirs.push(path.join(layout.targetDirectory, "debug"));
    dirs.push(path.join(layout.targetDirectory, layout.hostTriple, "debug"));
  }
  return [...new Set(dirs.map((dir) => path.resolve(dir)))];
}

/** Every path cargo might have written `relPath` to, in probe order. */
export function binaryCandidates(
  targetDir: string,
  layout: CargoLayout | null,
  relPath: string,
): string[] {
  return artifactProfileDirs(targetDir, layout).map((dir) => path.join(dir, relPath));
}

function exists(candidate: string): boolean {
  try {
    accessSync(candidate);
    return true;
  } catch {
    return false;
  }
}

/**
 * Resolve against an explicit cargo layout (the probe lives in
 * {@link resolveBuiltBinary}; tests inject a deterministic layout).
 * Throws when the artifact is at none of cargo's output paths: the build
 * claimed success, so a miss is a hook failure, never a skip.
 */
export function resolveBuiltBinaryWithLayout(
  targetDir: string,
  layout: CargoLayout | null,
  binary: BuiltBinary,
): string {
  const candidates = binaryCandidates(targetDir, layout, binary.relPath);
  const found = candidates.find(exists);
  if (found) {
    return found;
  }
  throw new Error(
    `freshly built ${binary.label} binary is missing from every cargo output path: ${candidates.join(
      ", ",
    )}`,
  );
}

let layoutCache: CargoLayout | null | undefined;

/** Ask cargo where its target directory is and what the host triple is. Both
 * are repo/host properties, so the probe runs at most once; `null` is cached
 * when cargo is unavailable. */
function cargoLayout(root: string): CargoLayout | null {
  if (layoutCache !== undefined) {
    return layoutCache;
  }
  try {
    const raw = execFileSync(
      "cargo",
      ["metadata", "--no-deps", "--format-version", "1"],
      { cwd: root, encoding: "utf8", maxBuffer: 64 * 1024 * 1024 },
    );
    const targetDirectory = (JSON.parse(raw) as { target_directory?: string }).target_directory;
    const versionVerbose = execFileSync("rustc", ["-vV"], { encoding: "utf8" });
    const hostTriple = /^host:\s*(\S+)$/m.exec(versionVerbose)?.[1];
    layoutCache = targetDirectory && hostTriple ? { targetDirectory, hostTriple } : null;
  } catch {
    layoutCache = null;
  }
  return layoutCache;
}

/**
 * Resolve a binary the hook JUST built, probing cargo's real output layout
 * (including a configured target-triple subdirectory). Throws when the
 * artifact is at none of the output paths: the build claimed success, so a
 * miss here is a hook failure and must never be downgraded to a skip.
 */
export function resolveBuiltBinary(root: string, targetDir: string, binary: BuiltBinary): string {
  return resolveBuiltBinaryWithLayout(targetDir, cargoLayout(root), binary);
}
