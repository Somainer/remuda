import type { Instance } from "../../types/instance";
import { knowledgeValue } from "../../types/command";
import { rank } from "../mobile/homeRows";

/**
 * Which of the §9 row menu's items apply to a row. Hidden items are absent;
 * the menu never shows a control that cannot do anything for this row.
 *
 * - 标为已看: only a done-unseen row has something unseen to clear.
 * - 中断: only a row ranked working, and only when interrupt is not reported
 *   unsupported. A row that is merely idle, ended, or starting has no turn.
 * - 重命名: always. It is a local display name; no Hub route exists for it.
 * - 复制目录: only when the row has a working directory.
 * - 复制恢复命令: only when a native session id is known.
 * - 在此项目新建: only when the row names a host and a workspace or directory.
 * - 关闭…: only while the session is still alive.
 */
export type RowActionKey = "markSeen" | "interrupt" | "rename" | "copyCwd" | "copyResume" | "newInProject" | "close";

const ENDED = new Set<Instance["lifecycle"]>(["exited", "failed", "closing"]);

export function rowActionKeys(instance: Instance): RowActionKey[] {
  const keys: RowActionKey[] = [];
  if (rank(instance) === "done-unseen") keys.push("markSeen");
  if (rank(instance) === "working" && instance.capabilities?.capabilities?.interrupt?.state !== "unsupported") {
    keys.push("interrupt");
  }
  keys.push("rename");
  if (instance.cwd?.trim()) keys.push("copyCwd");
  if (knowledgeValue(instance.nativeRef?.sessionId)) keys.push("copyResume");
  if (instance.hostId && (instance.workspaceId || instance.cwd)) keys.push("newInProject");
  if (!ENDED.has(instance.lifecycle)) keys.push("close");
  return keys;
}

/** `claude --resume <id>` for a claude session, `<kind> --resume <id>` otherwise. */
export function resumeCommand(instance: Instance): string | null {
  const sessionId = knowledgeValue(instance.nativeRef?.sessionId);
  if (!sessionId) return null;
  const driver = instance.kind === "claude" ? "claude" : instance.kind;
  return `${driver} --resume ${sessionId}`;
}
