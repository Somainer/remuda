import { useState } from "react";
import { Link, useLocation, useNavigate } from "react-router-dom";
import { Ellipsis, Inbox, MessagesSquare, Plus, type LucideIcon } from "lucide-react";
import { Icon } from "../components/Icon";
import { MORE_NAV, PHONE_NAV, isUnder } from "../lib/nav";
import ui from "../styles/ui.module.css";
import css from "./phoneNav.module.css";

const GLYPHS: Record<(typeof PHONE_NAV)[number]["id"], LucideIcon> = {
  home: MessagesSquare,
  inbox: Inbox,
  new: Plus,
  more: Ellipsis,
};

/**
 * Compact home bar (D-049, ui-overhaul §4.1): 会话 · 收件箱(n) · 新建 · 更多.
 * Rendered on home-level screens only — never on `/s/*`, whose bottom strip
 * belongs to the composer or the terminal bars. 更多 opens MORE_NAV in a sheet.
 */
export function PhoneNav({ pending, newHref }: { pending: number; newHref: string }) {
  const location = useLocation();
  const navigate = useNavigate();
  const [moreOpen, setMoreOpen] = useState(false);
  const [openPath, setOpenPath] = useState(location.pathname);
  if (openPath !== location.pathname) {
    setOpenPath(location.pathname);
    setMoreOpen(false);
  }
  const moreActive = MORE_NAV.some((item) => isUnder(location.pathname, item.to));
  const isActive = (id: (typeof PHONE_NAV)[number]["id"]) => {
    if (id === "home") return location.pathname === "/m";
    if (id === "inbox") return isUnder(location.pathname, "/m/inbox");
    if (id === "more") return moreActive;
    return false;
  };

  return (
    <>
      <nav className={css.bar} aria-label="手机底栏">
        {PHONE_NAV.map((item) => {
          const glyph = <Icon icon={GLYPHS[item.id]} size={20} />;
          if (item.id === "more") {
            return (
              <button
                key={item.id}
                type="button"
                className={css.item}
                data-active={isActive(item.id) ? "1" : undefined}
                data-testid="phone-nav-more"
                aria-expanded={moreOpen}
                aria-haspopup="menu"
                onClick={() => setMoreOpen((v) => !v)}
              >
                {glyph}
                <span className={css.label}>{item.label}</span>
              </button>
            );
          }
          if (item.id === "new") {
            return (
              <button
                key={item.id}
                type="button"
                className={css.item}
                data-testid="phone-nav-new"
                aria-label={item.label}
                onClick={() => navigate(newHref)}
              >
                {glyph}
                <span className={css.label} aria-hidden="true">
                  {item.label}
                </span>
              </button>
            );
          }
          return (
            <Link
              key={item.id}
              to={item.to}
              className={css.item}
              data-active={isActive(item.id) ? "1" : undefined}
              aria-current={isActive(item.id) ? "page" : undefined}
              data-testid={`phone-nav-${item.id}`}
              aria-label={item.id === "inbox" && pending ? `${item.label}(${pending})` : item.label}
            >
              <span className={css.glyph}>
                {glyph}
                {item.id === "inbox" && pending ? (
                  <span className={css.badge} data-testid="phone-inbox-badge">
                    {pending}
                  </span>
                ) : null}
              </span>
              <span className={css.label}>{item.label}</span>
            </Link>
          );
        })}
      </nav>
      {moreOpen ? (
        <div className={css.scrim} onClick={() => setMoreOpen(false)}>
          <div className={`${ui.sheet} ${css.sheet}`} role="menu" aria-label="更多" onClick={(event) => event.stopPropagation()}>
            {MORE_NAV.map((item) => (
              <Link
                key={item.id}
                role="menuitem"
                className={css.sheetItem}
                data-active={isUnder(location.pathname, item.to) ? "1" : undefined}
                to={item.to}
              >
                {item.label}
              </Link>
            ))}
          </div>
        </div>
      ) : null}
    </>
  );
}
