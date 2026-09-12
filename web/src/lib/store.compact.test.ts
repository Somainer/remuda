import { afterEach, describe, expect, it } from "vitest";
import { hubStore } from "./store";

const KEY = "runtime.compact";

describe("compact preference", () => {
  afterEach(() => {
    localStorage.removeItem(KEY);
    hubStore.setCompact(true);
  });

  it("persists Compact/Full per device", () => {
    hubStore.setCompact(false);
    expect(localStorage.getItem(KEY)).toBe("0");
    expect(hubStore.getSnapshot().compact).toBe(false);
    hubStore.setCompact(true);
    expect(localStorage.getItem(KEY)).toBe("1");
    expect(hubStore.getSnapshot().compact).toBe(true);
  });
});
