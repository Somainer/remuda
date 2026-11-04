import {
  SECRET_SHAPE_DUMMY_PART,
  SECRET_SHAPE_MASK,
  SECRET_SHAPE_PLACEHOLDER,
  SECRET_SHAPE_RULES,
} from "./secretShapes.generated";

/**
 * The web's secret-shape redactor (home-main-extras r2 item 5).
 *
 * It is NOT a second list: `secretShapes.generated.ts` is generated from
 * `scripts/ci/secret-shapes.json`, the one candidate set the gate scan
 * (`scripts/ci/secret-scan.py`) and the Hub's secret lint read. The matching
 * mirrors that scanner's `looks_like_secret`/`is_redacted`: a rule's payload
 * (group 1, else the whole match) is withheld only when it is at least the
 * rule's `minPayload` and does not already read as a placeholder, a docs
 * fixture name, or a mask.
 *
 * Used before quoting worker text to Main, so a credential inside a question
 * body never reaches Main's transcript verbatim.
 */

/** What a withheld span is replaced with (the Hub's marker). */
export const SECRET_REDACTED = "[redacted]";

const MASK = new Set(SECRET_SHAPE_MASK);
const PLACEHOLDER = new RegExp(SECRET_SHAPE_PLACEHOLDER);
/** The shared pattern spells its flag inline (`(?i)`); JS carries it as `i`. */
const DUMMY_PART = new RegExp(SECRET_SHAPE_DUMMY_PART.replace(/^\(\?i\)/, ""), "i");

function maskedChars(body: string): boolean {
  return [...body].every((ch) => MASK.has(ch) || /\s/.test(ch));
}

/** Whether a candidate is already redacted or is a docs/fixture placeholder. */
export function looksRedacted(match: string, payload: string): boolean {
  const body = payload.trim().replace(/^["']+|["']+$/g, "");
  if (!body) return true;
  if (PLACEHOLDER.test(body)) return true;
  if (DUMMY_PART.test(body)) return true;
  const visible = match.slice(0, 4);
  const rest = match.slice(4);
  if (rest.length > 0 && maskedChars(rest)) return true;
  if (body.length <= 4 && [...body].every((ch) => MASK.has(ch) || /[A-Za-z0-9]/.test(ch))) {
    if ([...body].some((ch) => MASK.has(ch)) || /[*xX]$/.test(visible)) return true;
  }
  return false;
}

/**
 * Mask every shared secret shape in `text`. Prefixes stay (`Bearer
 * [redacted]`) and each payload is replaced in place; text with no candidate
 * is returned unchanged.
 */
export function redactSecretShapes(text: string): string {
  let out = text;
  for (const rule of SECRET_SHAPE_RULES) {
    const rx = new RegExp(rule.pattern, "g");
    out = out.replace(rx, (match: string, ...args: unknown[]) => {
      // captures, then offset and the input string.
      const group = typeof args[0] === "string" ? args[0] : null;
      const payload = group ?? match;
      const body = payload.trim().replace(/^["']+|["']+$/g, "");
      if (looksRedacted(match, payload)) return match;
      if (body.length < rule.minPayload) return match;
      const at = match.lastIndexOf(payload);
      if (at < 0) return match;
      return `${match.slice(0, at)}${SECRET_REDACTED}${match.slice(at + payload.length)}`;
    });
  }
  return out;
}
