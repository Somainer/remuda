import type { Terminal } from "@xterm/xterm";

export type TerminalRenderer = "webgl" | "canvas" | "dom";

/** Prefer WebGL, then canvas, then the DOM renderer. */
export async function attachTerminalRenderer(term: Terminal): Promise<TerminalRenderer> {
  try {
    const { WebglAddon } = await import("@xterm/addon-webgl");
    const addon = new WebglAddon();
    addon.onContextLoss(() => {
      addon.dispose();
    });
    term.loadAddon(addon);
    return "webgl";
  } catch {
    /* canvas fallback */
  }
  try {
    const { CanvasAddon } = await import("@xterm/addon-canvas");
    term.loadAddon(new CanvasAddon());
    return "canvas";
  } catch {
    return "dom";
  }
}
