import { useState } from "react";
import { hubStore } from "../../lib/store";
import type { Workspace } from "../../types/workspace";
import css from "./workspaces.module.css";

export function WorkspaceRegistration({ hostId, disabled, onRegistered }: {
  hostId: string; disabled?: boolean; onRegistered: (workspace: Workspace) => void;
}) {
  const [adding, setAdding] = useState(false);
  const [path, setPath] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const register = async () => {
    const absolutePath = path.trim();
    if (!absolutePath.startsWith("/")) {
      setError("请输入这台主机上的绝对路径，例如 /home/dev/projects/app");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const workspace = await hubStore.registerWorkspace(hostId, absolutePath);
      if (workspace) onRegistered(workspace);
      setAdding(false);
      setPath("");
    } catch (err) {
      setError(err instanceof Error ? err.message : "添加目录失败");
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className={css.registration}>
      <button type="button" className={css.action} data-testid="workspace-add" disabled={disabled || busy}
        onClick={() => { setAdding((value) => !value); setError(null); }}>
        {adding ? "取消添加" : "+ 添加目录"}
      </button>
      {adding ? <>
        <label className={css.field}>
          主机上的绝对路径
          <input className={css.input} data-testid="workspace-register-path" value={path}
            placeholder="/home/dev/projects/app" disabled={busy} autoFocus
            onChange={(event) => setPath(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter" && !event.nativeEvent.isComposing) {
                event.preventDefault();
                if (!busy && !disabled) void register();
              }
            }} />
        </label>
        <button type="button" className={css.action} data-testid="workspace-register-submit"
          disabled={disabled || busy || !path.trim()} onClick={() => void register()}>
          {busy ? "添加中…" : "添加目录"}
        </button>
        {error ? <p className={css.error} role="alert">{error}</p> : null}
      </> : null}
    </div>
  );
}
