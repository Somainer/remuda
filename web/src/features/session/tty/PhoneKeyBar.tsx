import { useEffect, useMemo, useState } from "react";
import { useNavigate } from "react-router-dom";
import { Sheet } from "../../../components/Sheet";
import { hubStore, useHub } from "../../../lib/store";
import type { Instance } from "../../../types/instance";
import type { SessionView } from "../../../lib/viewPref";
import { openQuickFind } from "../../search/QuickFind";
import {
  clipboardReadStatusSync,
  probeClipboardRead,
  readClipboard,
  type ClipboardReadStatus,
} from "../../../lib/clipboard";
import { AuxKeys } from "./AuxKeys";
import { promptHistory } from "./promptHistory";
import { saveTtyScrollLine } from "./ttyScrollMemory";
import css from "./TerminalView.module.css";

/**
 * c-mkeybar (mobile-ui plan B.3.2 / ui-spec §4.7): the compact terminal
 * segment's bottom nine-key bar. Every key maps to a capability Remuda
 * already has — nothing is invented:
 *
 *   Ctrl · Esc · Tab · git · 跳转 · 贴 · 史 · 结构/终端 · 键
 *
 * Ctrl is the same sticky modifier as AuxKeys (shared state with the raw-key
 * second row). git navigates the existing /s/:id/files route; 跳转 opens the
 * grouped QuickFind Jump To sheet; 贴 reads the clipboard through lib/clipboard and sends the bytes on the raw
 * tty channel, disabled with a visible reason without permission; 史 only
 * FILLS the local input strip (D-028a write boundary — it never sends); the
 * view key shares the top-bar ViewSwitch navigation; 键 raises/dismisses the
 * soft keyboard and reveals the raw-key second row (arrows / PgUp / PgDn).
 */

/**
 * The xterm unmounts across the tty → files → tty trip, so the SessionPage
 * preFilesScroll ref cannot capture its position (the header files toggle is
 * folded away on the tty route). Navigation still uses the existing
 * /s/:id/files route; only the terminal's scroll LINE is stashed by the
 * capture callback and replayed through the xterm scroll API on remount.
 */

type ActionKey = {
  id: string;
  label: string;
  ariaLabel: string;
  /** Keys that write bytes to the PTY honor the frozen gate; navigation keys do not. */
  writes: boolean;
};

const ACTION_KEYS: ActionKey[] = [
  { id: "ctrl", label: "Ctrl", ariaLabel: "Ctrl 粘滞修饰键", writes: true },
  { id: "esc", label: "Esc", ariaLabel: "Esc", writes: true },
  { id: "tab", label: "Tab", ariaLabel: "Tab", writes: true },
  {
    id: "git",
    label: "git",
    ariaLabel: "打开 git 面板（工作区变更）",
    writes: false,
  },
  {
    id: "jump",
    label: "跳转",
    ariaLabel: "Jump To：跳转到其他会话",
    writes: false,
  },
  { id: "clip", label: "贴", ariaLabel: "粘贴剪贴板到终端", writes: true },
  {
    id: "history",
    label: "史",
    ariaLabel: "历史 prompt：填入本地输入条",
    writes: false,
  },
  { id: "view", label: "", ariaLabel: "", writes: false },
  {
    id: "keyboard",
    label: "键",
    ariaLabel: "唤起或收起软键盘与辅助键行",
    writes: false,
  },
];

