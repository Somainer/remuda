/**
 * Whether the transcript shows injected records (D-028 P3 feedback B).
 *
 * A Claude transcript files skill bodies, slash-command markup, hook context
 * and task notifications as `user` records. They stay in the journal — they
 * explain why the agent answered as it did — but they are collapsed to one
 * muted row by default, and this preference hides them outright.
 *
 * Device-wide rather than per-instance: it is a reading preference, and a user
 * who does not want to see injections in one session does not want them in the
 * next either.
 */
const KEY = "runtime.transcript-injected";

/**
 * Defaults to `true` — injected records are *shown*, as collapsed muted rows.
 *
 * The fix for the user's report is that they no longer masquerade as the
 * user's own bubbles, not that they vanish: they explain why the agent
 * answered the way it did, and a reader who opens a transcript to understand a
 * turn needs them. Hiding them entirely is the opt-in.
 */
export function readShowInjected(): boolean {
  try {
    return localStorage.getItem(KEY) !== "0";
  } catch {
    return true;
  }
}

export function writeShowInjected(show: boolean): void {
  try {
    localStorage.setItem(KEY, show ? "1" : "0");
  } catch {
    /* ignore quota */
  }
}
