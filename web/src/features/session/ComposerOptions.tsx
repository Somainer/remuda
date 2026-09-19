import { useRef, type ReactNode, type RefObject } from "react";
import { Sheet } from "../../components/Sheet";
import shared from "../../components/overlay.module.css";
import css from "./composerOptions.module.css";

/**
 * c-composer / D-042 (`ui-spec.md` §2.2 compact composer 边界):
 *
 * - {@link ComposerOptionsSheet} is the phone bottom sheet that holds the
 *   option-only controls — attachments, the read-only harness chip, the
 *   permission picker and the effort slider. The send/queue/interrupt
 *   three-state buttons, the queue chip and the 「尚未验证」 note never enter
 *   it; they stay on the collapsed bar.
 * - {@link ComposerConfirmDialog} replaces the two `window.confirm` calls
 *   (插队 / Esc 打断): a Sheet on every platform — `popover` on desktop,
 *   `sheet` on phones — with the shared focus trap (Esc cancels, focus
 *   returns to the trigger). The confirmed command path is unchanged.
 */

export function ComposerOptionsSheet({
  open,
  onClose,
  returnFocusRef,
  attach,
  harness,
  permission,
  effort,
}: {
  open: boolean;
  onClose: () => void;
  /** The collapsed trigger; focus returns here when the sheet closes. */
  returnFocusRef?: RefObject<HTMLElement | null>;
  attach?: ReactNode;
  harness?: ReactNode;
  permission?: ReactNode;
  effort?: ReactNode;
}) {
  return (
    <Sheet
      open={open}
      onClose={onClose}
      variant="sheet"
      testId="composer-options-sheet"
      labelledBy="composer-options-title"
      returnFocusRef={returnFocusRef}
      className={css.optionsPanel}
    >
      <div className={shared.head}>
        <h2 className={shared.title} id="composer-options-title">
          选项
        </h2>
        <button
          type="button"
          className={shared.close}
          data-testid="composer-options-close"
          onClick={onClose}
        >
          关闭
        </button>
      </div>
      {attach ? (
        <section className={css.section}>
          <h3 className={css.sectionLabel}>附件</h3>
          <div className={css.attachRow}>{attach}</div>
        </section>
      ) : null}
      {harness ? (
        <section className={css.section}>
          <h3 className={css.sectionLabel}>载体</h3>
          {harness}
        </section>
      ) : null}
      {permission ? (
        <section className={css.section}>
          <h3 className={css.sectionLabel}>权限</h3>
          <div className={css.optionList}>{permission}</div>
        </section>
      ) : null}
      {effort ? (
        <section className={css.section}>
          <h3 className={css.sectionLabel}>Effort</h3>
          {effort}
        </section>
      ) : null}
    </Sheet>
  );
}

export function ComposerConfirmDialog({
  open,
  variant,
  title,
  message,
  confirmLabel,
  onConfirm,
  onCancel,
}: {
  open: boolean;
  variant: "popover" | "sheet";
  title: string;
  message: string;
  confirmLabel: string;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  // The destructive action gets initial focus, mirroring window.confirm's
  // default accept; the shared trap returns focus to the trigger on close.
  const confirmRef = useRef<HTMLButtonElement>(null);
  return (
    <Sheet
      open={open}
      onClose={onCancel}
      variant={variant}
      testId="composer-confirm"
      labelledBy="composer-confirm-title"
      initialFocusRef={confirmRef}
      className={css.confirmPanel}
    >
      <h2 className={css.confirmTitle} id="composer-confirm-title" data-testid="composer-confirm-title">
        {title}
      </h2>
      <p className={css.confirmMessage}>{message}</p>
      <div className={css.confirmActions}>
        <button
          type="button"
          className={css.confirmCancel}
          data-testid="composer-confirm-cancel"
          onClick={onCancel}
        >
          取消
        </button>
        <button
          ref={confirmRef}
          type="button"
          className={css.confirmOk}
          data-testid="composer-confirm-ok"
          onClick={onConfirm}
        >
          {confirmLabel}
        </button>
      </div>
    </Sheet>
  );
}
