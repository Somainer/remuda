import { useState } from "react";
import { Link } from "react-router-dom";
import { StateDot } from "../../components/StateDot";
import { DELETE_UNSUPPORTED } from "../../lib/api";
import { HubHttpError } from "../../lib/httpError";
import { hubStore } from "../../lib/store";
import { projectStatus } from "../../lib/status";
import { newSessionPath, spaceSessions, spaceStore, type Space, type SpacePrefs } from "./store";
import type { Instance } from "../../types/instance";
import { ActionSheet } from "./ActionSheet";
import css from "./spaces.module.css";

export function HarnessGlyph({ kind }: { kind: string }) {
  return <span className={css.glyph} title={kind} aria-label={kind}>{({ claude: "✳", codex: "⌘", grok: "𝕏", agy: "A", terminal: ">_" } as Record<string, string>)[kind] ?? "◇"}</span>;
}

function SessionRow({ instance, spaceId, active, onNavigate }: { instance: Instance; spaceId: string; active: boolean; onNavigate?: () => void }) {
  return <Link className={css.sessionRow} data-testid="space-session" data-active={active} aria-current={active ? "page" : undefined}
    to={`/s/${instance.id}`} onClick={() => { spaceStore.selectTab(spaceId, instance.id); onNavigate?.(); }}>
    <HarnessGlyph kind={instance.kind} /><span className={css.spaceName}>{hubStore.titleOf(instance.id)}</span><StateDot status={projectStatus(instance)} />
  </Link>;
}

