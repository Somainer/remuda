import { useState } from "react";
import { hubStore } from "../../lib/store";
import type { Workspace } from "../../types/workspace";
import css from "./workspaces.module.css";

export function WorkspaceList({ hostId, workspaces, online }: {
  hostId: string; workspaces: Workspace[]; online: boolean;
}) {
  const [removing, setRemoving] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const remove = (workspace: Workspace) => {
    // c-dirpicker: confirm before unbinding. Files are never deleted, and the
    // Hub/Node refuse the call with a reason while a live session or active
    // task still uses the directory.
    const ok = window.confirm(
      `从已注册目录移除「${workspace.rootPath}」？\n\n只会解除 Remuda 与该目录的绑定，不会删除磁盘上的任何文件。若仍有进行中的会话或任务使用它，移除会被拒绝；已结束的会话保留历史记录。`,
    );
    if (!ok) return;
    setRemoving(workspace.id);
    setError(null);
    void hubStore
      .unregisterWorkspace(hostId, workspace.rootPath)
      .catch((err: unknown) => setError(err instanceof Error ? err.message : "移除目录失败"))
      .finally(() => setRemoving(null));
  };
  return <div className={css.list} data-testid="host-workspaces">
    <div className={css.label}>已注册目录</div>
    {workspaces.length ? workspaces.map((workspace) => <div key={workspace.id} className={css.row} data-testid="host-workspace">
      <span className={css.path}>{workspace.rootPath}</span>
      <button type="button" className={css.action} disabled={!online || removing !== null}
        aria-label={`移除目录 ${workspace.rootPath}`} onClick={() => remove(workspace)}>
        {removing === workspace.id ? "移除中…" : "移除"}
      </button>
    </div>) : <span className={css.label}>暂无目录，请在新建会话中添加。</span>}
    {error ? <p className={css.error} role="alert">{error}</p> : null}
  </div>;
}
