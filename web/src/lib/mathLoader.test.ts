import { describe, expect, it } from "vitest";
import {
  __setMathImporterForTest,
  getMathState,
  loadMath,
  resetMathEngineForTest,
} from "./mathRender";

/**
 * Sticky failure (#6): after a chunk import rejects the store publishes
 * "error"; the next loadMath() starts a NEW import from "loading" and
 * resolves to "ready" — a later mount renders instead of being stuck.
 */
describe("math loader sticky failure", () => {
  it("restarts the import after a rejection on the next loadMath()", async () => {
    resetMathEngineForTest();
    let attempts = 0;
    __setMathImporterForTest(() => {
      attempts += 1;
      return attempts === 1
        ? Promise.reject(new Error("chunk fetch failed"))
        : Promise.resolve({
            default: { renderToString: () => "<span class='katex'></span>" },
          });
    });

    await loadMath();
    expect(getMathState().status).toBe("error");
    expect(attempts).toBe(1);

    const p = loadMath();
    expect(getMathState().status).toBe("loading");
    await p;
    expect(getMathState().status).toBe("ready");
    expect(attempts).toBe(2);

    resetMathEngineForTest();
  });

  it("does not dedupe a retry while already in error state", async () => {
    resetMathEngineForTest();
    let attempts = 0;
    __setMathImporterForTest(() => {
      attempts += 1;
      return Promise.reject(new Error("fail"));
    });
    await loadMath();
    await loadMath();
    expect(getMathState().status).toBe("error");
    expect(attempts).toBe(2);
    resetMathEngineForTest();
  });
});
