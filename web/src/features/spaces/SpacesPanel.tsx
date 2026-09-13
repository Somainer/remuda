import { useState } from "react";
import { Link } from "react-router-dom";
import { StateDot } from "../../components/StateDot";
import { hubStore } from "../../lib/store";
import { projectStatus } from "../../lib/status";
import { newSessionPath, spaceStore, type Space, type SpacePrefs } from "./store";
import css from "./spaces.module.css";

export function HarnessGlyph({ kind }: { kind: string }) {
  return <span className={css.glyph} title={kind} aria-label={kind}>{({ claude: "✳", codex: "⌘", grok: "𝕏", agy: "A", terminal: ">_" } as Record<string, string>)[kind] ?? "◇"}</span>;
}

export function SpacesPanel({ spaces, active, prefs, onSelect, onNavigate, collapsed = false, drawer = false }: {
  spaces: Space[]; active?: Space; prefs: SpacePrefs; onSelect: (space: Space) => void; onNavigate?: () => void; collapsed?: boolean; drawer?: boolean;
}) {
  const [renaming, setRenaming] = useState<string>();
  return <section className={css.panel} data-testid="spaces-panel" data-collapsed={collapsed} aria-label="空间与会话">
    <header className={css.panelHead}>
      {!collapsed ? <span>Spaces <span className={css.muted}>/ Sessions</span></span> : null}
      {!drawer ? <button type="button" className={css.tool} data-testid="panel-toggle" aria-label={collapsed ? "展开空间面板" : "折叠空间面板"} title="⌘/Ctrl+B" aria-expanded={!collapsed} onClick={() => spaceStore.setCollapsed(!collapsed)}>{collapsed ? "»" : "«"}</button> : null}
    </header>
    <div className={css.groups}>
      {spaces.map((space, index) => <section key={space.id} className={css.group} data-active={space.id === active?.id}>
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
          {space.instances.length ? space.instances.map((instance) => <Link className={css.sessionRow} key={instance.id} to={`/s/${instance.id}`} onClick={() => { spaceStore.selectTab(space.id, instance.id); onNavigate?.(); }}>
            <HarnessGlyph kind={instance.kind} /><span className={css.spaceName}>{hubStore.titleOf(instance.id)}</span><StateDot status={projectStatus(instance)} />
          </Link>) : <div className={css.empty}>还没有 agent · ＋ 新建会话</div>}
        </> : null}
      </section>)}
      {!spaces.length ? <p className={css.empty}>{collapsed ? "—" : "注册工作区后在这里切换项目"}</p> : null}
    </div>
    {!collapsed ? <footer className={css.panelFoot}>活跃 / 待处理 <span>⌘/Ctrl+[ ] 切换</span></footer> : null}
  </section>;
}
