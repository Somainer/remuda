import { Button } from "../../../components/Button";
import css from "./TerminalView.module.css";

const KEYS: Array<[string, string]> = [
  ["Enter", "\r"],
  ["Esc", "\u001b"],
  ["Tab", "\t"],
  ["⇧", ""],
  ["⌃", "\u0003"],
  ["⌥", "\u001b"],
  ["⌘", ""],
  ["Ctrl+C", "\u0003"],
  ["↑", "\u001b[A"],
  ["↓", "\u001b[B"],
  ["←", "\u001b[D"],
  ["→", "\u001b[C"],
];

export function AuxKeys({ disabled, onKey }: { disabled: boolean; onKey: (data: string) => void }) {
  return (
    <div
      className={css.keys}
      role="toolbar"
      aria-label="终端辅助键"
      onPointerDown={(event) => {
        if ((event.target as HTMLElement).closest("button")) event.preventDefault();
      }}
    >
      {KEYS.filter(([, data]) => data).map(([label, data]) => (
        <Button key={label} disabled={disabled} aria-label={label} onClick={() => onKey(data)}>
          {label}
        </Button>
      ))}
    </div>
  );
}
