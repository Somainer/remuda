import { useState } from "react";
import type { Workspace } from "../../types/workspace";
import css from "./workspaces.module.css";
import { DirBrowser } from "./DirBrowser";

export function WorkspaceRegistration({ hostId, disabled, onRegistered }: {
  hostId: string; disabled?: boolean; onRegistered: (workspace: Workspace) => void;
}) {
  const [browsing, setBrowsing] = useState(false);
  return (
    <div className={css.registration}>
      <button type="button" className={css.action} data-testid="workspace-add"
        disabled={disabled} aria-haspopup="dialog"
        onClick={() => setBrowsing(true)}>
        + 添加目录
      </button>
      <DirBrowser hostId={hostId} open={browsing} disabled={disabled}
        onClose={() => setBrowsing(false)} onRegistered={onRegistered} />
    </div>
  );
}
