import { describe, expect, it } from "vitest";
import { mapDriver } from "./api";
import { DRIVER_LABELS } from "./driverMatrix";
import type { DriverKind } from "../types/nativeRef";

/**
 * D-037: `claude-sdk` is a distinct carrier, not a flavour of print.
 *
 * The allowlist in `api.ts` coerces anything it does not recognise to
 * `claude-print`. Before this, that swallowed `claude-sdk` — so an instance
 * whose child is alive across turns was labelled with the carrier that ends
 * after one turn and needs a manual resume. That is a wrong statement about the
 * session, not a cosmetic gap.
 */
describe("claude-sdk is never rendered as claude-print", () => {
  it("keeps a reported claude-sdk driver", () => {
    expect(mapDriver("claude-sdk")).toBe("claude-sdk");
    expect(mapDriver("claude-sdk")).not.toBe("claude-print");
  });

  it("still maps the other known drivers to themselves", () => {
    for (const kind of [
      "claude-print",
      "claude-pty",
      "claude-bg",
      "codex-appserver",
      "grok-acp",
      "agy-print",
      "generic-pty",
      "shell-pty",
    ] satisfies DriverKind[]) {
      expect(mapDriver(kind)).toBe(kind);
    }
  });

  it("only coerces a genuinely unknown driver", () => {
    expect(mapDriver("some-future-carrier")).toBe("claude-print");
    expect(mapDriver("")).toBe("claude-print");
  });

  it("gives claude-sdk its own label, marked experimental and distinct from print", () => {
    const sdk = DRIVER_LABELS["claude-sdk"];
    expect(sdk).toBeTruthy();
    expect(sdk).not.toBe(DRIVER_LABELS["claude-print"]);
    expect(sdk).toContain("claude-sdk");
    expect(sdk).toContain("实验性");
    // print's label promises a single turn; sdk's must not inherit that.
    expect(DRIVER_LABELS["claude-print"]).toContain("单轮");
    expect(sdk).not.toContain("单轮");
  });

  it("is never offered as a default or in the legacy fallback list", async () => {
    const { defaultDriver, legacyDrivers } = await import("./driverMatrix");
    for (const kind of ["claude", "codex", "grok", "agy", "terminal"] as const) {
      expect(defaultDriver(undefined, kind)).not.toBe("claude-sdk");
      expect(defaultDriver({}, kind)).not.toBe("claude-sdk");
    }
    for (const kind of ["claude", "codex", "grok", "agy"] as const) {
      expect(legacyDrivers(kind)).not.toContain("claude-sdk");
    }
  });
});
