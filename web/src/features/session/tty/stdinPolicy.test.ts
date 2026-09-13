import { describe, expect, it, vi } from "vitest";
import { applyStdinPolicy, stdinPolicy } from "./stdinPolicy";

describe("stdinPolicy", () => {
  it("keeps stdin enabled for a live direct-input terminal and focuses it", () => {
    expect(stdinPolicy({ directInput: true, frozen: false })).toEqual({ disableStdin: false, focus: true });
  });

  it("keeps stdin enabled in local-input (keys) mode so the mouse survives", () => {
    // A3: `disableStdin` also gates mouse reports in xterm's CoreService, so
    // keys mode filters the keyboard at onData instead of disabling stdin.
    expect(stdinPolicy({ directInput: false, frozen: false })).toEqual({ disableStdin: false, focus: false });
  });

  it("disables stdin only while frozen", () => {
    expect(stdinPolicy({ directInput: true, frozen: true })).toEqual({ disableStdin: true, focus: false });
    expect(stdinPolicy({ directInput: false, frozen: true })).toEqual({ disableStdin: true, focus: false });
  });
});

describe("applyStdinPolicy", () => {
  const fake = () => ({ options: { disableStdin: true }, focus: vi.fn() });

  it("clears a stale disableStdin on a freshly built direct terminal and focuses it", () => {
    // A1: the Terminal is rebuilt on instance.id with directInput unchanged,
    // so construction must re-apply the policy or stdin stays dead.
    const term = fake();
    applyStdinPolicy(term, { directInput: true, frozen: false });
    expect(term.options.disableStdin).toBe(false);
    expect(term.focus).toHaveBeenCalledTimes(1);
  });

  it("leaves stdin enabled but unfocused in keys mode", () => {
    const term = fake();
    applyStdinPolicy(term, { directInput: false, frozen: false });
    expect(term.options.disableStdin).toBe(false);
    expect(term.focus).not.toHaveBeenCalled();
  });

  it("disables stdin while reconnecting", () => {
    const term = fake();
    term.options.disableStdin = false;
    applyStdinPolicy(term, { directInput: true, frozen: true });
    expect(term.options.disableStdin).toBe(true);
    expect(term.focus).not.toHaveBeenCalled();
  });

  it("is a no-op on a missing terminal but still reports the policy", () => {
    expect(applyStdinPolicy(null, { directInput: true, frozen: false })).toEqual({
      disableStdin: false,
      focus: true,
    });
  });
});
