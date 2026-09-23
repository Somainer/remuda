// @vitest-environment node
import { describe, expect, it } from "vitest";
import { listCss, readSrc as read } from "./cssSource";

/* ------------------------------------------------------------------ */
/* tokenGuard (visual-system.md §1.1, plan §6.1). Modules whose first */
/* line is the strict marker may only speak in role tokens. UO-1b     */
/* widens this to every *.module.css.                                 */
/* ------------------------------------------------------------------ */

const MARKER = "/* @tokens strict */";

/** Modules UO-1 owns; each must carry the marker. */
const REQUIRED_STRICT = ["styles/ui.module.css", "features/settings/settings.module.css", "components/overlay.module.css"];

/** Transition names from §9; exact names only (`--danger` but not `--danger-fg`). */
const OLD_TOKENS = [
  "--canvas",
  "--ink",
  "--ink-0",
  "--ink-1",
  "--ink-2",
  "--line",
  "--paper",
  "--mute",
  "--dust",
  "--on-dust",
  "--cold",
  "--ok",
  "--danger",
  "--danger-strong",
  "--warn",
  "--info",
  "--diff-add",
  "--diff-del",
  "--diff-ctx",
  "--font",
  "--mono",
  "--radius",
  "--rail",
  "--list",
  "--top",
  "--bar",
  "--text",
  "--text-lg",
  "--text-xl",
  "--text-body",
  "--text-13",
  "--text-aux",
  "--text-label",
];

/**
 * Registered sub-12px literals (D-052 §9 shape-bound glyphs), as
 * `path → count`. Prefer var(--text-2xs), which needs no entry.
 */
const SMALL_TEXT_SITES: Record<string, number> = {};

const stripComments = (css: string) => css.replace(/\/\*[\s\S]*?\*\//g, "");

const ALL_CSS = listCss();
const STRICT = ALL_CSS.filter((rel) => read(rel).startsWith(MARKER));

function violations(rel: string): string[] {
  const css = stripComments(read(rel));
  const out: string[] = [];
  for (const m of css.matchAll(/#[0-9a-f]{3,8}\b/gi)) out.push(`hex colour ${m[0]}`);
  for (const name of OLD_TOKENS) {
    const re = new RegExp(`${name}(?![\\w-])`, "g");
    if (re.test(css)) out.push(`old token ${name}`);
  }
  for (const needle of ["[data-theme", "[data-appearance", "prefers-color-scheme", "backdrop-filter"]) {
    if (css.includes(needle)) out.push(needle);
  }
  if (/transition\s*:\s*all\b/.test(css)) out.push("transition: all");
  let small = 0;
  for (const m of css.matchAll(/font(?:-size)?\s*:[^;{}]*?(?<![\w.-])(\d+(?:\.\d+)?)px/g)) {
    if (Number(m[1]) < 12) small += 1;
  }
  if (small > (SMALL_TEXT_SITES[rel] ?? 0)) out.push(`${small} sub-12px font-size literal(s)`);
  return out;
}

describe("tokenGuard", () => {
  it.each(REQUIRED_STRICT)("%s carries the strict marker", (rel) => {
    expect(STRICT).toContain(rel);
  });

  it("strict modules speak only in role tokens", () => {
    const report = Object.fromEntries(STRICT.map((rel) => [rel, violations(rel)]).filter(([, v]) => v.length));
    expect(report).toEqual({});
  });

  it("the mode attribute appears only in tokens.css, only on :root", () => {
    const offenders: string[] = [];
    for (const rel of ALL_CSS) {
      const css = stripComments(read(rel));
      for (const m of css.matchAll(/([^{}]*)\[data-appearance[^\]]*\]/g)) {
        if (rel !== "styles/tokens.css") {
          offenders.push(`${rel}: ${m[0].trim()}`);
          continue;
        }
        const ok = /(^|[\s,])(:root\[data-appearance="(dark|light)"\]|:root:not\(\[data-appearance="dark"\])$/.test(
          m[0].trim(),
        );
        if (!ok) offenders.push(`${rel}: ${m[0].trim()}`);
      }
    }
    expect(offenders).toEqual([]);
  });
});
