import { describe, expect, it } from "vitest";
import { readCollapsedGroups, writeCollapsedGroups } from "./groupPrefs";

/** A Storage stand-in so the tests never depend on the jsdom global. */
function memory(seed: Record<string, string> = {}) {
  const map = new Map(Object.entries(seed));
  return {
    getItem: (k: string) => map.get(k) ?? null,
    setItem: (k: string, v: string) => void map.set(k, v),
    raw: map,
  };
}

describe("collapsed group prefs", () => {
  it("round-trips one profile's collapsed keys", () => {
    const store = memory();
    writeCollapsedGroups("pvp_a", ["cursor/", "gemini-"], store);
    expect([...readCollapsedGroups("pvp_a", store)]).toEqual(["cursor/", "gemini-"]);
  });

  it("keeps profiles apart, so folding one gateway does not fold another", () => {
    const store = memory();
    writeCollapsedGroups("pvp_a", ["cursor/"], store);
    writeCollapsedGroups("pvp_b", ["openai/"], store);
    expect([...readCollapsedGroups("pvp_a", store)]).toEqual(["cursor/"]);
    expect([...readCollapsedGroups("pvp_b", store)]).toEqual(["openai/"]);
    expect(readCollapsedGroups("pvp_unknown", store).size).toBe(0);
  });

  it("drops the entry instead of storing an empty list", () => {
    const store = memory();
    writeCollapsedGroups("pvp_a", ["cursor/"], store);
    writeCollapsedGroups("pvp_a", [], store);
    expect(store.raw.get("runtime.provider-model-groups")).toBe("{}");
    expect(readCollapsedGroups("pvp_a", store).size).toBe(0);
  });

  it("survives junk in storage and a storage that throws", () => {
    expect(readCollapsedGroups("pvp_a", memory({ "runtime.provider-model-groups": "{" })).size).toBe(0);
    // A list where an object was expected, and a non-string key inside one.
    const odd = memory({ "runtime.provider-model-groups": '["cursor/"]' });
    expect(readCollapsedGroups("pvp_a", odd).size).toBe(0);
    const mixed = memory({ "runtime.provider-model-groups": '{"pvp_a":["ok",3,null]}' });
    expect([...readCollapsedGroups("pvp_a", mixed)]).toEqual(["ok"]);

    const hostile = {
      getItem() {
        throw new Error("blocked");
      },
      setItem() {
        throw new Error("quota");
      },
    };
    expect(readCollapsedGroups("pvp_a", hostile).size).toBe(0);
    // A device with storage denied still gets a working list.
    expect(() => writeCollapsedGroups("pvp_a", ["cursor/"], hostile)).not.toThrow();
  });
});
