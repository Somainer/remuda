// @vitest-environment node
import { describe, expect, it } from "vitest";
import { composite, contrast, parseColor, toHex, type Rgba } from "./contrast";
import { readSrc } from "./cssSource";

const tokensCss = readSrc("styles/tokens.css");

/* ------------------------------------------------------------------ */
/* visual-system.md §3: WCAG contrast over the full ground set, per   */
/* mode, read straight from tokens.css so the file is the authority.  */
/* ------------------------------------------------------------------ */

/** Body of the first rule whose selector text is exactly `selector`. */
function block(css: string, selector: string): string {
  const at = css.indexOf(`${selector} {`);
  if (at < 0) throw new Error(`missing block ${selector}`);
  let depth = 0;
  const start = css.indexOf("{", at);
  for (let i = start; i < css.length; i += 1) {
    if (css[i] === "{") depth += 1;
    if (css[i] === "}") {
      depth -= 1;
      if (depth === 0) return css.slice(start + 1, i);
    }
  }
  throw new Error(`unterminated block ${selector}`);
}

function declarations(body: string): Map<string, string> {
  const out = new Map<string, string>();
  for (const m of body.matchAll(/(--[\w-]+)\s*:\s*([^;]+);/g)) out.set(m[1]!, m[2]!.trim());
  return out;
}

const DARK = declarations(block(tokensCss, ":root"));
const LIGHT_MEDIA_BODY = block(block(tokensCss, "@media (prefers-color-scheme: light)"), ':root:not([data-appearance="dark"])');
const LIGHT_ATTR_BODY = block(tokensCss, ':root[data-appearance="light"]');
const LIGHT = declarations(LIGHT_ATTR_BODY);

const lines = (body: string) =>
  body
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean);

const MODES: Array<[string, Map<string, string>]> = [
  ["dark", DARK],
  ["light", LIGHT],
];

const BASES = ["--bg-nav", "--bg-canvas", "--bg-surface", "--bg-raised", "--bg-inset"];
const OVERLAYS = ["--bg-hover", "--bg-selected"];
const STATUS = ["attention", "danger", "success"] as const;
const TEXT = ["--fg-strong", "--fg-body", "--fg-muted", "--fg-faint", "--link", "--attention-fg", "--danger-fg", "--success-fg"];
const TOK = ["comment", "keyword", "type", "string", "number", "function", "meta", "attr", "del", "add"];

type Ground = { name: string; rgb: Rgba };

function tone(tokens: Map<string, string>, name: string): Rgba {
  const value = tokens.get(name);
  if (!value) throw new Error(`token ${name} not declared`);
  return parseColor(value);
}

/** Five opaque bases plus hover/selected composited onto each (15 grounds). */
function grounds(tokens: Map<string, string>): Ground[] {
  const out: Ground[] = [];
  for (const base of BASES) {
    const b = tone(tokens, base);
    out.push({ name: base, rgb: b });
    for (const overlay of OVERLAYS) out.push({ name: `${overlay} on ${base}`, rgb: composite(tone(tokens, overlay), b) });
  }
  return out;
}

function statusGrounds(tokens: Map<string, string>, status: string): Ground[] {
  return BASES.map((base) => ({
    name: `--${status}-bg on ${base}`,
    rgb: composite(tone(tokens, `--${status}-bg`), tone(tokens, base)),
  }));
}

function failures(tokens: Map<string, string>, fg: string, over: Ground[], min: number): string[] {
  const colour = tone(tokens, fg);
  return over
    .map((g) => ({ g, ratio: contrast(colour, g.rgb) }))
    .filter(({ ratio }) => ratio < min)
    .map(({ g, ratio }) => `${fg} on ${g.name} (${toHex(g.rgb)}) = ${ratio.toFixed(2)}`);
}

describe("tokens.css structure", () => {
  it("keeps the two light blocks verbatim-equal", () => {
    expect(lines(LIGHT_MEDIA_BODY)).toEqual(lines(LIGHT_ATTR_BODY));
  });

  it("declares every dark colour role in light too", () => {
    const missing = [...DARK.keys()].filter((k) => !LIGHT.has(k));
    expect(missing).toEqual([]);
  });

  it("does not use the old mode attribute", () => {
    expect(tokensCss).not.toContain("data-theme");
  });
});

describe.each(MODES)("contrast — %s", (_mode, tokens) => {
  const all = grounds(tokens);

  it.each(TEXT)("%s ≥ 4.5 on every base, hover and selected ground", (fg) => {
    expect(failures(tokens, fg, all, 4.5)).toEqual([]);
  });

  it.each(["--fg-strong", "--fg-body"])("%s ≥ 4.5 on every status ground", (fg) => {
    const over = STATUS.flatMap((s) => statusGrounds(tokens, s));
    expect(failures(tokens, fg, over, 4.5)).toEqual([]);
  });

  it.each(STATUS)("--%s-fg ≥ 4.5 on its own status ground", (status) => {
    expect(failures(tokens, `--${status}-fg`, statusGrounds(tokens, status), 4.5)).toEqual([]);
  });

  it.each(["--border-control", "--focus"])("%s ≥ 3 on every ground", (fg) => {
    expect(failures(tokens, fg, all, 3)).toEqual([]);
  });

  it("primary and attention fills carry readable text", () => {
    expect(contrast(tone(tokens, "--on-primary"), tone(tokens, "--primary-fill"))).toBeGreaterThanOrEqual(4.5);
    expect(contrast(tone(tokens, "--on-attention"), tone(tokens, "--attention-fg"))).toBeGreaterThanOrEqual(4.5);
  });

  it.each(TOK)("--tok-%s ≥ 4.5 on --bg-inset", (tok) => {
    expect(failures(tokens, `--tok-${tok}`, [{ name: "--bg-inset", rgb: tone(tokens, "--bg-inset") }], 4.5)).toEqual([]);
  });
});