export function SpacesPanel({ spaces, active, prefs, instanceId, onSelect, onNavigate, collapsed = false, drawer = false }: {
  spaces: Space[]; active?: Space; prefs: SpacePrefs; instanceId?: string; onSelect: (space: Space) => void;
  onNavigate?: () => void; collapsed?: boolean; drawer?: boolean;
}) {
  const [renaming, setRenaming] = useState<string>();
  // Ids, not the row itself: a session that resumes while the sheet is open
  // must be offered 停止并删除 rather than the exited-only action.
  const [deleting, setDeleting] = useState<{ spaceId: string; instanceId: string }>();
  const [busy, setBusy] = useState(false);
  const target = deleting
    ? spaces.find((row) => row.id === deleting.spaceId)?.instances.find((row) => row.id === deleting.instanceId)
    : undefined;

  async function resume(instanceId: string) {
    try { await hubStore.resume(instanceId); } catch { hubStore.toast("恢复失败，请重试"); }
  }

  /** Stops the session first when asked, then deletes the record for real. */
  async function remove(spaceId: string, instance: Instance, stopFirst: boolean) {
    setBusy(true);
    try {
      if (stopFirst) await hubStore.close(instance.id);
      await hubStore.deleteInstance(instance.id);
      hubStore.toast("已删除会话");
    } catch (error) {
      // Until the Hub ships DELETE, the row leaves this device's lists and the
      // record stays on the Hub; say so rather than claiming a deletion.
      if (error instanceof HubHttpError && error.code === DELETE_UNSUPPORTED) {
        spaceStore.hideSession(spaceId, instance.id);
        hubStore.toast("当前 Hub 尚不支持删除，已从本设备列表隐藏");
      } else hubStore.toast("删除失败，请重试");
    } finally {
      setBusy(false);
      setDeleting(undefined);
    }
  }

  return <section className={css.panel} data-testid="spaces-panel" data-collapsed={collapsed} aria-label="空间与会话">
    <header className={css.panelHead}>
      {!collapsed ? <span>Spaces <span className={css.muted}>/ Sessions</span></span> : null}
      {!drawer ? <button type="button" className={css.tool} data-testid="panel-toggle" aria-label={collapsed ? "展开空间面板" : "折叠空间面板"} title="⌘/Ctrl+B" aria-expanded={!collapsed} onClick={() => spaceStore.setCollapsed(!collapsed)}>{collapsed ? "»" : "«"}</button> : null}
    </header>
    <div className={css.groups}>
      {spaces.map((space, index) => {
        const { live, exited } = spaceSessions(space, prefs);
        const exitedOpen = prefs.exitedOpen[space.id] === true;
        return <section key={space.id} className={css.group} data-active={space.id === active?.id}>
          <div className={css.groupHead}>
            {!collapsed ? <button type="button" className={css.disclosure} aria-label={`${prefs.groupCollapsed[space.id] ? "展开" : "折叠"} ${space.name}`} aria-expanded={!prefs.groupCollapsed[space.id]} onClick={() => spaceStore.toggleGroup(space.id)}>{prefs.groupCollapsed[space.id] ? "▸" : "▾"}</button> : null}
            <button type="button" className={css.spaceSelect} data-testid="space-select" data-space-id={space.id} aria-pressed={space.id === active?.id} title={`${space.name} · ${space.liveCount} 活跃 · ${space.blockedCount} 待处理`} onClick={() => onSelect(space)}>
              {collapsed ? <span className={css.initial}>{Array.from(space.name).slice(0, 2).join("").toUpperCase()}</span> : <><span className={css.spaceName}>{space.name}</span><span className={css.count} aria-label={`${space.liveCount} 活跃，${space.blockedCount} 待处理`}>{space.liveCount}<span className={space.blockedCount ? css.blocked : ""}> / {space.blockedCount}</span></span></>}
            </button>
          </div>
          {!collapsed && !prefs.groupCollapsed[space.id] ? <>
            <div className={css.groupMeta}>{space.hostId ? hubStore.hostName(space.hostId) : "未匹配已注册工作区"}</div>
            {renaming === space.id ? <form className={css.rename} onSubmit={(event) => {
              event.preventDefault();
              spaceStore.rename(space.id, String(new FormData(event.currentTarget).get("name") ?? ""));
              setRenaming(undefined);
            }}><input name="name" aria-label="空间名称" defaultValue={space.name} autoFocus maxLength={80} onKeyDown={(event) => { if (event.key === "Escape") setRenaming(undefined); }} /><button type="submit" className={css.tool}>保存</button></form> : null}
            {space.id === active?.id ? <div className={css.groupTools}>
              {space.workspaceId ? <button type="button" className={css.tool} onClick={() => setRenaming(space.id)}>重命名</button> : null}
              <button type="button" className={css.tool} aria-label={`上移 ${space.name}`} disabled={index === 0 || !space.workspaceId} onClick={() => spaceStore.moveSpace(space.id, -1, spaces.map((s) => s.id))}>↑</button>
              <button type="button" className={css.tool} aria-label={`下移 ${space.name}`} disabled={!space.workspaceId || index >= spaces.filter((s) => s.workspaceId).length - 1} onClick={() => spaceStore.moveSpace(space.id, 1, spaces.map((s) => s.id))}>↓</button>
              <Link className={css.tool} to={newSessionPath(space)} onClick={onNavigate} aria-label={`在 ${space.name} 新建会话`}>＋</Link>
            </div> : null}
            {live.map((instance) => <SessionRow key={instance.id} instance={instance} spaceId={space.id}
              active={instance.id === instanceId} onNavigate={onNavigate} />)}
            {exited.length ? <div className={css.exitedGroup}>
              <button type="button" className={css.exitedHead} data-testid="exited-toggle" data-space-id={space.id}
                aria-expanded={exitedOpen} onClick={() => spaceStore.toggleExited(space.id)}>
                <span className={css.disclosure} aria-hidden="true">{exitedOpen ? "▾" : "▸"}</span>已退出 ({exited.length})
              </button>
              {exitedOpen ? exited.map((instance) => <div key={instance.id} className={css.exitedRow} data-testid="exited-session" data-instance-id={instance.id}>
                <SessionRow instance={instance} spaceId={space.id} active={instance.id === instanceId} onNavigate={onNavigate} />
                <div className={css.exitedTools}>
                  <button type="button" className={css.tool} data-testid="exited-resume" aria-label={`恢复 ${hubStore.titleOf(instance.id)}`}
                    onClick={() => { void resume(instance.id); }}>恢复</button>
                  <button type="button" className={css.tool} data-testid="exited-delete" aria-label={`删除 ${hubStore.titleOf(instance.id)}`}
                    onClick={() => setDeleting({ spaceId: space.id, instanceId: instance.id })}>删除</button>
                </div>
              </div>) : null}
            </div> : null}
            {!live.length && !exited.length ? <div className={css.empty}>还没有 agent · ＋ 新建会话</div> : null}
          </> : null}
        </section>;
      })}
      {!spaces.length ? <p className={css.empty}>{collapsed ? "—" : "注册工作区后在这里切换项目"}</p> : null}
    </div>
    {!collapsed ? <footer className={css.panelFoot}>活跃 / 待处理 <span>⌘/Ctrl+[ ] 切换</span></footer> : null}
    {deleting && target ? <ActionSheet testId="delete-session-sheet" title="删除会话及其记录？"
      detail={projectStatus(target) === "exited"
        ? `「${hubStore.titleOf(target.id)}」的记录将被删除，无法恢复。`
        : `「${hubStore.titleOf(target.id)}」仍在运行，删除前会先停止它。记录将被删除，无法恢复。`}
      busy={busy} onClose={() => { if (!busy) setDeleting(undefined); }}
      actions={projectStatus(target) === "exited"
        ? [{ id: "delete-session-confirm", label: "删除", tone: "danger", onSelect: () => void remove(deleting.spaceId, target, false) }]
        : [{ id: "delete-session-stop", label: "停止并删除", tone: "danger", onSelect: () => void remove(deleting.spaceId, target, true) }]} /> : null}
  </section>;
}
