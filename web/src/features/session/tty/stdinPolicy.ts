/**
 * Stdin policy for the embedded xterm instance.
 *
 * xterm 6 gates far more than keystrokes on `disableStdin`: mouse and wheel
 * reports are dropped inside CoreService before `onData`, local scrollback
 * wheel is cancelled while the app has mouse tracking on, and the alt-screen
 * wheel→arrow conversion is skipped. So `disableStdin` is an all-or-nothing
 * switch — it can only mean "this terminal is not interactive at all".
 *
 * Therefore:
 *   - `frozen` (reconnecting / failed) is the only state that sets it, since
 *     nothing can reach the PTY anyway.
 *   - `keys` (local-input) mode leaves stdin enabled and filters the keyboard
 *     at the `onData` boundary instead (see `mouseReports.ts`), so a narrow
 *     viewport keeps clicks and wheel reports.
 *
 * Keep the derivation pure so construction and prop changes funnel through the
 * same code: the Terminal is rebuilt on `instance.id` while `directInput` and
 * `frozen` are unchanged, so the policy must be re-applied at construction.
 */
export type StdinPolicyInput = {
  directInput: boolean;
  frozen: boolean;
};

export type StdinPolicy = {
  disableStdin: boolean;
  focus: boolean;
};

export function stdinPolicy({ directInput, frozen }: StdinPolicyInput): StdinPolicy {
  return { disableStdin: frozen, focus: directInput && !frozen };
}

type StdinTarget = {
  options: { disableStdin?: boolean };
  focus: () => void;
};

/** Apply the policy to a live terminal. Safe to call repeatedly. */
export function applyStdinPolicy(term: StdinTarget | null | undefined, input: StdinPolicyInput): StdinPolicy {
  const policy = stdinPolicy(input);
  if (!term) return policy;
  term.options.disableStdin = policy.disableStdin;
  if (policy.focus) term.focus();
  return policy;
}
