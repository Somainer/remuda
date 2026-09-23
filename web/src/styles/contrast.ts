/* ------------------------------------------------------------------ */
/* WCAG 2.x contrast for the visual-system token contract (D-053).    */
/* Translucent overlays (hover, selected, status grounds) are first   */
/* composited in sRGB onto an opaque base, then compared — the same   */
/* arithmetic visual-system.md §3 publishes its matrix with.          */
/* ------------------------------------------------------------------ */

export type Rgba = { r: number; g: number; b: number; a: number };

const HEX = /^#([0-9a-f]{3}|[0-9a-f]{6})$/i;
// Space- or comma-separated rgb()/rgba(), with an optional `/ alpha`.
const RGB = /^rgba?\(\s*([\d.]+)[\s,]+([\d.]+)[\s,]+([\d.]+)\s*(?:[/,]\s*([\d.]+%?))?\s*\)$/i;

/** Parse `#rgb`, `#rrggbb`, `rgb(r g b / a)` or `rgba(r, g, b, a)`. */
export function parseColor(value: string): Rgba {
  const text = value.trim();
  const hex = HEX.exec(text);
  if (hex) {
    const digits = hex[1]!.length === 3 ? [...hex[1]!].map((d) => d + d).join("") : hex[1]!;
    return {
      r: parseInt(digits.slice(0, 2), 16),
      g: parseInt(digits.slice(2, 4), 16),
      b: parseInt(digits.slice(4, 6), 16),
      a: 1,
    };
  }
  const rgb = RGB.exec(text);
  if (rgb) {
    const alpha = rgb[4] === undefined ? 1 : rgb[4].endsWith("%") ? parseFloat(rgb[4]) / 100 : parseFloat(rgb[4]);
    return { r: Number(rgb[1]), g: Number(rgb[2]), b: Number(rgb[3]), a: alpha };
  }
  throw new Error(`not a colour literal: ${value}`);
}

/** Source-over composite of `top` onto an opaque `base`, in sRGB. */
export function composite(top: Rgba, base: Rgba): Rgba {
  const a = top.a;
  return {
    r: top.r * a + base.r * (1 - a),
    g: top.g * a + base.g * (1 - a),
    b: top.b * a + base.b * (1 - a),
    a: 1,
  };
}

function channel(value: number): number {
  const c = value / 255;
  return c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
}

export function luminance({ r, g, b }: Rgba): number {
  return 0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b);
}

/** Contrast ratio of two opaque colours (1–21). */
export function contrast(a: Rgba, b: Rgba): number {
  const la = luminance(a);
  const lb = luminance(b);
  return (Math.max(la, lb) + 0.05) / (Math.min(la, lb) + 0.05);
}

/** Hex form of an opaque colour, for failure messages. */
export function toHex({ r, g, b }: Rgba): string {
  return `#${[r, g, b].map((v) => Math.round(v).toString(16).padStart(2, "0")).join("")}`;
}
