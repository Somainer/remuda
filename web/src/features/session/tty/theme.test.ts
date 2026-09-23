import { describe, expect, it } from "vitest";
import { contrast, parseColor } from "../../../styles/contrast";
import { NIGHT_CORRAL_THEME, TERMINAL_THEME } from "./theme";

const ANSI = [
  "red",
  "green",
  "yellow",
  "blue",
  "magenta",
  "cyan",
  "white",
  "brightBlack",
  "brightRed",
  "brightGreen",
  "brightYellow",
  "brightBlue",
  "brightMagenta",
  "brightCyan",
  "brightWhite",
] as const;

const on = (fg: string | undefined, bg: string | undefined) => contrast(parseColor(fg!), parseColor(bg!));

describe("TERMINAL_THEME", () => {
  it("is the always-dark 墨 terminal, with the old name as an alias", () => {
    expect(TERMINAL_THEME.background).toBe("#1a1917");
    expect(TERMINAL_THEME.foreground).toBe("#e4dfd6");
    expect(TERMINAL_THEME.cursor).toBe("#e0b872");
    expect(NIGHT_CORRAL_THEME).toBe(TERMINAL_THEME);
  });

  it("leaves ANSI 16–255 to xterm's standard cube", () => {
    expect(TERMINAL_THEME.extendedAnsi).toBeUndefined();
  });

  it.each(ANSI)("%s clears 4.5:1 on the terminal background", (name) => {
    expect(on(TERMINAL_THEME[name], TERMINAL_THEME.background)).toBeGreaterThanOrEqual(4.5);
  });

  it("keeps the foreground readable on the background and the selection", () => {
    expect(on(TERMINAL_THEME.foreground, TERMINAL_THEME.background)).toBeGreaterThanOrEqual(4.5);
    expect(on(TERMINAL_THEME.foreground, TERMINAL_THEME.selectionBackground)).toBeGreaterThanOrEqual(4.5);
  });
});
