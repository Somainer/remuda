import { useState } from "react";
import { hubStore } from "../../lib/store";
import type { Workspace } from "../../types/workspace";
import css from "./workspaces.module.css";

export function WorkspaceList({ hostId, workspaces, online }: {
  hostId: string; workspaces: Workspace[]; online: boolean;
}) {
  const [removing, setRemoving] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  return <div className={css.list} data-testid="host-workspaces">
    <div className={css.label}>已注册目录</div>
    {workspaces.length ? workspaces.map((workspace) => <div key={workspace.id} className={css.row} data-testid="host-workspace">
      <span className={css.path}>{workspace.rootPath}</span>
      <button type="button" className={css.action} disabled={!online || removing !== null}
        aria-label={`移除目录 ${workspace.rootPath}`} onClick={() => {
          setRemoving(workspace.id); setError(null);
          void hubStore.unregisterWorkspace(hostId, workspace.rootPath)
            .catch((err: unknown) => setError(err instanceof Error ? err.message : "移除目录失败"))
            .finally(() => setRemoving(null));
        }}>{removing === workspace.id ? "移除中…" : "移除"}</button>
    </div>) : <span className={css.label}>暂无目录，请在新建会话中添加。</span>}
    {error ? <p className={css.error} role="alert">{error}</p> : null}
  </div>;
}
