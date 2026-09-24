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
 */
export function balanceFences(text: string): string {
  let open: { char: string; len: number } | null = null;
  for (const line of text.split("\n")) {
    const m = /^ {0,3}(`{3,}|~{3,})(.*)$/.exec(line);
    if (!m) continue;
    const run = m[1];
    const rest = m[2];
    if (!open) {
      // A backtick fence's info string cannot itself contain a backtick.
      if (run[0] === "`" && rest.includes("`")) continue;
      open = { char: run[0], len: run.length };
    } else if (run[0] === open.char && run.length >= open.len && rest.trim() === "") {
      open = null;
    }
  }
  if (!open) return text;
  const close = open.char.repeat(open.len);
  return text.endsWith("\n") ? `${text}${close}` : `${text}\n${close}`;
}
