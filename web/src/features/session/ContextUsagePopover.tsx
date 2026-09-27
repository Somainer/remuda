import type { CSSProperties, RefObject } from "react";
import { useNow } from "./live/useElapsed";
import {
  contextHeadline,
  lastTurnLabel,
  sessionCells,
  tpmCells,
  type UsageCell,
  type UsageRollup,
} from "./contextUsage";
// UO-4: popover.module.css is imported BEFORE contextUsage.module.css so the
// base .popover/.popoverCard rules precede contextUsage's overrides — the
// source order they had in the old single session.module.css. The panel
// combines both bindings directly; using composes-only stubs here made the
// composed stylesheet inject AFTER contextUsage, flipping .usageSheet's
// z-index:60 under .popover's z-index:5 so the mobile sheet fell behind the
// options sheet.
import popover from "./popover.module.css";
import css from "./contextUsage.module.css";

/**
 * The context chip's hover/tap popover (context-usage-1): context-window
 * fill, session token totals, turn count, and TPM over a 60 s / 5 min
 * sliding window. Every figure comes from the Hub rollup; a channel the
 * harness never reported renders `—` with a tooltip naming that channel.
 *
 * The composer owns positioning (AnchoredPopover): `panelRef` is the anchor
 * panel element and `anchorStyle` carries its computed left/top/max-height.
 * On touch widths `mobile` renders the fixed bottom sheet instead.
 */
export function ContextUsagePopover({
  rollup,
  mobile,
  onClose,
  panelRef,
  anchorStyle,
  placement = "down",
  onMouseEnter,
  onMouseLeave,
}: {
  rollup: UsageRollup;
  mobile: boolean;
  onClose: () => void;
  /** Anchor panel ref the composer measures with. */
  panelRef?: RefObject<HTMLDivElement | null>;
  /** Computed fixed-position style from the AnchoredPopover hook. */
  anchorStyle?: CSSProperties;
  placement?: "up" | "down";
  onMouseEnter?: () => void;
  onMouseLeave?: () => void;
}) {
  // The last-turn relative label and the TPM windows age in place; the
  // parent's 2 s instance poll refreshes the numbers, this 1 Hz clock only
  // refreshes the relative-time wording.
  const nowMs = useNow(true);
  const head = contextHeadline(rollup);
  const pct = Math.max(0, Math.min(100, rollup.contextPct ?? 0));
  const cells = sessionCells(rollup);
  const tpm = tpmCells(rollup);
  const lastTurn = lastTurnLabel(rollup, nowMs);

  return (
    <div
      ref={panelRef}
      style={mobile ? undefined : anchorStyle}
      className={`${popover.popover} ${css.usagePopover} ${
        mobile ? css.usageSheet : popover.popoverCard
      }`}
      data-testid="context-usage-popover"
      data-mobile={mobile ? "1" : "0"}
      data-placement={mobile ? undefined : placement}
      role="dialog"
      aria-label="上下文用量"
      onMouseEnter={onMouseEnter}
      onMouseLeave={onMouseLeave}
    >
      <div className={css.usageHead}>
        <span>上下文用量</span>
        <button
          type="button"
          className={css.usageClose}
          data-testid="context-usage-close"
          aria-label="关闭上下文用量"
          onClick={onClose}
        >
          ×
        </button>
      </div>

      <div data-popover-scroll="1">
        <div className={css.usageSection}>
          <div
            className={css.usageContextLine}
            data-unknown={head.missing ? "1" : "0"}
            title={head.missing ?? undefined}
            data-testid="context-usage-headline"
          >
            上下文 {head.text}
          </div>
          <div
            className={css.usageBar}
            data-testid="context-usage-bar"
            data-pct={pct}
            role="img"
            aria-label={
              rollup.contextPct == null
                ? "上下文占用未知"
                : `上下文占用 ${rollup.contextPct}%`
            }
          >
            <span style={{ width: `${pct}%` }} />
          </div>
        </div>

        <div className={css.usageSection}>
          <div className={css.usageLabel}>本会话 tokens</div>
          <div className={css.usageGrid}>
            {cells.map((cell) => (
              <Cell key={cell.label} cell={cell} />
            ))}
          </div>
        </div>

        <div className={css.usageSection}>
          <span className={css.usageInline}>
            <span className={css.usageLabel}>回合数</span>
            <span data-testid="context-usage-turns">{rollup.turns}</span>
          </span>
        </div>

        <div className={css.usageSection}>
          <div className={css.usageLabel}>TPM</div>
          <table className={css.usageTpm}>
            <tbody>
              {tpm.map((row) => (
                <tr key={row.label}>
                  <th>{row.label}</th>
                  <td>
                    <Cell cell={row.in} inline />
                  </td>
                  <td>
                    <Cell cell={row.out} inline />
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>

        <div className={css.usageFoot}>
          <Cell cell={lastTurn} inline />
        </div>
      </div>
    </div>
  );
}

function Cell({ cell, inline = false }: { cell: UsageCell; inline?: boolean }) {
  const value = (
    <span
      className={cell.missingChannel ? css.usageUnknown : undefined}
      title={cell.missingChannel ?? cell.exact ?? undefined}
    >
      {cell.value}
    </span>
  );
  if (inline) {
    return (
      <span className={css.usageCellInline}>
        <span className={css.usageMute}>{cell.label}</span> {value}
      </span>
    );
  }
  return (
    <div className={css.usageCell} data-testid={`context-usage-cell-${cell.label}`}>
      <span className={css.usageMute}>{cell.label}</span>
      {value}
      {/* Exact figure stays reachable to assistive tech; sighted users get it
          on hover, the compact figure stays narrow. */}
      {cell.exact ? <span className={css.usageVisuallyHidden}>{cell.exact}</span> : null}
    </div>
  );
}
