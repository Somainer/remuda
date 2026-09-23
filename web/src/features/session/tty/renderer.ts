import type { Terminal } from "@xterm/xterm";
import { probeWebglContextLoss } from "./rendererProbe";

export type TerminalRenderer = "webgl" | "canvas" | "dom";

/**
 * Attach the canvas (2D) renderer. Throws if that addon cannot load — the
 * WebGL addon's dispose() already re-installs xterm's built-in DOM renderer,
 * so a throw here leaves a working DOM terminal behind.
 */
async function loadCanvasRenderer(term: Terminal): Promise<void> {
  const { CanvasAddon } = await import("@xterm/addon-canvas");
  term.loadAddon(new CanvasAddon());
}

/**
 * Prefer WebGL, then canvas, then the DOM renderer.
 *
 * c-mfix real-device finding: on iOS Safari the WebGL context can be lost
 * (memory pressure, backgrounding) and never restored. Disposing the WebGL
 * addon alone falls back to the *DOM* renderer, which re-lays the whole
 * screen; instead, chain a canvas renderer in on context loss — it is the
 * same 2D path the initial fallback uses — and report the effective renderer
 * via `onEffective` so the toolbar pill/data-attr never advertise WebGL for a
 * terminal that no longer renders through it.
 */
export async function attachTerminalRenderer(
  term: Terminal,
  onEffective?: (name: TerminalRenderer) => void,
): Promise<TerminalRenderer> {
  let addon: import("@xterm/addon-webgl").WebglAddon | null = null;
  try {
    const { WebglAddon } = await import("@xterm/addon-webgl");
    addon = new WebglAddon();
    addon.onContextLoss(() => {
      // Make the WebGL→canvas fallback visible in a profile run.
      probeWebglContextLoss();
      // dispose() first: it re-installs xterm's own DOM renderer, so even if
      // the canvas import fails (offline, chunk error) rows still paint.
      addon?.dispose();
      void loadCanvasRenderer(term)
        .then(() => onEffective?.("canvas"))
        .catch(() => onEffective?.("dom"));
    });
    term.loadAddon(addon);
    return "webgl";
  } catch {
    addon?.dispose();
  }
  try {
    await loadCanvasRenderer(term);
    return "canvas";
  } catch {
    return "dom";
  }
}
