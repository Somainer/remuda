import { useState } from "react";
import css from "./TerminalView.module.css";

type KeyDef = { id: string; label: string; data?: string; mod?: "ctrl" | "alt" };

const BAR: KeyDef[] = [
  { id: "esc", label: "esc", data: "" },
  { id: "tab", label: "tab", data: "\t" },
  { id: "ctrl", label: "ctrl", mod: "ctrl" },
  { id: "alt", label: "alt", mod: "alt" },
  { id: "up", label: "↑", data: "[A" },
  { id: "down", label: "↓", data: "[B" },
  { id: "left", label: "←", data: "[D" },
  { id: "right", label: "→", data: "[C" },
  { id: "pgup", label: "pgup", data: "[5~" },
  { id: "pgdn", label: "pgdn", data: "[6~" },
  { id: "ctrl-c", label: "ctrl+c", data: "\u0003" },
];

const STRIP: KeyDef[] = [
  { id: "esc", label: "Esc", data: "" },
  { id: "tab", label: "Tab", data: "\t" },
  { id: "ctrl", label: "⌃", mod: "ctrl" },
  { id: "alt", label: "⌥", mod: "alt" },
  { id: "up", label: "↑", data: "[A" },
  { id: "down", label: "↓", data: "[B" },
  { id: "left", label: "←", data: "[D" },
  { id: "right", label: "→", data: "[C" },
  { id: "pgup", label: "PgUp", data: "[5~" },
  { id: "pgdn", label: "PgDn", data: "[6~" },
  { id: "ctrl-c", label: "⌃C", data: "\u0003" },
];

/**
 * c-mkeybar (mobile-ui plan B.3.2, ui-spec §4.7): the compact terminal's
 * second row defaults to these nine raw-byte keys; expanding brings back the
 * two BAR keys the action bar does not surface (alt, ⌃C), so the full 11-key
 * set and every tty-key-* testid stays reachable. BAR and STRIP are
 * untouched — desktop keeps both variants byte-for-byte.
 */
const PHONE: KeyDef[] = [
  { id: "esc", label: "Esc", data: "" },
  { id: "tab", label: "Tab", data: "\t" },
  { id: "ctrl", label: "Ctrl", mod: "ctrl" },
  { id: "up", label: "↑", data: "[A" },
  { id: "down", label: "↓", data: "[B" },
  { id: "left", label: "←", data: "[D" },
  { id: "right", label: "→", data: "[C" },
  { id: "pgup", label: "PgUp", data: "[5~" },
  { id: "pgdn", label: "PgDn", data: "[6~" },
];

function applyModifiers(data: string, ctrl: boolean, alt: boolean): string {
  let out = data;
  if (ctrl && out.length === 1) {
    const code = out.toLowerCase().charCodeAt(0);
    if (code >= 97 && code <= 122) out = String.fromCharCode(code - 96);
  }
  if (alt) out = `[${out}`;
  return out;
}

export function AuxKeys({
  disabled,
  onKey,
  variant = "bar",
  expanded = false,
  stickyCtrl,
  onStickyCtrlChange,
}: {
  disabled: boolean;
  onKey: (data: string) => void;
  variant?: "bar" | "toolbar" | "phone";
  /** phone variant only: the raw-key row shows alt / ⌃C once expanded. */
  expanded?: boolean;
  /**
   * phone variant only: when supplied the sticky Ctrl modifier is owned by the
   * caller (the nine-key action bar), so the Ctrl key in both rows reflects
   * and toggles one shared sticky state.
   */
  stickyCtrl?: boolean;
  onStickyCtrlChange?: (on: boolean) => void;
}) {
  const [ownCtrl, setOwnCtrl] = useState(false);
  const [alt, setAlt] = useState(false);
  const ctrl = stickyCtrl ?? ownCtrl;
  const keys = variant === "toolbar" ? STRIP : variant === "phone" && !expanded ? PHONE : BAR;

  const flipCtrl = () => {
    const next = !ctrl;
    if (onStickyCtrlChange) onStickyCtrlChange(next);
    else setOwnCtrl(next);
  };
  const clearCtrl = () => {
    if (!ctrl) return;
    onStickyCtrlChange?.(false);
    setOwnCtrl(false);
  };

  const press = (key: KeyDef) => {
    if (key.mod === "ctrl") {
      flipCtrl();
      return;
    }
    if (key.mod === "alt") {
      setAlt((on) => !on);
      return;
    }
    if (!key.data) return;
    onKey(applyModifiers(key.data, ctrl, alt));
    clearCtrl();
    if (alt) setAlt(false);
  };

  return (
    <div
      className={variant === "toolbar" ? css.auxStrip : css.keys}
      role="toolbar"
      aria-label="终端辅助键"
      data-testid="tty-keybar"
      onPointerDown={(event) => {
        if ((event.target as HTMLElement).closest("button")) event.preventDefault();
      }}
    >
      {keys.map((key) => (
        <button
          key={key.id}
          type="button"
          disabled={disabled}
          aria-label={key.label}
          aria-pressed={key.mod === "ctrl" ? ctrl : key.mod === "alt" ? alt : undefined}
          data-testid={`tty-key-${key.id}`}
          className={
            (key.mod === "ctrl" && ctrl) || (key.mod === "alt" && alt) ? css.keyOn : undefined
          }
          onClick={() => press(key)}
        >
          {key.label}
        </button>
      ))}
    </div>
  );
}
