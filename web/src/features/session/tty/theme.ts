import type { ITheme } from "@xterm/xterm";

const INK = [0x12, 0x16, 0x1c] as const;
const PAPER = [0xe7, 0xdc, 0xc8] as const;

function hexByte(n: number): string {
  return Math.max(0, Math.min(255, Math.round(n))).toString(16).padStart(2, "0");
}

function rgbHex(r: number, g: number, b: number): string {
  return `#${hexByte(r)}${hexByte(g)}${hexByte(b)}`;
}

/** 6×6×6 cube + 24 greys (ANSI 16–255), tinted toward Night Corral ink/paper. */
export function nightCorralExtendedAnsi(): string[] {
  const out: string[] = [];
  const levels = [0, 95, 135, 175, 215, 255];
  const mix = 0.18;
  for (const r of levels) {
    for (const g of levels) {
      for (const b of levels) {
        const t = (r + g + b) / (255 * 3);
        out.push(
          rgbHex(
            r * (1 - mix) + (INK[0] * (1 - t) + PAPER[0] * t) * mix,
            g * (1 - mix) + (INK[1] * (1 - t) + PAPER[1] * t) * mix,
            b * (1 - mix) + (INK[2] * (1 - t) + PAPER[2] * t) * mix,
          ),
        );
      }
    }
  }
  for (let i = 0; i < 24; i++) {
    const t = i / 23;
    out.push(
      rgbHex(
        INK[0] + (PAPER[0] - INK[0]) * t,
        INK[1] + (PAPER[1] - INK[1]) * t,
        INK[2] + (PAPER[2] - INK[2]) * t,
      ),
    );
  }
  return out;
}

/** Night Corral tokens from ui-spec.md §6 mapped onto xterm, including 256-color cube. */
export const NIGHT_CORRAL_THEME: ITheme = {
  background: "#12161C",
  foreground: "#E7DCC8",
  cursor: "#C9842A",
  cursorAccent: "#12161C",
  selectionBackground: "#2A3340",
  black: "#12161C",
  red: "#8F3D2C",
  green: "#7A8F62",
  yellow: "#C9842A",
  blue: "#6A8B9A",
  magenta: "#8B7F6A",
  cyan: "#6A8B9A",
  white: "#E7DCC8",
  brightBlack: "#2A3340",
  brightRed: "#8F3D2C",
  brightGreen: "#7A8F62",
  brightYellow: "#C9842A",
  brightBlue: "#6A8B9A",
  brightMagenta: "#8B7F6A",
  brightCyan: "#6A8B9A",
  brightWhite: "#E7DCC8",
  extendedAnsi: nightCorralExtendedAnsi(),
};

export const TERMINAL_FONT_FAMILY = '"IBM Plex Mono", ui-monospace, "SFMono-Regular", Menlo, Consolas, monospace';
