import { useState } from "react";
import { Link } from "react-router-dom";
import { Button } from "../components/Button";
import { iosStandaloneHint, readDeviceSettings, writeDeviceSettings, type PermissionDefault } from "../features/settings";
import { readAccessCode, writeAccessCode } from "../lib/accessCode";
import { MORE_NAV } from "../lib/nav";
import { subscribePush } from "../lib/push";
import { hubStore, useHub } from "../lib/store";
import ui from "../styles/ui.module.css";

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
    <div style={{ padding: 16 }} data-testid="settings-page">
      <h1 style={{ fontSize: 18 }}>设置</h1>

      <h2 style={{ fontSize: 14 }}>设备</h2>
      <label className={ui.field} style={{ margin: "8px 0" }}>
        设备名
        <input
          className={ui.input}
          data-testid="settings-device-name"
          value={settings.deviceName}
          onChange={(e) => patch({ deviceName: e.target.value })}
        />
      </label>
      <label className={ui.field} style={{ margin: "12px 0" }}>
        remuda dev 访问码
        <input
          className={ui.input}
          type="password"
          value={access}
          onChange={(e) => {
            setAccess(e.target.value);
            writeAccessCode(e.target.value);
          }}
        />
      </label>
      <p className={ui.listMeta}>请求头 X-Remuda-Access-Code。也可用 VITE_ACCESS_CODE。不要把长期 token 放进 WebSocket URL。</p>
      <div className={ui.row} style={{ margin: "12px 0" }}>
        <Button onClick={() => hubStore.setCompact(!hub.compact)}>Compact {hub.compact ? "开" : "关"}</Button>
      </div>

      <h2 style={{ fontSize: 14 }}>推送</h2>
      <p className={ui.listMeta} data-testid="settings-ios-hint">
        {iosStandaloneHint()}
      </p>
      <p className={ui.listMeta}>当前权限 {push}</p>
      <div className={ui.row} style={{ margin: "12px 0" }}>
        <Button
          data-testid="settings-push"
          onClick={() => {
            void subscribePush().then((result) => {
              setPush(result.ok ? "granted" : typeof Notification === "undefined" ? "unsupported" : Notification.permission);
            });
          }}
        >
          开启推送
        </Button>
      </div>

      <h2 style={{ fontSize: 14 }}>主题</h2>
      <p className={ui.listMeta} data-testid="settings-theme">
        Night Corral。v1 无浅色开关。
      </p>
      <div className={ui.row} style={{ margin: "8px 0" }}>
        <button type="button" className={`${ui.chip} ${ui.chipOn}`} disabled>
          Night Corral
        </button>
        <button type="button" className={ui.chip} disabled>
          浅色（v1 不做）
        </button>
      </div>

      <h2 style={{ fontSize: 14 }}>权限默认</h2>
      <p className={ui.listMeta}>新建会话的默认 permissionMode。全自动仅限本人遥控。</p>
      <div className={ui.row} style={{ margin: "8px 0" }} data-testid="settings-permission">
        {PERMS.map((opt) => (
          <button
            key={opt.id}
            type="button"
            className={`${ui.chip} ${settings.permissionDefault === opt.id ? ui.chipOn : ""}`}
            data-testid={`settings-perm-${opt.id}`}
            onClick={() => patch({ permissionDefault: opt.id })}
          >
            {opt.label}
          </button>
        ))}
      </div>

      <h2 style={{ fontSize: 14 }}>终端</h2>
      <label className={ui.row} style={{ margin: "8px 0" }}>
        <input
          type="checkbox"
          data-testid="settings-auto-reveal-tty"
          checked={settings.autoRevealTty}
          onChange={(e) => patch({ autoRevealTty: e.target.checked })}
        />
        自动切 tty（autoRevealTty）
      </label>
      <p className={ui.listMeta}>默认关。打开后仍须 capabilities.artifact；M3 才考虑生产打开。</p>

      <h2 style={{ fontSize: 14 }}>更多</h2>
      {MORE_NAV.map((item) => (
        <Link key={item.id} to={item.to} className={ui.listItem}>
          {item.label}
        </Link>
      ))}
    </div>
  );
}

export function PairPage() {
  return (
    <div style={{ padding: 24 }}>
      <h1>设备配对</h1>
      <p className={ui.listMeta}>第一里程碑占位。</p>
    </div>
  );
}
