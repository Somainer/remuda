import type { Instance } from "../../types/instance";

/**
 * D-062: a claimed external herdr pane. Remuda does not own it — closing one
 * only releases it, and the pane keeps running in the human's herdr.
 */
export function isClaimed(instance: Pick<Instance, "driver">): boolean {
  return instance.driver === "herdr-claimed";
}

/** The close verb for a row: 释放 for a claimed pane, the caller's word otherwise. */
export function closeVerb(instance: Pick<Instance, "driver">, fallback: string): string {
  return isClaimed(instance) ? "释放" : fallback;
}

/** The confirm sentence for releasing a claimed pane. */
export function releaseConfirmText(title: string): string {
  return `释放「${title}」？外部 herdr 里的 pane 和进程会继续运行，Remuda 只是不再接管它。`;
}

/**
 * Small 「外部 herdr」 badge with `session · workspace · tab`; 失联 while the
 * external herdr server does not answer (contact loss, never an end).
 */
export function HerdrClaimBadge({
  instance,
  testId = "herdr-claim-badge",
}: {
  instance: Pick<Instance, "driver" | "herdrClaim">;
  testId?: string;
}) {
  const claim = instance.herdrClaim;
  if (!isClaimed(instance) || !claim) return null;
  const where = [claim.session, claim.workspaceId, claim.tabId].filter(Boolean).join(" · ");
  const lost = claim.contact === "lost";
  const title = `外部 herdr：${where}（pane ${claim.paneId}）。Remuda 认领但不托管：释放后 pane 继续运行。${
    lost ? " 外部 herdr 暂时失联。" : ""
  }`;
  return (
    <span
      data-testid={testId}
      data-contact={lost ? "lost" : "ok"}
      title={title}
      style={{
        fontSize: 12,
        color: lost ? "var(--fg-strong)" : "var(--fg-muted)",
        border: "1px solid var(--border)",
        borderRadius: 999,
        padding: "0 6px",
        whiteSpace: "nowrap",
        flex: "none",
        maxWidth: "100%",
        overflow: "hidden",
        textOverflow: "ellipsis",
      }}
    >
      外部 herdr
      <span style={{ fontFamily: "var(--font-mono)", marginLeft: 4 }}>{where}</span>
      {lost ? <span style={{ marginLeft: 4 }}>· 失联</span> : null}
    </span>
  );
}
