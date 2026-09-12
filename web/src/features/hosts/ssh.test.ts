import { describe, expect, it } from "vitest";
import { probeSshAlias } from "./ssh";

describe("ssh-stdio probe", () => {
  it("fails forge-doloris and times out sg-small", async () => {
    const bad = await probeSshAlias("forge-doloris");
    expect(bad.ok).toBe(false);
    if (!bad.ok) expect(bad.error).toMatch(/GSSAPI|comment/);
    const timeout = await probeSshAlias("devbox-sg-small");
    expect(timeout.ok).toBe(false);
  });

  it("probes a healthy alias", async () => {
    const ok = await probeSshAlias("devbox-sg");
    expect(ok.ok).toBe(true);
    if (ok.ok) {
      expect(ok.cli[0]?.kind).toBe("claude");
      expect(ok.rttMs).toBeGreaterThan(0);
    }
  });
});
