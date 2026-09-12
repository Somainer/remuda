import css from "./TerminalView.module.css";

const BAR: Array<[string, string]> = [
  ["Esc", "\u001b"],
  ["Tab", "\t"],
  ["⌃", "\u0003"],
  ["⌥", "\u001b"],
  ["↑", "\u001b[A"],
  ["↓", "\u001b[B"],
  ["⌘K", "\u001bk"],
];

const STRIP: Array<[string, string]> = [
  ["Esc", "\u001b"],
  ["⇧", ""],
  ["⌃", "\u0003"],
  ["⌥", "\u001b"],
  ["⌘", ""],
  ["Tab", "\t"],
];

export function AuxKeys({
  disabled,
  onKey,
  variant = "bar",
}: {
  disabled: boolean;
  onKey: (data: string) => void;
  variant?: "bar" | "toolbar";
}) {
  if (variant === "toolbar") {
    return (
      <div className={css.auxStrip} role="toolbar" aria-label="终端辅助键">
        {STRIP.map(([label, data]) =>
          data ? (
            <button key={label} type="button" disabled={disabled} aria-label={label} onClick={() => onKey(data)}>
              {label}
            </button>
          ) : (
            <span key={label}>{label}</span>
          ),
        )}
      </div>
    );
  }

  return (
    <div
      className={css.keys}
      role="toolbar"
      aria-label="终端辅助键"
      onPointerDown={(event) => {
        if ((event.target as HTMLElement).closest("button")) event.preventDefault();
      }}
    >
      {BAR.map(([label, data]) => (
        <button key={label} type="button" disabled={disabled} aria-label={label} onClick={() => onKey(data)}>
          {label}
        </button>
      ))}
    </div>
  );
}
