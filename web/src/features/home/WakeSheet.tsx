import { useState } from "react";
import { Modal } from "../../components/Modal";
import { Sheet } from "../../components/Sheet";
import { useCompactLayout } from "./useMain";
import {
  orderWakes,
  wakeNextLabel,
  wakeNotifyLabel,
  wakeStore,
  wakeTriggerLabel,
  type WakeUp,
} from "./wakes";
import css from "./wakes.module.css";

/**
 * ⑩'s sheet (home.md §6.2, §9): the wake-ups of Main's lineage — name,
 * trigger, next fire and notify — with a per-row cancel behind a confirm.
 * Cancel is `DELETE /v1/lineages/{id}/wake-ups/{wake}`, which the Hub allows
 * only for a Human device; this surface is the owner's, never Main's.
 *
 * A stale read (home.md §8) greys the cancel controls, like every other home
 * action. The row leaves on the Hub's answer, not optimistically: the store
 * drops it only after the DELETE resolves.
 */
export function WakeSheet({
  open,
  wakes,
  stale,
  onClose,
  store = wakeStore,
}: {
  open: boolean;
  wakes: readonly WakeUp[];
  stale: boolean;
  onClose: () => void;
  store?: Pick<typeof wakeStore, "cancel">;
}) {
  const mobile = useCompactLayout();
  const [confirming, setConfirming] = useState<WakeUp | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const rows = orderWakes(wakes);
  return (
    <>
      <Sheet open={open} onClose={onClose} labelledBy="wakes-title" testId="wakes-sheet" variant={mobile ? "sheet" : "popover"}>
        <h2 id="wakes-title" className={css.title}>
          定时
        </h2>
        {rows.length === 0 ? (
          <p className={css.empty} data-testid="wakes-empty">
            没有定时
          </p>
        ) : (
          <ul className={css.list} data-testid="wakes-list">
            {rows.map((wake) => (
              <li key={wake.wakeId} className={css.row} data-testid="wakes-row" data-wake-id={wake.wakeId}>
                <div className={css.rowText}>
                  <span className={css.name} title={wake.name}>
                    {wake.name}
                  </span>
                  <span className={css.detail} data-testid="wakes-row-detail">
                    {wakeTriggerLabel(wake)} · 下次 {wakeNextLabel(wake)} · {wakeNotifyLabel(wake.notify)}
                  </span>
                  {wake.state === "done" ? (
                    <span className={css.state} data-testid="wakes-row-state">
                      已完成
                    </span>
                  ) : null}
                </div>
                <button
                  type="button"
                  className={css.cancel}
                  data-testid="wakes-cancel"
                  disabled={stale || busy || wake.state === "done"}
                  onClick={() => {
                    setError(null);
                    setConfirming(wake);
                  }}
                >
                  取消
                </button>
              </li>
            ))}
          </ul>
        )}
        {error ? (
          <p className={css.error} role="alert" data-testid="wakes-error">
            {error}
          </p>
        ) : null}
        <button type="button" className={css.close} data-testid="wakes-close" onClick={onClose}>
          关闭
        </button>
      </Sheet>
      <Modal open={confirming !== null} onClose={() => setConfirming(null)} labelledBy="wake-cancel-title">
        <p id="wake-cancel-title" data-testid="wake-cancel-confirm">
          取消定时「{confirming?.name}」？之后不会再触发。
        </p>
        <button type="button" data-testid="wake-cancel-no" onClick={() => setConfirming(null)}>
          返回
        </button>
        <button
          type="button"
          data-testid="wake-cancel-yes"
          disabled={stale || busy}
          onClick={() => {
            const target = confirming;
            if (!target) return;
            setBusy(true);
            void store.cancel(target.wakeId).then(
              () => {
                setBusy(false);
                setConfirming(null);
              },
              (reason: unknown) => {
                setBusy(false);
                setConfirming(null);
                setError(reason instanceof Error ? reason.message : String(reason));
              },
            );
          }}
        >
          取消定时
        </button>
      </Modal>
    </>
  );
}
