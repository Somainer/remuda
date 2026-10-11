import { useRef, useState, useSyncExternalStore, type ReactNode } from "react";
import { Link, useLocation } from "react-router-dom";
import { ChevronLeft, Square } from "lucide-react";
import { Icon } from "../components/Icon";
import { StateDot } from "../components/StateDot";
import { ViewSwitch } from "../features/session/ViewSwitch";
import { SpacesDrawer } from "../features/spaces/SpacesMobile";
import type { Space, SpacePrefs } from "../features/spaces/store";
import type { SessionView } from "../lib/viewPref";
import type { UiStatus } from "../types/instance";
import ui from "../styles/ui.module.css";
import css from "./sessionHeader.module.css";

const WIDE_QUERY = "(min-width: 1024px)";

function subscribeWide(onChange: () => void) {
  if (typeof window.matchMedia !== "function") return () => {};
  const media = window.matchMedia(WIDE_QUERY);
  media.addEventListener("change", onChange);
  return () => media.removeEventListener("change", onChange);
}

/**
 * ≥1024: the wide desktop header keeps 「文件」 on the row; 768–1023 moves it
 * into ⋯ (ui-spec §2.2). No matchMedia (jsdom) reads as wide.
 */
export function useWideDesktop(): boolean {
  return useSyncExternalStore(
    subscribeWide,
    () => typeof window.matchMedia !== "function" || window.matchMedia(WIDE_QUERY).matches,
    () => true,
  );
}

export type SessionHeaderSpaces = {
  spaces: Space[];
  active?: Space;
  prefs: SpacePrefs;
  instanceId?: string;
  onSelect: (space: Space) => void;
};

/**
 * The one-row session header (ui-spec §2.2, D-053). Desktop: breadcrumb,
 * title and a 12px meta line on the left; ViewSwitch, 文件, Stop and ⋯ on the
 * right. Compact: 返回 ‹ / title block (opens the Space drawer) / 终端|结构 /
 * Stop / ⋯, 52px, with 44px reach only under a coarse pointer (§3.4).
 */
export function SessionHeader({
  mobile,
  title,
  taskTitle,
  status,
  statusLabel,
  hostName,
  cost,
  view,
  onView,
  files,
  onStop,
  more,
  spaces,
}: {
  mobile: boolean;
  title: string;
  taskTitle?: string | null;
  status: UiStatus;
  statusLabel: string;
  hostName: string;
  cost: string;
  /** The current projection when the session has both; null hides the switch. */
  view: SessionView | null;
  onView: (next: SessionView) => void;
  /** The inline 文件 control (wide desktop only); null when it lives in ⋯. */
  files: { active: boolean; onToggle: () => void } | null;
  /** Null once the session has exited. */
  onStop: (() => void) | null;
  /** The ⋯ trigger and its menu. */
  more: ReactNode;
  spaces: SessionHeaderSpaces;
}) {
  const spaceName = spaces.active?.name;
  const statusWord = (
    <span
      className={css.statusWord}
      data-testid="session-status-label"
      data-status={status}
      title={statusLabel}
    >
      {statusLabel}
    </span>
  );
  const switchControl = view ? <ViewSwitch value={view} onChange={onView} /> : null;
  const stop = onStop ? (
    <button
      type="button"
      className={`${ui.iconBtn} ${css.stopBtn}`}
      aria-label="Stop"
      title="停止会话"
      onClick={onStop}
    >
      <Icon icon={Square} />
    </button>
  ) : null;

  if (mobile) {
    return (
      <header className={css.header} data-layout="compact">
        <Link className={css.back} to="/sessions" aria-label="返回">
          <Icon icon={ChevronLeft} size={20} />
        </Link>
        <TitleBlock
          title={title}
          spaceName={spaceName}
          status={status}
          statusWord={statusWord}
          spaces={spaces}
        />
        {switchControl}
        {stop}
        {more}
      </header>
    );
  }

  const crumbs = [spaceName, taskTitle].filter((part): part is string => Boolean(part));
  return (
    <header className={css.header} data-layout="desktop">
      <div className={css.lead}>
        {crumbs.length > 0 ? (
          <span className={css.crumbs} data-testid="session-breadcrumb" title={crumbs.join(" / ")}>
            {crumbs.map((part, index) => (
              <span
                key={index}
                className={index === crumbs.length - 1 ? css.crumbLast : css.crumbLead}
              >
                {part}
                <span className={css.crumbSep} aria-hidden="true"> / </span>
              </span>
            ))}
          </span>
        ) : null}
        <h1 className={css.title} title={title}>
          {title}
        </h1>
        <span className={css.meta}>
          <StateDot status={status} />
          {statusWord}
          <span className={css.sep} aria-hidden="true">·</span>
          <span className={css.host} data-testid="session-host" title={`主机 ${hostName}`}>
            {hostName}
          </span>
          <span className={css.sep} aria-hidden="true">·</span>
          <span className={css.cost} data-testid="session-cost">
            {cost}
          </span>
        </span>
      </div>
      <div className={css.cluster}>
        {switchControl}
        {files ? (
          <button
            type="button"
            className={`${ui.btnGhost} ${css.filesBtn}`}
            data-testid="files-toggle"
            aria-pressed={files.active}
            onClick={files.onToggle}
          >
            文件
          </button>
        ) : null}
        {stop}
        {more}
      </div>
    </header>
  );
}

/**
 * The compact title block: one button that opens the Space drawer. Line 1 is
 * the state dot and title; line 2 the Space name and status word. Its own
 * inline-size container drives the D-049 sacrifice order (§4.7): title
 * ellipsis first, then the Space name falls back to its initial (≤120px), then
 * the status word folds (≤72px). The dot never leaves line 1.
 */
function TitleBlock({
  title,
  spaceName,
  status,
  statusWord,
  spaces,
}: {
  title: string;
  spaceName?: string;
  status: UiStatus;
  statusWord: ReactNode;
  spaces: SessionHeaderSpaces;
}) {
  const [open, setOpen] = useState(false);
  const opener = useRef<HTMLButtonElement | null>(null);
  // The drawer follows navigation away, same as the strip's drawer.
  const location = useLocation();
  const route = location.pathname + location.search;
  const [openRoute, setOpenRoute] = useState(route);
  if (openRoute !== route) {
    setOpenRoute(route);
    setOpen(false);
  }
  const name = spaceName ?? "空间";
  const initial = Array.from(name)[0]?.toUpperCase() ?? "";
  return (
    <>
      <h1 className={css.titleHeading}>
        <button
        type="button"
        ref={opener}
        className={css.titleBlock}
        data-testid="spaces-drawer-open"
        aria-expanded={open}
        aria-haspopup="dialog"
        title={`${name} / ${title}`}
        onClick={() => setOpen(true)}
      >
        <span className={css.titleLine}>
          <StateDot status={status} />
          <span className={css.titleText}>{title}</span>
        </span>
        <span className={css.subLine}>
          <span className={css.spaceName} data-testid="spaces-chips" data-initial={initial}>
            <span className={css.spaceFull}>{name}</span>
          </span>
          <span className={css.subSep} aria-hidden="true"> · </span>
          {statusWord}
        </span>
      </button>
      </h1>
      {/* A sibling of the heading, so the dialog is never part of its name. */}
      {open ? (
        <SpacesDrawer
          spaces={spaces.spaces}
          active={spaces.active}
          prefs={spaces.prefs}
          instanceId={spaces.instanceId}
          onSelect={spaces.onSelect}
          onClose={() => setOpen(false)}
          openerRef={opener}
        />
      ) : null}
    </>
  );
}
