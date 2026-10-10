import { useEffect, useRef, type KeyboardEvent, type ReactNode, type RefObject } from "react";
import { Ellipsis, type LucideIcon } from "lucide-react";
import { Icon } from "../components/Icon";
import { Sheet } from "../components/Sheet";
import ui from "../styles/ui.module.css";
import css from "./sessionHeader.module.css";

export type MoreMenuItem = {
  key: string;
  testId: string;
  label: string;
  icon: LucideIcon;
  onSelect?: () => void;
  /** Toggle items announce their state as a menuitemcheckbox. */
  checked?: boolean;
  disabled?: boolean;
  /** Extra data-* attributes (e.g. density-toggle's data-mode). */
  data?: Record<`data-${string}`, string>;
};

/**
 * The session ⋯ menu (ui-spec §2.2, D-053): a `.menu` popover anchored under
 * the trigger on desktop, a bottom `.sheet` on compact. Items are 48px rows in
 * the fixed §2.2 order the caller passes. Items are mounted only while the menu
 * is open, so a testid the transcript toolbar also carries (搜索正文 / 全部折叠)
 * is never duplicated on a closed page.
 */
export function SessionMoreMenu({
  open,
  onOpenChange,
  sheet,
  items,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Compact layout: render the phone sheet instead of the popover. */
  sheet: boolean;
  items: MoreMenuItem[];
}) {
  const triggerRef = useRef<HTMLButtonElement | null>(null);
  const close = () => onOpenChange(false);

  const menu = (
    <MenuList items={items} onClose={close} focusFirst={!sheet} triggerRef={triggerRef} />
  );

  return (
    <span className={css.moreAnchor}>
      <button
        type="button"
        className={ui.iconBtn}
        data-testid="session-more-open"
        aria-label="更多会话操作"
        aria-haspopup="menu"
        aria-expanded={open}
        ref={triggerRef}
        onClick={() => onOpenChange(!open)}
      >
        <Icon icon={Ellipsis} />
      </button>
      {sheet ? (
        <Sheet
          open={open}
          onClose={close}
          variant="sheet"
          testId="session-more-sheet"
          returnFocusRef={triggerRef}
        >
          {menu}
        </Sheet>
      ) : open ? (
        <DesktopPopover onClose={close} triggerRef={triggerRef}>
          {menu}
        </DesktopPopover>
      ) : null}
    </span>
  );
}

/** Outside press and Escape close the anchored popover; focus returns to ⋯. */
function DesktopPopover({
  onClose,
  triggerRef,
  children,
}: {
  onClose: () => void;
  triggerRef: RefObject<HTMLButtonElement | null>;
  children: ReactNode;
}) {
  const panelRef = useRef<HTMLDivElement | null>(null);
  useEffect(() => {
    const onDown = (event: MouseEvent) => {
      const target = event.target as Node | null;
      if (!target) return;
      if (panelRef.current?.contains(target) || triggerRef.current?.contains(target)) return;
      onClose();
    };
    document.addEventListener("mousedown", onDown);
    return () => document.removeEventListener("mousedown", onDown);
  }, [onClose, triggerRef]);
  return (
    <div ref={panelRef} className={`${ui.menu} ${css.morePopover}`} data-testid="session-more-popover">
      {children}
    </div>
  );
}

function MenuList({
  items,
  onClose,
  focusFirst,
  triggerRef,
}: {
  items: MoreMenuItem[];
  onClose: () => void;
  /** The phone sheet runs its own focus trap; the popover focuses here. */
  focusFirst: boolean;
  triggerRef: RefObject<HTMLButtonElement | null>;
}) {
  const listRef = useRef<HTMLDivElement | null>(null);
  useEffect(() => {
    if (focusFirst) listRef.current?.querySelector<HTMLElement>("[role^='menuitem']:not(:disabled)")?.focus();
  }, [focusFirst]);

  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    const entries = Array.from(
      listRef.current?.querySelectorAll<HTMLElement>("[role^='menuitem']:not(:disabled)") ?? [],
    );
    const index = entries.indexOf(document.activeElement as HTMLElement);
    let next = -1;
    if (event.key === "ArrowDown") next = (index + 1) % entries.length;
    else if (event.key === "ArrowUp") next = (index - 1 + entries.length) % entries.length;
    else if (event.key === "Home") next = 0;
    else if (event.key === "End") next = entries.length - 1;
    else if (event.key === "Escape" && focusFirst) {
      event.preventDefault();
      onClose();
      triggerRef.current?.focus();
      return;
    } else return;
    event.preventDefault();
    entries[next]?.focus();
  };

  return (
    <div
      ref={listRef}
      className={css.moreMenu}
      role="menu"
      aria-label="会话操作"
      data-testid="session-more-menu"
      onKeyDown={onKeyDown}
    >
      {items.map((item) => (
        <button
          key={item.key}
          type="button"
          role={item.checked === undefined ? "menuitem" : "menuitemcheckbox"}
          aria-checked={item.checked}
          className={`${ui.menuItem} ${css.moreItem}`}
          data-testid={item.testId}
          {...item.data}
          disabled={item.disabled}
          onClick={() => {
            onClose();
            // The sheet's trap returns focus itself; the popover hands it back
            // to ⋯ before the action runs, so an action that moves focus
            // (搜索正文 focuses its input a frame later) still wins.
            if (focusFirst) triggerRef.current?.focus();
            item.onSelect?.();
          }}
        >
          <Icon icon={item.icon} />
          <span>{item.label}</span>
        </button>
      ))}
    </div>
  );
}
