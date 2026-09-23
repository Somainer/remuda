import type { ITheme } from "@xterm/xterm";

/**
 * The terminal is an always-dark instrument (visual-system.md §4): the same
 * palette in both appearances, never subscribed to mode changes. Every
 * non-black ANSI colour clears 4.5:1 on the background; ANSI 16–255 use
 * xterm's standard cube, so program output is never recoloured.
 */
export const TERMINAL_THEME: ITheme = {
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

/** Transition alias; removed by UO-1b once TerminalView reads TERMINAL_THEME. */
export const NIGHT_CORRAL_THEME = TERMINAL_THEME;

export const TERMINAL_FONT_FAMILY = '"IBM Plex Mono", ui-monospace, "SFMono-Regular", Menlo, Consolas, monospace';
