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

const CHROMATIC = [
  "red",
  "green",
  "yellow",
  "blue",
  "magenta",
  "cyan",
  "brightRed",
  "brightGreen",
  "brightYellow",
  "brightBlue",
  "brightMagenta",
  "brightCyan",
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
      for (const name of CHROMATIC) {
        expect(on(theme[name], theme.background), `${mode} ${name}`).toBeGreaterThanOrEqual(
          4.5,
        );
      }
    },
  );

  it.each(["dark", "light"] as TerminalAppearance[])(
    "%s: foreground clears 4.5:1 on background and selection",
    (mode) => {
      const theme = terminalThemeFor(mode);
      expect(on(theme.foreground, theme.background)).toBeGreaterThanOrEqual(4.5);
      expect(on(theme.foreground, theme.selectionBackground)).toBeGreaterThanOrEqual(
        4.5,
      );
    },
  );

  it("light and dark use distinct backgrounds", () => {
    expect(LIGHT_TERMINAL_THEME.background).not.toBe(
      DARK_TERMINAL_THEME.background,
    );
  });

  it("dark white slots (TUI default text) clear 4.5:1 on the dark background", () => {
    // black/brightBlack are intentionally near-background (TUI dark
    // backgrounds); only the white slots carry default light text.
    for (const name of ["white", "brightWhite"] as const) {
      expect(on(DARK_TERMINAL_THEME[name], DARK_TERMINAL_THEME.background)).toBeGreaterThanOrEqual(
        4.5,
      );
    }
  });

  it("light: black and the default foreground are readable ON white/brightWhite TUI backgrounds", () => {
    // TUIs (htop/dialog/ncurses bars) paint black or default text on the
    // ANSI white slots; both pairs must clear 4.5:1.
    for (const slot of ["white", "brightWhite"] as const) {
      const bg = LIGHT_TERMINAL_THEME[slot]!;
      expect(on(LIGHT_TERMINAL_THEME.black, bg), `black on ${slot}`).toBeGreaterThanOrEqual(
        4.5,
      );
      expect(
        on(LIGHT_TERMINAL_THEME.foreground, bg),
        `default fg on ${slot}`,
      ).toBeGreaterThanOrEqual(4.5);
    }
  });

  it("light: ANSI white used as FOREGROUND on the page is the documented low-contrast case", () => {
    // Accepted trade-off so the same slot can serve as a TUI background:
    // pale foreground text on the page is below 4.5:1; programs use
    // truecolor / 256-cube 231 for pale text. Assert the choice is a light
    // grey (not near-black like the default foreground).
    const white = parseColor(LIGHT_TERMINAL_THEME.white!);
    const fg = parseColor(LIGHT_TERMINAL_THEME.foreground!);
    expect(Math.min(white.r, white.g, white.b)).toBeGreaterThan(
      Math.max(fg.r, fg.g, fg.b),
    );
  });
});
