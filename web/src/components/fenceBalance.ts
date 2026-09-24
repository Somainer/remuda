/**
 * Close an unterminated code fence in streaming markdown, at render time only.
 *
 * While an assistant reply streams, the text often stops inside a ``` block.
 * Rendered as-is, everything after the opening fence flips between prose and
 * code on every batch, and the row jumps. Appending the missing closing fence
 * keeps the partial block a code block until the real one arrives. The stored
 * text is never changed; callers apply this only while the message streams.
 *
 * Fences follow CommonMark: up to three spaces of indent, then three or more
 * backticks or tildes. A closing fence uses the same character, is at least as
 * long as the opener, and carries no info string.
 *
 * Container-aware: a fence may sit inside block quotes and list items. Each
 * line first consumes the prefixes of the containers still open (`>` for a
 * quote, the content indent for a list item); a container that does not match
 * closes, and a fence inside it ends with it. The appended closer repeats the
 * open containers' prefixes, so it lands inside the same container instead
 * of opening a new, empty block after it.
 */

type Container = { kind: "quote" } | { kind: "list"; indent: number };

const QUOTE = /^ {0,3}> ?/;
const LIST = /^( {0,3})([-+*]|\d{1,9}[.)])( +|$)/;
const FENCE = /^ {0,3}(`{3,}|~{3,})(.*)$/;

/** Consume one container's prefix from `line`; null when it does not continue. */
function continueContainer(container: Container, line: string): string | null {
  if (container.kind === "quote") {
    const m = QUOTE.exec(line);
    return m ? line.slice(m[0].length) : null;
  }
  if (line.trim() === "") return "";
  const lead = /^ */.exec(line)![0].length;
  return lead >= container.indent ? line.slice(container.indent) : null;
}

/** Open new containers at the start of `line`; returns the remaining content. */
function openContainers(line: string, stack: Container[]): string {
  for (;;) {
    const quote = QUOTE.exec(line);
    if (quote) {
      stack.push({ kind: "quote" });
      line = line.slice(quote[0].length);
      continue;
    }
    const list = LIST.exec(line);
    // A thematic break (`- - -`, `***`) is not a list item.
    if (list && !/^ {0,3}([-*_])( *\1){2,} *$/.test(line)) {
      const spaces = list[3].length;
      // Five or more spaces after the marker start indented code: the content
      // indent is then the marker plus one space.
      const gap = spaces === 0 || spaces > 4 ? 1 : spaces;
      const indent = list[1].length + list[2].length + gap;
      stack.push({ kind: "list", indent });
      line = line.slice(Math.min(line.length, indent));
      continue;
    }
    return line;
  }
}

function prefixFor(stack: readonly Container[]): string {
  return stack.map((c) => (c.kind === "quote" ? "> " : " ".repeat(c.indent))).join("");
}

export function balanceFences(text: string): string {
  const stack: Container[] = [];
  let open: { char: string; len: number; depth: number } | null = null;
  for (const raw of text.split("\n")) {
    let line = raw;
    let matched = 0;
    for (const container of stack) {
      const rest = continueContainer(container, line);
      if (rest === null) break;
      line = rest;
      matched += 1;
    }
    if (open && matched < open.depth) {
      // The fence's container closed, and the fence with it.
      open = null;
    }
    if (open) {
      const m = FENCE.exec(line);
      if (m && m[1][0] === open.char && m[1].length >= open.len && m[2].trim() === "") {
        open = null;
      }
      continue;
    }
    stack.length = matched;
    line = openContainers(line, stack);
    const m = FENCE.exec(line);
    if (!m) continue;
    const run = m[1];
    // A backtick fence's info string cannot itself contain a backtick.
    if (run[0] === "`" && m[2].includes("`")) continue;
    open = { char: run[0], len: run.length, depth: stack.length };
  }
  if (!open) return text;
  const close = prefixFor(stack.slice(0, open.depth)) + open.char.repeat(open.len);
  return text.endsWith("\n") ? `${text}${close}` : `${text}\n${close}`;
}
