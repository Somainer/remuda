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
 * - 复制恢复命令: only for a kind with a documented resume form and a native
 *   session id. Only `claude --resume <id>` is documented.
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
  if (resumeCommand(instance)) keys.push("copyResume");
  if (instance.hostId && (instance.workspaceId || instance.cwd)) keys.push("newInProject");
  if (!ENDED.has(instance.lifecycle)) keys.push("close");
  return keys;
}

/** Single-quote a shell word, escaping embedded quotes (`'` → `'\''`). */
export function shellQuote(value: string): string {
  return `'${value.replace(/'/g, `'\\''`)}'`;
}

/**
 * The documented resume command for the row, or null.
 *
 * Only Claude documents `--resume <session uuid>` (docs/design/protocol.md
 * D-026); no other harness's resume form is documented and the remuda CLI has
 * no verb that resumes an instance by id, so the item is offered for a claude
 * row only. The id comes from the driver's report and is never trusted to be
 * shell-safe: it is single-quoted, with embedded quotes escaped.
 */
export function resumeCommand(instance: Instance): string | null {
  const sessionId = knowledgeValue(instance.nativeRef?.sessionId);
  if (!sessionId || instance.kind !== "claude") return null;
  return `claude --resume ${shellQuote(sessionId)}`;
}
