import { mkdtempSync, mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { describe, expect, it } from "vitest";

import {
  HUB_E2E_BUILT_BINARIES,
  artifactProfileDirs,
  binaryCandidates,
  resolveBuiltBinaryWithLayout,
} from "./cargo-artifacts.ts";

describe("artifactProfileDirs", () => {
  it("probes the conventional dir, cargo metadata dir and the target-triple dir", () => {
    const dirs = artifactProfileDirs("/work/target", {
      targetDirectory: "/work/target",
      hostTriple: "x86_64-unknown-linux-gnu",
    });
    expect(dirs).toEqual([
      "/work/target/debug",
      "/work/target/x86_64-unknown-linux-gnu/debug",
    ]);
  });

  it("falls back to the conventional dir when cargo cannot be probed", () => {
    expect(artifactProfileDirs("/work/target", null)).toEqual(["/work/target/debug"]);
  });

  it("includes a distinct CARGO_TARGET_DIR and metadata target without duplicates", () => {
    const dirs = artifactProfileDirs("/per-job/cargo-target", {
      targetDirectory: "/repo/target",
      hostTriple: "aarch64-apple-darwin",
    });
    expect(dirs).toEqual([
      "/per-job/cargo-target/debug",
      "/repo/target/debug",
      "/repo/target/aarch64-apple-darwin/debug",
    ]);
    expect(new Set(dirs).size).toBe(dirs.length);
  });
});

describe("binaryCandidates", () => {
  it("places example binaries under examples/ in every candidate dir", () => {
    const candidates = binaryCandidates(
      "/t",
      { targetDirectory: "/t", hostTriple: "x86_64-unknown-linux-gnu" },
      path.join("examples", "native_hub_e2e"),
    );
    expect(candidates).toEqual([
      "/t/debug/examples/native_hub_e2e",
      "/t/x86_64-unknown-linux-gnu/debug/examples/native_hub_e2e",
    ]);
  });

  it("covers the three built binaries the e2e hook produces", () => {
    expect(HUB_E2E_BUILT_BINARIES.map((b) => b.relPath)).toEqual([
      "remuda",
      "fake-harness",
      path.join("examples", "native_hub_e2e"),
    ]);
  });
});

describe("resolveBuiltBinaryWithLayout", () => {
  it("returns the conventional path when it exists", () => {
    const root = mkdtempSync(path.join(tmpdir(), "cargo-bin-"));
    const target = path.join(root, "target");
    const bin = path.join(target, "debug", "remuda");
    mkdirSync(path.dirname(bin), { recursive: true });
    writeFileSync(bin, "");
    expect(
      resolveBuiltBinaryWithLayout(target, null, { label: "remuda", relPath: "remuda" }),
    ).toBe(path.resolve(bin));
  });

  it("finds an artifact only present in the target-triple subdir", () => {
    const root = mkdtempSync(path.join(tmpdir(), "cargo-bin-"));
    const target = path.join(root, "target");
    const triple = "x86_64-unknown-linux-gnu";
    const bin = path.join(target, triple, "debug", "fake-harness");
    mkdirSync(path.dirname(bin), { recursive: true });
    writeFileSync(bin, "");
    expect(
      resolveBuiltBinaryWithLayout(
        target,
        { targetDirectory: target, hostTriple: triple },
        { label: "fake-harness", relPath: "fake-harness" },
      ),
    ).toBe(path.resolve(bin));
  });

  it("throws when a freshly built artifact is at no output path", () => {
    const root = mkdtempSync(path.join(tmpdir(), "cargo-bin-"));
    const target = path.join(root, "target");
    expect(() =>
      resolveBuiltBinaryWithLayout(
        target,
        { targetDirectory: target, hostTriple: "x86_64-unknown-linux-gnu" },
        {
          label: "native node",
          relPath: path.join("examples", "native_hub_e2e"),
        },
      ),
    ).toThrow(/freshly built native node binary is missing/);
  });
});
