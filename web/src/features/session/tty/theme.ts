import type { ITheme } from "@xterm/xterm";

/**
 * Terminal palettes (visual-system.md §4).
 *
 * The terminal FOLLOWS the workbench appearance (owner decision
 * 2026-09-24, superseding the earlier "always-dark instrument" rule):
 *
 *  - DARK_TERMINAL_THEME for dark appearance,
 *  - LIGHT_TERMINAL_THEME for light appearance.
 *
 * TerminalView switches live by assigning `term.options.theme` (no terminal
 * rebuild, scrollback preserved); WebGL/canvas repaint from the option.
 *
 * Every chromatic colour in BOTH palettes clears WCAG AA 4.5:1 on its own
 * terminal background (see theme.test.ts). ANSI 16–255 use xterm's standard
 * cube and truecolor passes through untouched — program output is never
 * recoloured beyond the 16 named palette slots.
 *
 * Light-palette white/brightWhite are deliberately DARK greys, not near
 * white: many TUIs paint normal/default text on "white" (the classic
 * light-terminal convention), so the ANSI "white" slot must be readable on
 * the light background. Programs that want the true page white emit
 * truecolor or 256-cube colour 231.
 */
export const DARK_TERMINAL_THEME: ITheme = {
  background: "#1a1917",
  foreground: "#e4dfd6",
  cursor: "#e0b872",
  cursorAccent: "#1a1917",
  selectionBackground: "#3d3a35",
  black: "#2c2a27",
  red: "#e8837c",
  green: "#9dbf87",
  yellow: "#dcb46a",
  blue: "#86aee0",
  magenta: "#c79ad8",
  cyan: "#7fbfbb",
  white: "#cfc9be",
  brightBlack: "#8f897e",
  brightRed: "#f4a59e",
  brightGreen: "#b9d6a4",
  brightYellow: "#ecd08f",
  brightBlue: "#a9c7ee",
  brightMagenta: "#dcb8e8",
  brightCyan: "#a0d8d3",
  brightWhite: "#f6f2ea",
};

export const LIGHT_TERMINAL_THEME: ITheme = {
  background: "#faf9f6",
  foreground: "#2b2a27",
  cursor: "#8a630f",
  cursorAccent: "#faf9f6",
  selectionBackground: "#d8e2ee",
  black: "#1f1e1b",
  red: "#c13d32",
  green: "#3a702e",
  yellow: "#8a630f",
  blue: "#2f5a9e",
  magenta: "#9335a8",
  cyan: "#1f6f6b",
  /*
   * ANSI white / brightWhite on the LIGHT theme are light GREYS, not near
   * white: TUIs (htop, dialog, ncurses status bars) paint black/default
   * text ON these slots, so black and the default foreground must clear
   * 4.5:1 on each (verified in theme.test.ts). The accepted trade-off:
   * these slots used as FOREGROUND on the light page are low-contrast —
   * programs wanting pale text emit truecolor or 256-cube colour 231.
   */
  white: "#96918a",
  brightBlack: "#767066",
  brightRed: "#a52a22",
  brightGreen: "#2e5c24",
  brightYellow: "#755308",
  brightBlue: "#224a8a",
  brightMagenta: "#7c2790",
  brightCyan: "#165e5a",
  brightWhite: "#a39e96",
};

export type TerminalAppearance = "dark" | "light";

export function terminalThemeFor(appearance: TerminalAppearance): ITheme {
  return appearance === "light" ? LIGHT_TERMINAL_THEME : DARK_TERMINAL_THEME;
}

/** Back-compat alias for older imports. */
export const TERMINAL_THEME = DARK_TERMINAL_THEME;

/** Transition alias; removed by UO-1b once TerminalView reads TERMINAL_THEME. */
export const NIGHT_CORRAL_THEME = DARK_TERMINAL_THEME;

export const TERMINAL_FONT_FAMILY =
  '"IBM Plex Mono", ui-monospace, "SFMono-Regular", Menlo, Consolas, monospace';
