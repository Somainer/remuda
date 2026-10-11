import { mkdtempSync, mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { describe, expect, it } from "vitest";

import {
  HUB_E2E_BUILT_BINARIES,
  parseBuiltExecutables,
  resolveBuiltBinary,
} from "./cargo-artifacts.ts";

/** A cargo `compiler-artifact` line, with only the fields the parser reads. */
function artifactLine(name: string, kinds: string[], executable: string | null): string {
  return JSON.stringify({
    reason: "compiler-artifact",
    target: { name, kind: kinds },
    executable,
  });
}

describe("parseBuiltExecutables", () => {
  it("maps bins by name and examples by examples/<name> from cargo's json stream", () => {
    const stream = [
      JSON.stringify({ reason: "compiler-message", message: { level: "warning" } }),
      artifactLine("remuda_lib", ["lib"], null),
      artifactLine("remuda", ["bin"], "/t/debug/remuda"),
      artifactLine("fake-harness", ["bin"], "/t/debug/fake-harness"),
      artifactLine(
        "native_hub_e2e",
        ["example"],
        "/t/debug/examples/native_hub_e2e",
      ),
      "",
    ].join("\n");
    const artifacts = parseBuiltExecutables(stream);
    expect(artifacts.get("remuda")).toBe("/t/debug/remuda");
    expect(artifacts.get("fake-harness")).toBe("/t/debug/fake-harness");
    expect(artifacts.get(path.join("examples", "native_hub_e2e"))).toBe(
      "/t/debug/examples/native_hub_e2e",
    );
    expect(artifacts.size).toBe(3);
  });

  it("takes cargo's selected triple subdirectory verbatim, with no host guess", () => {
    const stream = [
      artifactLine(
        "remuda",
        ["bin"],
        "/t/aarch64-unknown-linux-gnu/debug/remuda",
      ),
    ].join("\n");
    const artifacts = parseBuiltExecutables(stream);
    expect(artifacts.get("remuda")).toBe("/t/aarch64-unknown-linux-gnu/debug/remuda");
  });

  it("ignores malformed lines and non-bin artifacts without executable paths", () => {
    const stream = [
      "not json",
      artifactLine("a_cdylib", ["cdylib"], null),
      JSON.stringify({ reason: "build-script-executable" }),
    ].join("\n");
    expect(parseBuiltExecutables(stream).size).toBe(0);
  });
});

describe("resolveBuiltBinary", () => {
  it("uses cargo's reported fresh target output even when a stale default-dir binary exists", () => {
    const root = mkdtempSync(path.join(tmpdir(), "cargo-bin-"));
    const target = path.join(root, "target");
    // A STALE binary left in the conventional dir by an earlier host build.
    const stale = path.join(target, "debug", "remuda");
    mkdirSync(path.dirname(stale), { recursive: true });
    writeFileSync(stale, "stale");
    // The fresh build emitted under the configured target triple instead.
    const fresh = path.join(
      target,
      "aarch64-unknown-linux-gnu",
      "debug",
      "remuda",
    );
    mkdirSync(path.dirname(fresh), { recursive: true });
    writeFileSync(fresh, "fresh");
    const artifacts = parseBuiltExecutables(
      artifactLine("remuda", ["bin"], fresh),
    );
    expect(resolveBuiltBinary(artifacts, { label: "remuda", relPath: "remuda" })).toBe(
      fresh,
    );
  });

  it("resolves a selected target different from the host", () => {
    const root = mkdtempSync(path.join(tmpdir(), "cargo-bin-"));
    const reported = path.join(root, "target", "wasm32-wasip1", "debug", "fake-harness");
    mkdirSync(path.dirname(reported), { recursive: true });
    writeFileSync(reported, "");
    const artifacts = parseBuiltExecutables(
      artifactLine("fake-harness", ["bin"], reported),
    );
    expect(
      resolveBuiltBinary(artifacts, {
        label: "fake-harness",
        relPath: "fake-harness",
      }),
    ).toBe(reported);
  });

  it("throws when cargo reported the fresh output but the file is missing", () => {
    const root = mkdtempSync(path.join(tmpdir(), "cargo-bin-"));
    const reported = path.join(root, "target", "triple-x", "debug", "remuda");
    const artifacts = parseBuiltExecutables(
      artifactLine("remuda", ["bin"], reported),
    );
    expect(() =>
      resolveBuiltBinary(artifacts, { label: "remuda", relPath: "remuda" }),
    ).toThrow(/freshly built remuda binary is missing from cargo's reported path/);
  });

  it("throws when cargo never reported the artifact in the build stream", () => {
    const artifacts = parseBuiltExecutables(
      artifactLine("remuda", ["bin"], "/t/debug/remuda"),
    );
    expect(() =>
      resolveBuiltBinary(artifacts, {
        label: "native node",
        relPath: path.join("examples", "native_hub_e2e"),
      }),
    ).toThrow(/freshly built native node binary was not reported by cargo/);
  });

  it("covers the three built binaries the e2e hook produces", () => {
    expect(HUB_E2E_BUILT_BINARIES.map((b) => b.relPath)).toEqual([
      "remuda",
      "fake-harness",
      path.join("examples", "native_hub_e2e"),
    ]);
  });
});
