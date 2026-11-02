import { useCallback, useEffect, useRef, useState, type PointerEvent as ReactPointerEvent } from "react";
import { useNavigate } from "react-router-dom";
import { Modal } from "../../components/Modal";
import { Sheet } from "../../components/Sheet";
import { clipboardIo } from "../../lib/clipboard";
import { hubStore } from "../../lib/store";
import type { Instance } from "../../types/instance";
import { resumeCommand, rowActionKeys, type RowActionKey } from "./rowActions";
import { useCompactLayout } from "./useMain";
import css from "./rowActions.module.css";

/** How long a press must hold before it counts as the menu's long-press entry. */
export const LONG_PRESS_MS = 500;
/** Movement beyond this cancels the long-press, so a scroll never opens it. */
const LONG_PRESS_SLOP_PX = 8;

/**
 * The class every ledger puts on its row element: the whole row is the ⋯
 * button's hover target (see rowActions.module.css).
 */
export const ROW_ACTIONS_HOST = css.host;

const LABELS: Record<RowActionKey, string> = {
  markSeen: "标为已看",
  interrupt: "中断",
  rename: "重命名",
  copyCwd: "复制目录",
  copyResume: "复制恢复命令",
  newInProject: "在此项目新建",
  close: "关闭…",
};

/**
 * The long-press handlers, for a caller that wants the press on the whole row
 * rather than on the ⋯ button. A short tap does nothing, so the row's own
 * click still navigates.
 *
 * The timer is cleared on a WINDOW pointerup/pointercancel as well as on the
 * element's own events: a press that starts inside the menu and ends on the
 * scrim (which closes the menu) retargets the pointerup elsewhere, and a
 * stranded timer would pop the menu open 500 ms after the user dismissed it.
 * The unmount cleanup clears it too, so a row removed by a poll inside the
 * hold window never opens a menu the ledger can no longer show.
 */
export function useRowLongPress(onOpen: () => void) {
  const press = useRef<{ timer: ReturnType<typeof setTimeout>; x: number; y: number } | null>(null);
  const clear = useCallback(() => {
    const current = press.current;
    if (!current) return;
    clearTimeout(current.timer);
    press.current = null;
  }, []);
  useEffect(() => {
    window.addEventListener("pointerup", clear);
    window.addEventListener("pointercancel", clear);
    return () => {
      clear();
      window.removeEventListener("pointerup", clear);
      window.removeEventListener("pointercancel", clear);
    };
  }, [clear]);
  return {
    onPointerDown(event: ReactPointerEvent) {
      if (event.button !== 0) return;
      // A press on an open overlay is that surface's gesture, not the row's.
      // The home row renders the menu's Sheet inside its own article, so a
      // HELD backdrop press would otherwise arm this timer and re-open the
      // menu (before pointerup) the press is dismissing.
      if ((event.target as Element | null)?.closest("[data-overlay]")) return;
      clear();
      const timer = setTimeout(() => {
        press.current = null;
        onOpen();
      }, LONG_PRESS_MS);
      press.current = { timer, x: event.clientX, y: event.clientY };
    },
    onPointerMove(event: ReactPointerEvent) {
      const current = press.current;
      if (!current) return;
      if (Math.abs(event.clientX - current.x) > LONG_PRESS_SLOP_PX || Math.abs(event.clientY - current.y) > LONG_PRESS_SLOP_PX) {
        clear();
      }
    },
    onPointerUp: clear,
    onPointerCancel: clear,
  };
}

/**
 * ⑦ 行操作菜单 (home.md §9), shared by the phone ledger, the desktop ledger
 * and ⑥ 工作中.
 *
 * Entry is a long-press on the row, or the 44px ⋯ button: on a fine pointer
 * the button paints only while its whole row is hovered or the button is
 * focus-visible, and it is always in flow and in tab order. Items that do not
 * apply to the row's state are absent ({@link rowActionKeys}). 关闭… confirms
 * first, naming the session and the host. While the read is stale every item
 * is disabled, including the portaled 关闭… confirm.
 */
