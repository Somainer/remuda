import { useState } from "react";
import ui from "../../styles/ui.module.css";
import css from "./hosts.module.css";

export type LaunchDefaultsPatch = {
  defaultLaunchArgs?: string[] | null;
  claudeBinaryPath?: string | null;
};

type Props = {
  args: string[] | undefined;
  binaryPath: string | undefined;
  /** Probed `claude` path from HostCli, shown as the placeholder. */
  probedBinaryPath?: string;
  disabled?: boolean;
  onSave: (patch: LaunchDefaultsPatch) => void;
};

/** Split on whitespace into an argv array. Never a shell parse. */
export function parseLaunchArgs(raw: string): string[] {
  return raw.split(/\s+/).filter(Boolean);
}

/**
 * Per-host launch defaults: extra CLI args and the claude executable.
 *
 * Saved explicitly rather than on every keystroke, because both fields are
 * only meaningful complete — a PATCH fired mid-word would store a half-typed
 * path and, for args, get rejected by the allowlist for a flag the operator
 * was still typing.
 */
export function HostLaunchDefaults({ args, binaryPath, probedBinaryPath, disabled, onSave }: Props) {
  const savedArgs = (args ?? []).join(" ");
  const savedBinary = binaryPath ?? "";
  const [argsDraft, setArgsDraft] = useState(savedArgs);
  const [binaryDraft, setBinaryDraft] = useState(savedBinary);
  // Re-sync when the host row changes underneath (another tab, a refetch)
  // without an effect: comparing against the last-seen saved value during
  // render avoids the extra render pass an effect would cause.
  const [seen, setSeen] = useState({ args: savedArgs, binary: savedBinary });
  if (seen.args !== savedArgs || seen.binary !== savedBinary) {
    setSeen({ args: savedArgs, binary: savedBinary });
    setArgsDraft(savedArgs);
    setBinaryDraft(savedBinary);
  }

  const tokens = parseLaunchArgs(argsDraft);
  const dirty = argsDraft.trim() !== savedArgs.trim() || binaryDraft.trim() !== savedBinary.trim();

  return (
    <div className={ui.field} data-testid="host-launch-defaults">
      <span>启动默认值</span>
      <input
        className={ui.input}
        data-testid="host-default-args"
        placeholder="--effort high --add-dir /srv"
        disabled={disabled}
        value={argsDraft}
        onChange={(e) => setArgsDraft(e.target.value)}
      />
      {tokens.length ? (
        <p className={css.hint} data-testid="host-default-args-chips">
          {tokens.map((token, index) => (
            <code key={`${token}-${index}`}>{token}</code>
          ))}
        </p>
      ) : null}
      <input
        className={ui.input}
        data-testid="host-default-binary"
        placeholder={probedBinaryPath || "/opt/claude/bin/claude"}
        disabled={disabled}
        value={binaryDraft}
        onChange={(e) => setBinaryDraft(e.target.value)}
      />
      <p className={css.hint}>
        新建会话留空时套用；会话自己填了就整组替换，不是拼接。可执行文件由 Node 校验，Hub 只存字符串。
      </p>
      <button
        type="button"
        className={css.add}
        data-testid="host-default-save"
        disabled={disabled || !dirty}
        onClick={() =>
          // `null` clears the default; an empty string would be stored as a
          // present-but-empty value, which is a different thing.
          onSave({
            defaultLaunchArgs: tokens.length ? tokens : null,
            claudeBinaryPath: binaryDraft.trim() || null,
          })
        }
      >
        保存
      </button>
    </div>
  );
}
