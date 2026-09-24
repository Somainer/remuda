import type { ReactNode } from "react";
import { Link } from "react-router-dom";
import css from "./pageHeader.module.css";

export type Crumb = { label: string; to?: string };

/**
 * The 48px page header (ui-overhaul §4.1): ancestor crumbs in muted text, the
 * current page as the `<h1>`, and the page's own actions on the right. Below
 * 1024px only the last ancestor stays, so the line never wraps.
 */
export function PageHeader({
  crumbs = [],
  title,
  actions,
  testId = "page-header",
}: {
  crumbs?: readonly Crumb[];
  title: string;
  actions?: ReactNode;
  testId?: string;
}) {
  return (
    <header className={css.header} data-testid={testId}>
      <nav className={css.trail} aria-label="当前位置">
        {crumbs.map((crumb, index) => (
          <span key={`${index}:${crumb.label}`} className={css.crumb} data-last={index === crumbs.length - 1 ? "1" : undefined}>
            {crumb.to ? (
              <Link className={css.crumbLink} to={crumb.to}>
                {crumb.label}
              </Link>
            ) : (
              <span className={css.crumbText}>{crumb.label}</span>
            )}
            <span className={css.sep} aria-hidden="true">
              /
            </span>
          </span>
        ))}
        <h1 className={css.title}>{title}</h1>
      </nav>
      {actions ? <div className={css.actions}>{actions}</div> : null}
    </header>
  );
}
