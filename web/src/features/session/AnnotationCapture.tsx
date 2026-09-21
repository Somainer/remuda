import { useCallback, useEffect, useRef, useState } from "react";
import { Link, useLocation } from "react-router-dom";
import {
  readAnnotationSelection,
  type AnnotationAnchor,
  type AnnotationSelection,
} from "../tasks/annotations";
import { useAnnotationsContext } from "../tasks/AnnotationPanel";
import css from "../tasks/annotation.module.css";

/**
 * Selection-to-anchor affordance (plan task-model task 9 acceptance 1,
 * ui-spec §2.9). Selecting text inside a `data-anchor-surface` (the task
 * detail mandate body or a transcript message) raises a small 批注 trigger
 * above the selection; saving stores a ① anchor draft for the OWNING session
 * (inherited from `data-annotation-instance`). Read-only surfaces (sessions
 * of an archived task) raise nothing: the preview cannot annotate.
 *
 * Mounted once in the app Shell so the same gesture works on /board and on
 * /s/:id. It owns no composer state (D-042 untouched).
 */
export function AnnotationCapture() {
  const ctx = useAnnotationsContext();
  const location = useLocation();
  const [selection, setSelection] = useState<AnnotationSelection | null>(null);
  const [pos, setPos] = useState<{ top: number; left: number }>({ top: 0, left: 0 });
  const [open, setOpen] = useState(false);
  const [body, setBody] = useState("");
  const [saved, setSaved] = useState(false);
  const boxRef = useRef<HTMLDivElement>(null);
  const rafRef = useRef<number | null>(null);

  const dismiss = useCallback(() => {
    setOpen(false);
    setSelection(null);
    setBody("");
    setSaved(false);
    window.getSelection()?.removeAllRanges();
  }, []);

  const inspect = useCallback(() => {
    // While the popover is open the selection naturally moves into its
    // textarea; never re-inspect and dismiss the form mid-type.
    if (open) return;
    const sel = readAnnotationSelection();
    if (!sel || sel.readonly) {
      setSelection(null);
      return;
    }
    const rect = window.getSelection()?.getRangeAt(0).getBoundingClientRect();
    if (!rect || (rect.width === 0 && rect.height === 0)) {
      setSelection(null);
      return;
    }
    setPos({ top: Math.max(8, rect.top - 34), left: Math.max(8, rect.left) });
    setSelection(sel);
  }, [open]);

  useEffect(() => {
    const onSelectionChange = () => {
      if (rafRef.current != null) cancelAnimationFrame(rafRef.current);
      rafRef.current = requestAnimationFrame(inspect);
    };
    const onScroll = () => {
      if (!open) setSelection(null);
    };
    document.addEventListener("selectionchange", onSelectionChange);
    window.addEventListener("resize", onScroll);
    return () => {
      document.removeEventListener("selectionchange", onSelectionChange);
      window.removeEventListener("resize", onScroll);
      if (rafRef.current != null) cancelAnimationFrame(rafRef.current);
    };
  }, [inspect, open]);

  // Any route change abandons the in-flight gesture.
  useEffect(() => {
    dismiss();
  }, [location.pathname, dismiss]);

  useEffect(() => {
    if (!open) return;
    const onPointerDown = (event: PointerEvent) => {
      if (boxRef.current?.contains(event.target as Node)) return;
      dismiss();
    };
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") dismiss();
    };
    document.addEventListener("pointerdown", onPointerDown, true);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown, true);
      document.removeEventListener("keydown", onKey);
    };
  }, [open, dismiss]);

  if (!selection) return null;

  const anchor: AnnotationAnchor = {
    surface: selection.surface,
    messageId: selection.messageId,
    quote: selection.quote,
  };

  const save = () => {
    if (!body.trim()) return;
    ctx.addAnchor(selection.instanceId, anchor, body);
    setBody("");
    setSaved(true);
    // On the workbench the 标记 tab opens immediately; on /board the panel is
    // not mounted (no composer there) — the confirmation link is the way over.
    ctx.openPanel(selection.instanceId, "anchor", null);
  };

  return (
    <>
      {!open ? (
        <button
          type="button"
          className={css.captureTrigger}
          data-testid="annotation-capture"
          style={{ top: pos.top, left: pos.left }}
          // Keep the page selection alive while clicking.
          onMouseDown={(event) => event.preventDefault()}
          onClick={() => setOpen(true)}
        >
          批注
        </button>
      ) : null}
      {open ? (
        <div
          ref={boxRef}
          className={css.capture}
          data-testid="annotation-capture-popover"
          style={{ top: pos.top + 28, left: pos.left }}
        >
          <div className={css.captureHead}>
            <span className={css.captureTitle}>
              <span aria-hidden="true">①</span> 文本标记
            </span>
            <button
              type="button"
              className={css.remove}
              aria-label="关闭"
              onClick={dismiss}
            >
              ✕
            </button>
          </div>
          <p className={css.quotePreview} data-testid="annotation-capture-quote">
            {selection.quote}
          </p>
          {saved ? (
            <p className={css.savedNote} data-testid="annotation-capture-saved" role="status">
              已加入批注，将随该会话的下一次发送投递。
            </p>
          ) : null}
          <textarea
            className={css.textarea}
            data-testid="annotation-capture-input"
            autoFocus
            value={body}
            onChange={(event) => setBody(event.target.value)}
            placeholder="对这段正文的批注，随会话下一次发送投递…"
            onKeyDown={(event) => {
              if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
                event.preventDefault();
                save();
              }
              event.stopPropagation();
            }}
          />
          <div className={css.actions}>
            <button
              type="button"
              className={css.save}
              data-testid="annotation-capture-save"
              data-disabled={body.trim() ? "0" : "1"}
              disabled={!body.trim()}
              onClick={save}
            >
              加入批注
            </button>
            {location.pathname.startsWith("/board") ? (
              <Link
                className={css.cancel}
                to={`/s/${selection.instanceId}`}
                data-testid="annotation-capture-open-session"
              >
                去工作台查看
              </Link>
            ) : null}
            {saved && !location.pathname.startsWith("/board") ? (
              <button
                type="button"
                className={css.cancel}
                data-testid="annotation-capture-done"
                onClick={dismiss}
              >
                完成
              </button>
            ) : null}
          </div>
        </div>
      ) : null}
    </>
  );
}
