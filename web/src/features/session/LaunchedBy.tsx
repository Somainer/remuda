/**
 * Subtle provenance mark (D-028 §1.0 rule 4): `launchedBy` says who typed the
 * launch command — Remuda, or the user in their own terminal. Provenance only;
 * it never gates a capability, so it renders quietly.
 *
 * `compact` is a tiny title-only glyph for width-constrained tab strips; the
 * full pill is for sidebar rows and the session header.
 */
export function LaunchedByMark({
  launchedBy,
  testId = "launched-by",
  compact = false,
}: {
  launchedBy?: "remuda" | "user" | null;
  testId?: string;
  compact?: boolean;
}) {
  if (launchedBy !== "user" && launchedBy !== "remuda") return null;
  const isUser = launchedBy === "user";
  const title = isUser
    ? "用户在自己的终端里启动（D-025 promote）；能力与 Remuda 启动的会话相同"
    : "由 Remuda 在自持 PTY 里预填 launch 命令启动";
  if (compact) {
    return (
      <span
        data-testid={testId}
        data-launched-by={launchedBy}
        title={title}
        aria-label={title}
        style={{
          width: 6,
          height: 6,
          borderRadius: 999,
          flex: "none",
          background: isUser ? "var(--paper)" : "var(--line)",
        }}
      />
    );
  }
  return (
    <span
      data-testid={testId}
      data-launched-by={launchedBy}
      title={title}
      style={{
        fontFamily: "var(--mono)",
        fontSize: 10,
        color: isUser ? "var(--paper)" : "var(--mute)",
        border: "1px solid var(--line)",
        borderRadius: 999,
        padding: "0 6px",
        whiteSpace: "nowrap",
        flex: "none",
      }}
    >
      {isUser ? "user" : "remuda"}
    </span>
  );
}