export function PhoneKeyBar({
  instance,
  disabled,
  onKey,
  captureScrollLine,
  onFillInput,
}: {
  instance: Instance;
  /** TTY frozen (stale frame / reconnecting / failed): byte-writing keys gate. */
  disabled: boolean;
  onKey: (data: string) => void;
  /** TerminalView owns the xterm instance; this reads its current baseY. */
  captureScrollLine: () => number;
  onFillInput: (text: string) => void;
}) {
  const navigate = useNavigate();
  const hub = useHub();
  const [ctrl, setCtrl] = useState(false);
  const [auxOpen, setAuxOpen] = useState(false);
  const [historyOpen, setHistoryOpen] = useState(false);
  const [clipboard, setClipboard] = useState<ClipboardReadStatus>(
    clipboardReadStatusSync,
  );

  useEffect(() => {
    let alive = true;
    void probeClipboardRead().then((status) => {
      if (alive) setClipboard(status);
    });
    const refresh = () => {
      void probeClipboardRead().then((status) => {
        if (alive) setClipboard(status);
      });
    };
    window.addEventListener("focus", refresh);
    document.addEventListener("visibilitychange", refresh);
    return () => {
      alive = false;
      window.removeEventListener("focus", refresh);
      document.removeEventListener("visibilitychange", refresh);
    };
  }, []);

  const prompts = useMemo(
    () => promptHistory(instance.id, hub.events[instance.id] ?? []),
    [hub.events, instance.id],
  );

  // PhoneKeyBar only mounts on the tty segment, so the view key always points
  // at the structured projection; same navigation the top-bar ViewSwitch uses.
  const otherView: SessionView = "structured";
  const otherLabel = "结构";

  const sendRaw = (data: string) => {
    onKey(data);
    // Sticky Ctrl is one-shot, same as AuxKeys.
    if (ctrl) setCtrl(false);
  };

  const onAction = (id: string) => {
    switch (id) {
      case "ctrl":
        setCtrl((on) => !on);
        return;
      case "esc":
        sendRaw("");
        return;
      case "tab":
        sendRaw("\t");
        return;
      case "git":
        saveTtyScrollLine(instance.id, captureScrollLine());
        navigate(`/s/${instance.id}/files`);
        return;
      case "jump":
        // The shared QuickFind overlay only mounts inside the spaces drawer on
        // this route, so open that drawer and call the exported opener in
        // grouped mode — no copy of its state or UI (m-jumpto acceptance 4).
        document
          .querySelector<HTMLElement>("[data-testid='spaces-drawer-open']")
          ?.click();
        openQuickFind({ grouped: true });
        return;
      case "clip": {
        if (clipboard.state !== "ready") return;
        void readClipboard()
          .then((text) => {
            if (!text) {
              hubStore.toast("剪贴板为空");
              return;
            }
            onKey(text);
          })
          .catch(() => {
            // Read was attempted on a user gesture and the browser denied it;
            // the mouth is the toast plus the next probe flipping the button
            // disabled-with-reason — never a silent no-op.
            hubStore.toast("读取剪贴板失败：浏览器未授权");
            setClipboard({
              state: "blocked",
              reason: "浏览器拒绝了剪贴板读取权限",
            });
          });
        return;
      }
      case "history":
        setHistoryOpen(true);
        return;
      case "view":
        // Same onChange target as the top-bar ViewSwitch (SessionPage.tsx
        // navigates /s/:id/<view> and writes viewPref on arrival).
        navigate(`/s/${instance.id}/${otherView}`);
        return;
      case "keyboard": {
        setAuxOpen((open) => {
          const next = !open;
          const input = document.querySelector<HTMLInputElement>(
            "[data-testid='tty-dock'] input[aria-label='本地输入']",
          );
          if (next) requestAnimationFrame(() => input?.focus());
          else input?.blur();
          return next;
        });
        return;
      }
    }
  };

  return (
    <div
      className={css.phoneBar}
      data-testid="phone-keybar"
      data-aux={auxOpen ? "1" : "0"}
    >
      <div
        className={css.phoneRow}
        role="toolbar"
        aria-label="九键键盘条"
        onPointerDown={(event) => {
          if ((event.target as HTMLElement).closest("button"))
            event.preventDefault();
        }}
      >
        {ACTION_KEYS.map((key) => {
          const id = key.id === "view" ? otherView : key.id;
          const label = key.id === "view" ? otherLabel : key.label;
          const ariaLabel =
            key.id === "view"
              ? `切换到${otherLabel}视图（与顶栏分段同步）`
              : key.ariaLabel;
          const pressed =
            key.id === "ctrl"
              ? ctrl
              : key.id === "keyboard"
                ? auxOpen
                : undefined;
          const keyDisabled =
            key.writes &&
            (disabled || (key.id === "clip" && clipboard.state !== "ready"));
          return (
            <button
              key={key.id}
              type="button"
              className={`${css.phoneKey} ${pressed ? css.keyOn : ""}`}
              data-testid={`phone-key-${key.id}`}
              data-key={id}
              disabled={keyDisabled}
              aria-pressed={pressed}
              aria-label={ariaLabel}
              title={
                key.id === "clip" && clipboard.state === "blocked"
                  ? clipboard.reason
                  : undefined
              }
              onClick={() => onAction(key.id)}
            >
              {label}
            </button>
          );
        })}
      </div>
      {clipboard.state === "blocked" ? (
        <p className={css.clipReason} data-testid="phone-clip-reason">
          贴键不可用：{clipboard.reason}
        </p>
      ) : null}
      {auxOpen ? (
        <AuxKeys
          variant="phone"
          expanded
          disabled={disabled}
          onKey={onKey}
          stickyCtrl={ctrl}
          onStickyCtrlChange={setCtrl}
        />
      ) : null}
      <Sheet
        open={historyOpen}
        onClose={() => setHistoryOpen(false)}
        variant="sheet"
        testId="phone-history-sheet"
        // UO-10: the history panel belongs to the always-dark terminal
        // instrument, even though Sheet renders in a page-level portal.
        className={css.historySheet}
      >
        <div className={css.historyHead}>
          <h2>历史 prompt</h2>
          <button
            type="button"
            className={css.historyClose}
            data-testid="phone-history-close"
            onClick={() => setHistoryOpen(false)}
          >
            关闭
          </button>
        </div>
        <p className={css.historyHint}>
          选中只填入本地输入条，不会自动发送（D-028a）。
        </p>
        {prompts.length ? (
          <div className={css.historyList} role="list">
            {prompts.map((prompt, index) => (
              <button
                type="button"
                key={`${index}-${prompt.slice(0, 24)}`}
                className={css.historyItem}
                role="listitem"
                data-testid="phone-history-item"
                title={prompt}
                onClick={() => {
                  // Fill only: the strip keeps the text; the operator sends it.
                  onFillInput(prompt);
                  setHistoryOpen(false);
                }}
              >
                {prompt}
              </button>
            ))}
          </div>
        ) : (
          <p className={css.historyEmpty} data-testid="phone-history-empty">
            本实例还没有历史 prompt。
          </p>
        )}
      </Sheet>
    </div>
  );
}
