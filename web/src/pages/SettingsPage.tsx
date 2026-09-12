import { useState } from "react";
import { Link } from "react-router-dom";
import { iosStandaloneHint, readDeviceSettings, writeDeviceSettings, type PermissionDefault } from "../features/settings";
import css from "../features/settings/settings.module.css";
import { readAccessCode, writeAccessCode } from "../lib/accessCode";
import { MORE_NAV } from "../lib/nav";
import { subscribePush } from "../lib/push";
import { hubStore, useHub } from "../lib/store";

const PERMS: { id: PermissionDefault; label: string }[] = [
  { id: "manual", label: "询问" },
  { id: "acceptEdits", label: "可改文件" },
  { id: "bypassPermissions", label: "全自动" },
];

export function SettingsPage() {
  const hub = useHub();
  const [access, setAccess] = useState(() => readAccessCode());
  const [settings, setSettings] = useState(() => readDeviceSettings());
  const [push, setPush] = useState(() => (typeof Notification === "undefined" ? "unsupported" : Notification.permission));

  const patch = (next: Partial<typeof settings>) => setSettings(writeDeviceSettings(next));

  return (
    <div className={css.page} data-testid="settings-page">
      <header className={css.head}>
        <h1 className={css.title}>设置</h1>
      </header>
      <div className={css.body}>
        <section className={css.section}>
          <div className={css.label}>设备</div>
          <label className={css.field}>
            设备名
            <input
              className={css.input}
              data-testid="settings-device-name"
              value={settings.deviceName}
              onChange={(e) => patch({ deviceName: e.target.value })}
            />
          </label>
          <label className={css.field}>
            remuda dev 访问码
            <input
              className={css.input}
              type="password"
              value={access}
              onChange={(e) => {
                setAccess(e.target.value);
                writeAccessCode(e.target.value);
              }}
            />
          </label>
          <p className={css.hint}>请求头 X-Remuda-Access-Code。也可用 VITE_ACCESS_CODE。不要把长期 token 放进 WebSocket URL。</p>
          <div className={css.row}>
            <button type="button" className={css.action} onClick={() => hubStore.setCompact(!hub.compact)}>
              Compact {hub.compact ? "开" : "关"}
            </button>
          </div>
        </section>

        <section className={css.section}>
          <div className={css.label}>推送</div>
          <p className={css.hint} data-testid="settings-ios-hint">
            {iosStandaloneHint()}
          </p>
          <p className={css.hint}>当前权限 {push}</p>
          <div className={css.row}>
            <button
              type="button"
              className={css.action}
              data-testid="settings-push"
              onClick={() => {
                void subscribePush().then((result) => {
                  setPush(result.ok ? "granted" : typeof Notification === "undefined" ? "unsupported" : Notification.permission);
                });
              }}
            >
              开启推送
            </button>
          </div>
        </section>

        <section className={css.section}>
          <div className={css.label}>主题</div>
          <p className={css.hint} data-testid="settings-theme">
            Night Corral。v1 无浅色开关。
          </p>
          <div className={css.row}>
            <button type="button" className={`${css.chip} ${css.chipOn}`} disabled>
              Night Corral
            </button>
            <button type="button" className={css.chip} disabled>
              浅色（v1 不做）
            </button>
          </div>
        </section>

        <section className={css.section}>
          <div className={css.label}>权限默认</div>
          <p className={css.hint}>新建会话的默认 permissionMode。全自动仅限本人遥控。</p>
          <div className={css.row} data-testid="settings-permission">
            {PERMS.map((opt) => (
              <button
                key={opt.id}
                type="button"
                className={`${css.chip} ${settings.permissionDefault === opt.id ? css.chipOn : ""}`}
                data-testid={`settings-perm-${opt.id}`}
                onClick={() => patch({ permissionDefault: opt.id })}
              >
                {opt.label}
              </button>
            ))}
          </div>
        </section>

        <section className={css.section}>
          <div className={css.label}>终端</div>
          <label className={css.check}>
            <input
              type="checkbox"
              data-testid="settings-auto-reveal-tty"
              checked={settings.autoRevealTty}
              onChange={(e) => patch({ autoRevealTty: e.target.checked })}
            />
            自动切 tty（autoRevealTty）
          </label>
          <p className={css.hint}>默认关。打开后仍须 capabilities.artifact；M3 才考虑生产打开。</p>
        </section>

        <section className={css.section}>
          <div className={css.label}>更多</div>
          {MORE_NAV.map((item) => (
            <Link key={item.id} to={item.to} className={css.link}>
              {item.label}
            </Link>
          ))}
        </section>
      </div>
    </div>
  );
}

export function PairPage() {
  return (
    <div className={css.page} style={{ padding: 24 }}>
      <h1 className={css.title}>设备配对</h1>
      <p className={css.hint}>第一里程碑占位。</p>
    </div>
  );
}