export function RowActions({
  instance,
  open,
  onOpen,
  onClose,
  stale = false,
}: {
  instance: Instance;
  open: boolean;
  onOpen: () => void;
  onClose: () => void;
  /** home.md §8: a stale read greys the controls. */
  stale?: boolean;
}) {
  const navigate = useNavigate();
  // A read-only compact-layout read: useWorkbenchViewport's cleanup deletes
  // the shell's --workbench-* vars and keyboard flags, and a per-row caller
  // unmounting would collapse the /m band under the soft keyboard.
  const mobile = useCompactLayout();
  const [confirming, setConfirming] = useState(false);
  const [renaming, setRenaming] = useState(false);
  const longPress = useRowLongPress(onOpen);
  const title = hubStore.titleOf(instance.id);
  const host = hubStore.hostName(instance.hostId);
  const keys = rowActionKeys(instance);

  function act(key: RowActionKey) {
    if (stale) return;
    if (key === "markSeen") void hubStore.markSeen(instance.id).catch(() => undefined);
    else if (key === "interrupt") {
      if (!hubStore.canInterrupt) {
        hubStore.toast("主机离线，无法中断");
        return;
      }
      // 409/410 (the turn already ended) is the common refusal; never leave
      // it as an unhandled rejection.
      void hubStore.cancel(instance.id).catch(() => hubStore.toast("中断失败，请重试"));
    } else if (key === "rename") {
      setRenaming(true);
      return;
    } else if (key === "copyCwd") {
      if (instance.cwd) void copy(instance.cwd);
    } else if (key === "copyResume") {
      const command = resumeCommand(instance);
      if (command) void copy(command);
    } else if (key === "newInProject") {
      const params = new URLSearchParams({ host: instance.hostId, workspace: instance.workspaceId, cwd: instance.cwd ?? "" });
      navigate(`/sessions/new?${params}`);
    } else if (key === "close") {
      setConfirming(true);
      return;
    }
    onClose();
  }

  return (
    <div className={css.wrap} data-testid="row-actions">
      <button
        type="button"
        className={css.more}
        data-testid="row-actions-more"
        aria-label="行操作"
        aria-haspopup="menu"
        aria-expanded={open}
        disabled={stale}
        // The press lives on the trigger, never on the wrapper that holds the
        // Sheet: a pointerdown on the scrim must not arm a second open.
        {...longPress}
        onClick={(event) => {
          event.preventDefault();
          event.stopPropagation();
          if (open) onClose();
          else onOpen();
        }}
      >
        ⋯
      </button>
      <Sheet open={open} onClose={onClose} labelledBy="row-actions-title" testId="row-actions-panel" variant={mobile ? "sheet" : "popover"}>
        <h2 id="row-actions-title" className={css.heading}>行操作</h2>
        <ul className={css.menu} data-testid="row-actions-menu" role="menu">
          {keys.map((key) => (
            <li key={key}>
              <button
                type="button"
                role="menuitem"
                className={css.item}
                data-testid={`row-action-${key}`}
                disabled={stale || (key === "interrupt" && !hubStore.canInterrupt)}
                onClick={() => act(key)}
              >
                {LABELS[key]}
              </button>
            </li>
          ))}
        </ul>
      </Sheet>
      <Modal open={confirming} onClose={() => setConfirming(false)} labelledBy="row-close-title">
        <p id="row-close-title" data-testid="row-close-confirm">
          关闭会话「{title}」？（主机 {host}）
        </p>
        <button type="button" onClick={() => setConfirming(false)}>取消</button>
        <button
          type="button"
          data-testid="row-close-confirm-yes"
          disabled={stale}
          onClick={() => {
            setConfirming(false);
            onClose();
            void hubStore.close(instance.id).catch(() => hubStore.toast("关闭失败，请重试"));
          }}
        >
          关闭
        </button>
      </Modal>
      <Modal open={renaming} onClose={() => setRenaming(false)} labelledBy="row-rename-title">
        <form
          onSubmit={(event) => {
            event.preventDefault();
            const data = new FormData(event.currentTarget);
            hubStore.rename(instance.id, String(data.get("title") ?? ""));
            setRenaming(false);
            onClose();
          }}
        >
          <p id="row-rename-title">重命名</p>
          <input name="title" data-testid="row-rename-input" defaultValue={title} aria-label="会话名" />
          <button type="submit" data-testid="row-rename-save">保存</button>
        </form>
      </Modal>
    </div>
  );
}

function copy(text: string) {
  void clipboardIo.write(text).catch(() => hubStore.toast("复制失败"));
}