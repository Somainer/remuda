import { useState } from "react";
import { api } from "../../lib/api";
import { TUI_OPTIONS } from "../../lib/sessionOptions";
import { hubStore } from "../../lib/store";
import type { Host, TuiMode } from "../../types/instance";
import { hostRegistry } from "../hosts/registry";
import css from "./settings.module.css";

export function HostTuiDefaults({ hosts }: { hosts: Host[] }) {
  const [pending, setPending] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  return (
    <section className={css.section} data-testid="settings-host-tui">
      <div className={css.label}>主机 · Claude 默认终端渲染</div>
      <p className={css.hint}>用于该主机的新会话；新建会话中的选择优先。</p>
      {hosts.map((host) => (
        <label key={host.id} className={css.field}>
          {host.label}
          <select
            className={css.input}
            data-testid={`settings-host-tui-${host.id}`}
            value={host.defaultTui ?? "fullscreen"}
            disabled={pending !== null}
            onChange={(event) => {
              const defaultTui = event.target.value as TuiMode;
              setPending(host.id);
              setError(null);
              void api.hostPatch(host.id, { defaultTui })
                .then(async (saved) => {
                  hostRegistry.patch(host.id, { defaultTui: saved.defaultTui });
                  await hubStore.refreshHosts();
                })
                .catch((err: unknown) => setError(err instanceof Error ? err.message : "保存失败"))
                .finally(() => setPending(null));
            }}
          >
            {TUI_OPTIONS.map((option) => <option key={option.id} value={option.id}>{option.label}</option>)}
          </select>
        </label>
      ))}
      {!hosts.length ? <p className={css.hint}>添加主机后可设置。</p> : null}
      {error ? <p className={css.hint} role="alert">{error}</p> : null}
    </section>
  );
}
