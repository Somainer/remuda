import { describe, expect, it } from "vitest";
import { compareModelPin } from "./modelEffective";

// Anchored on ids measured against a real gateway (model-pin-1 §3), not
// invented pairs. The TS rule must stay in step with
// `remuda_protocol::compare_model_pin`.
describe("compareModelPin", () => {
  it("treats equal ids (and context-suffix variants) as honoured", () => {
    expect(compareModelPin("ark/seed-evolving[1m]", "ark/seed-evolving")).toBe("honoured");
    expect(
      compareModelPin(
        "model_hub/es1_orange_o50[1m]",
        "model_hub/es1_orange_o50[1m]",
      ),
    ).toBe("honoured");
  });

  it("flags a different id in the pin's namespace as a mismatch", () => {
    expect(
      compareModelPin(
        "model_hub/es1_orange_o50[1m]",
        "model_hub/es1_orange_o48[1m]",
      ),
    ).toBe("mismatch");
    expect(compareModelPin("sonnet", "haiku")).toBe("mismatch");
  });

  it("does NOT flag a gateway resolving the pin to an upstream vendor name", () => {
    // A correctly pinned session records the vendor name. Refusing this was
    // the equality-gate false positive.
    expect(compareModelPin("model_hub/es1_orange_o50[1m]", "claude-opus-5")).toBe(
      "unresolvable",
    );
    // The substituted demo case looks identical here — which is why neither is
    // refused on message.model alone.
    expect(compareModelPin("model_hub/es1_orange_o48[1m]", "claude-opus-4-8")).toBe(
      "unresolvable",
    );
  });

  it("uses the catalog to make an un-namespaced observation comparable", () => {
    const catalog = ["es1_orange_o48", "es1_orange_o50"];
    expect(compareModelPin("model_hub/es1_orange_o50[1m]", "es1_orange_o48", catalog)).toBe(
      "mismatch",
    );
    expect(compareModelPin("model_hub/es1_orange_o50[1m]", "claude-opus-5", catalog)).toBe(
      "unresolvable",
    );
  });

  it("treats a bare alias resolving to a namespaced id as unresolvable", () => {
    expect(compareModelPin("sonnet", "model_hub/es1_orange_o48")).toBe("unresolvable");
  });

  it("never mismatches an absent pin or observation", () => {
    expect(compareModelPin("", "claude-opus-5")).toBe("honoured");
    expect(compareModelPin("model_hub/es1_orange_o50[1m]", "  ")).toBe("honoured");
  });
});
