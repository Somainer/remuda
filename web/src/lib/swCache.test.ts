import { describe, expect, it } from "vitest";
import { cacheNameForBuild } from "./swCache";

describe("cacheNameForBuild", () => {
  it("gives two build ids two different cache names", () => {
    expect(cacheNameForBuild("5df699d7e6a6")).not.toBe(cacheNameForBuild("deadbeef1234"));
  });

  it("is stable for the same id", () => {
    expect(cacheNameForBuild("5df699d7e6a6")).toBe(cacheNameForBuild("5df699d7e6a6"));
  });

  it("carries the build id so a redeploy always changes the name", () => {
    expect(cacheNameForBuild("abc123")).toBe("runtime-shell-abc123");
  });

  it("stays a stable, name-safe token even for empty or dirty input", () => {
    expect(cacheNameForBuild("")).toBe("runtime-shell-dev");
    expect(cacheNameForBuild("feat/x y")).toBe("runtime-shell-featxy");
  });
});
