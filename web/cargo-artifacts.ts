/**
 * Resolve where cargo actually wrote a just-built binary (c-ctxusage r10
 * item 6; hardened in r11 item 2).
 *
 * The output is NOT always `<target dir>/debug/<name>`: with a default target
 * triple configured (`.cargo/config.toml build.target`, `--target`,
 * `CARGO_BUILD_TARGET`) cargo writes to
 * `<target dir>/<selected-triple>/debug/<name>`, and the selected triple is
 * not necessarily the HOST triple. Probing conventional locations therefore
 * (a) lets a stale binary in `target/debug` mask a missing fresh output under
 * the triple dir and (b) guesses the host triple instead of cargo's selected
 * target. The e2e hook builds with `--message-format=json`; cargo then reports
 * each `compiler-artifact.executable` itself, so resolution takes the
 * executable paths from THAT bounded build and requires exactly those files
 * to exist. No `cargo metadata` / `rustc -vV` probes run after the build, so
 * nothing can block the Playwright worker beyond its own bounded build.
 *
 * Only pure parsing/path selection is exported for unit testing; the build
 * itself is run by the e2e hook and its stdout handed to
 * {@link parseBuiltExecutables}.
 */
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

/** Cargo's reported executable for each built binary, keyed by
 * {@link BuiltBinary.relPath}. `relPath` is the target name for a bin and
 * `examples/<name>` for an example target. */
export type BuiltExecutables = Map<string, string>;

/**
 * Parse cargo's `--message-format=json` stdout into the executable cargo
 * itself wrote for each bin/example target it built as an artifact.
 *
 * Only `compiler-artifact` messages with a non-null `executable` and a `bin`
 * or `example` target kind contribute; examples are keyed under
 * `examples/<name>` to match {@link BuiltBinary.relPath}. Malformed lines and
 * unrelated messages (libraries, build scripts, compiler diagnostics) are
 * ignored. Duplicate emissions (the same artifact reported from more than
 * one package graph) must agree; a disagreement is a cargo error and throws
 * rather than silently picking one.
 */
export function parseBuiltExecutables(jsonStream: string): BuiltExecutables {
  const executables = new Map<string, string>();
  for (const line of jsonStream.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed) {
      continue;
    }
    let message: {
      reason?: string;
      target?: { name?: string; kind?: unknown };
      executable?: unknown;
    };
    try {
      message = JSON.parse(trimmed) as typeof message;
    } catch {
      continue;
    }
    if (message.reason !== "compiler-artifact" || typeof message.executable !== "string") {
      continue;
    }
    const name = message.target?.name;
    const kinds = Array.isArray(message.target?.kind) ? message.target!.kind : [];
    const relPath = kinds.includes("bin")
      ? name
      : kinds.includes("example")
        ? path.join("examples", name ?? "")
        : undefined;
    if (!name || !relPath) {
      continue;
    }
    const previous = executables.get(relPath);
    if (previous !== undefined && previous !== message.executable) {
      throw new Error(
        `cargo reported two different executable paths for ${relPath}: ${previous} vs ${message.executable}`,
      );
    }
    executables.set(relPath, message.executable);
  }
  return executables;
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
 * Resolve a binary the hook JUST built against cargo's own reported artifact
 * paths. Throws when cargo never reported the target, or when the exact path
 * cargo reported does not exist on disk: the build claimed success, so a miss
 * is a hook failure and must never be downgraded to a skip or masked by a
 * stale binary in another directory.
 */
export function resolveBuiltBinary(
  built: BuiltExecutables,
  binary: BuiltBinary,
): string {
  const reported = built.get(binary.relPath);
  if (reported === undefined) {
    throw new Error(
      `freshly built ${binary.label} binary was not reported by cargo in the --message-format=json build output`,
    );
  }
  if (!exists(reported)) {
    throw new Error(
      `freshly built ${binary.label} binary is missing from cargo's reported path: ${reported}`,
    );
  }
  return reported;
}
