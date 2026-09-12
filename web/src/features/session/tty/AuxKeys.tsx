import { useState } from "react";
import css from "./TerminalView.module.css";

type KeyDef = { id: string; label: string; data?: string; mod?: "ctrl" | "alt" };

const BAR: KeyDef[] = [
  { id: "esc", label: "esc", data: "\u001b" },
  { id: "tab", label: "tab", data: "\t" },
  { id: "ctrl", label: "ctrl", mod: "ctrl" },
  { id: "alt", label: "alt", mod: "alt" },
  { id: "up", label: "↑", data: "\u001b[A" },
  { id: "down", label: "↓", data: "\u001b[B" },
  { id: "left", label: "←", data: "\u001b[D" },
  { id: "right", label: "→", data: "\u001b[C" },
  { id: "pgup", label: "pgup", data: "\u001b[5~" },
  { id: "pgdn", label: "pgdn", data: "\u001b[6~" },
  { id: "ctrl-c", label: "ctrl+c", data: "\u0003" },
];

const STRIP: KeyDef[] = [
  { id: "esc", label: "Esc", data: "\u001b" },
  { id: "tab", label: "Tab", data: "\t" },
  { id: "ctrl", label: "⌃", mod: "ctrl" },
  { id: "alt", label: "⌥", mod: "alt" },
  { id: "up", label: "↑", data: "\u001b[A" },
  { id: "down", label: "↓", data: "\u001b[B" },
  { id: "ctrl-c", label: "⌃C", data: "\u0003" },
];

function applyModifiers(data: string, ctrl: boolean, alt: boolean): string {
  let out = data;
  if (ctrl && out.length === 1) {
    const code = out.toLowerCase().charCodeAt(0);
    if (code >= 97 && code <= 122) out = String.fromCharCode(code - 96);
  }
  if (alt) out = `\u001b${out}`;
  return out;
}

export function AuxKeys({
  disabled,
  onKey,
  variant = "bar",
}: {
  disabled: boolean;
  onKey: (data: string) => void;
  variant?: "bar" | "toolbar";
}) {
  const [ctrl, setCtrl] = useState(false);
  const [alt, setAlt] = useState(false);
  const keys = variant === "toolbar" ? STRIP : BAR;

  const press = (key: KeyDef) => {
    if (key.mod === "ctrl") {
      setCtrl((on) => !on);
      return;
    }
    if (key.mod === "alt") {
      setAlt((on) => !on);
      return;
    }
    if (!key.data) return;
    onKey(applyModifiers(key.data, ctrl, alt));
    if (ctrl) setCtrl(false);
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
