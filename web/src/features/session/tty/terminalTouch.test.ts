import { afterEach, describe, expect, it, vi } from "vitest";
import { attachTerminalTouch } from "./terminalTouch";

const cleanups: Array<() => void> = [];
afterEach(() => {
  cleanups.splice(0).forEach((cleanup) => cleanup());
  document.body.replaceChildren();
  vi.restoreAllMocks();
});

function fixture(height = 200, contentHeight = height) {
  const viewport = document.createElement("div");
  document.body.appendChild(viewport);
  Object.defineProperties(viewport, { clientHeight: { value: height }, scrollHeight: { value: contentHeight } });
  const onScrollPixels = vi.fn();
  const onGestureCancel = vi.fn();
  const state = { generation: 1, selection: false };
  cleanups.push(
    attachTerminalTouch(viewport, {
      onScrollPixels,
      onGestureCancel,
      getGeneration: () => state.generation,
      hasSelection: () => state.selection,
    }),
  );
  const touch = (type: string, points: Array<[number, number, number?]>, cancelable = true) => {
    const event = new Event(type, { bubbles: true, cancelable });
    Object.defineProperty(event, "touches", {
      value: points.map(([x, y, identifier = 1]) => ({ clientX: x, clientY: y, identifier })),
    });
    viewport.dispatchEvent(event);
    return event;
  };
  return { onScrollPixels, touch };
}

describe("terminal finger scrolling", () => {
  it("tracks vertical finger movement with wheel-compatible pixels", () => {
    const { onScrollPixels, touch } = fixture();
    expect(touch("touchstart", [[50, 100]]).defaultPrevented).toBe(false);
    expect(touch("touchmove", [[50, 103]]).defaultPrevented).toBe(false);
    expect(touch("touchmove", [[50, 120]]).defaultPrevented).toBe(true);
    touch("touchmove", [[52, 140]]);
    touch("touchmove", [[53, 110]]);
    expect(onScrollPixels.mock.calls).toEqual([
      [-14, 50, 120],
      [-20, 52, 140],
      [30, 53, 110],
    ]);
  });
});
