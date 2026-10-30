import type { ReactNode } from "react";
import { Link } from "react-router-dom";
import ui from "../../styles/ui.module.css";
import css from "./homeStates.module.css";

/**
 * First-snapshot placeholders (home.md §8): before the first successful Hub
 * read the list paints three skeleton rows — never an empty state. Cold start
 * must not flash 「还没有会话」 / 「暂无会话」 while the first read is in
 * flight (or while a reload is still offline).
 *
 * `variant` matches the host list's row geometry: the phone's 64px rows or
 * the desktop board's padded cards.
 */
export function SkeletonRows({
  count = 3,
  variant = "phone",
  testId = "home-skeleton",
}: {
  count?: number;
  variant?: "phone" | "desktop";
  testId?: string;
}) {
  return (
    <div
      className={css.skeleton}
      data-testid={testId}
      data-variant={variant}
      aria-busy="true"
      aria-label="加载中"
    >
      {Array.from({ length: count }, (_, index) => (
        <div key={index} className={css.skeletonRow} data-variant={variant}>
          <span className={css.dot} />
          <span className={css.lines}>
            <span className={`${css.line} ${css.lineTitle}`} />
            <span className={`${css.line} ${css.lineBody}`} />
          </span>
        </div>
      ))}
    </div>
  );
}

function EmptyFrame({ children, kind }: { children: ReactNode; kind: string }) {
  return (
    <div className={css.empty} data-testid="home-empty" data-empty={kind}>
      {children}
    </div>
  );
}

/**
 * Empty Space / no sessions at all: 「还没有会话」 + 「新建会话」. Distinct from
 * a search with no matches — this one offers the action that creates one.
 */
export function NoSessions({ newHref }: { newHref: string }) {
  return (
    <EmptyFrame kind="none">
      <p className={css.emptyTitle}>还没有会话</p>
      <Link className={ui.btn} to={newHref} data-testid="home-empty-new">
        新建会话
      </Link>
    </EmptyFrame>
  );
}

/**
 * A search/filter that matched nothing: 「没有匹配的会话」 + 「清除搜索」. The
 * sessions still exist; the action only changes what the list shows.
 */
export function NoMatches({ onClear }: { onClear: () => void }) {
  return (
    <EmptyFrame kind="no-match">
      <p className={css.emptyTitle}>没有匹配的会话</p>
      <button
        type="button"
        className={ui.btn}
        data-testid="home-empty-clear"
        onClick={onClear}
      >
        清除搜索
      </button>
    </EmptyFrame>
  );
}
