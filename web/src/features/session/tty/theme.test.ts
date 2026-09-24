import { describe, expect, it } from "vitest";
import {
  DARK_TERMINAL_THEME,
  LIGHT_TERMINAL_THEME,
  NIGHT_CORRAL_THEME,
  terminalThemeFor,
  TERMINAL_THEME,
  type TerminalAppearance,
} from "./theme";
import { contrast, parseColor } from "../../../styles/contrast";

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

const on = (fg: string | undefined, bg: string | undefined) =>
  contrast(parseColor(fg!), parseColor(bg!));

describe("terminal themes follow appearance", () => {
  it("selects the palette by appearance, defaulting to dark", () => {
    expect(terminalThemeFor("dark")).toBe(DARK_TERMINAL_THEME);
    expect(terminalThemeFor("light")).toBe(LIGHT_TERMINAL_THEME);
    expect(TERMINAL_THEME).toBe(DARK_TERMINAL_THEME);
    expect(NIGHT_CORRAL_THEME).toBe(DARK_TERMINAL_THEME);
  });

  it.each(["dark", "light"] as TerminalAppearance[])(
    "%s theme leaves ANSI 16–255 to xterm's standard cube",
    (mode) => {
      expect(terminalThemeFor(mode).extendedAnsi).toBeUndefined();
    },
  );

  it.each(["dark", "light"] as TerminalAppearance[])(
    "%s: every chromatic ANSI colour clears 4.5:1 on its background",
    (mode) => {
      const theme = terminalThemeFor(mode);
      for (const name of ANSI) {
        expect(
          on(theme[name], theme.background),
          `${mode} ${name}`,
        ).toBeGreaterThanOrEqual(4.5);
      }
    },
  );

  it.each(["dark", "light"] as TerminalAppearance[])(
    "%s: foreground clears 4.5:1 on background and selection",
    (mode) => {
      const theme = terminalThemeFor(mode);
      expect(on(theme.foreground, theme.background)).toBeGreaterThanOrEqual(
        4.5,
      );
      expect(
        on(theme.foreground, theme.selectionBackground),
      ).toBeGreaterThanOrEqual(4.5);
    },
  );

  it("light and dark use distinct backgrounds", () => {
    expect(LIGHT_TERMINAL_THEME.background).not.toBe(
      DARK_TERMINAL_THEME.background,
    );
  });

  it("light white/brightWhite are dark greys (TUI default text stays readable)", () => {
    for (const name of ["white", "brightWhite"] as const) {
      const rgb = parseColor(LIGHT_TERMINAL_THEME[name]!);
      expect(Math.max(rgb.r, rgb.g, rgb.b)).toBeLessThan(120);
    }
  });
});
