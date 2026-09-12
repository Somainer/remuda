import { useEffect, useState } from "react";
import { Link, useNavigate } from "react-router-dom";
import { iosStandaloneHint, readDeviceSettings, writeDeviceSettings, type PermissionDefault } from "../features/settings";
import { effortTable, isEmberTier } from "../features/session/effort";
import css from "../features/settings/settings.module.css";
import { readAccessCode, writeAccessCode } from "../lib/accessCode";
import { clipboardIo } from "../lib/clipboard";
import { MORE_NAV } from "../lib/nav";
import { readPushStatus, subscribePush, unsubscribePush, type PushStatus } from "../lib/push";
import { hubStore, useHub } from "../lib/store";
import { LoginPage } from "./LoginPage";

const PERMS: { id: PermissionDefault; label: string }[] = [
  { id: "manual", label: "询问" },
  { id: "acceptEdits", label: "可改文件" },
  { id: "dontAsk", label: "全自动" },
];

export function SettingsPage() {
  const hub = useHub();
  const navigate = useNavigate();
  const [access, setAccess] = useState(() => readAccessCode());
  const [settings, setSettings] = useState(() => readDeviceSettings());
  const [push, setPush] = useState<PushStatus>(() => ({
    permission: typeof Notification === "undefined" ? "unsupported" : Notification.permission,
    subscribed: false,
    endpoint: null,
    needsHomeScreen: false,
  }));
  const [pairBusy, setPairBusy] = useState(false);

  useEffect(() => {
    void readPushStatus().then(setPush);
  }, []);

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
            <button
              type="button"
              className={css.action}
              data-testid="settings-logout"
              onClick={() => {
                hubStore.logout();
                navigate("/login", { replace: true });
              }}
            >
              退出登录
            </button>
          </div>
        </section>

        <section className={css.section} data-testid="settings-devices">
          <div className={css.label}>已配对设备</div>
          <p className={css.hint}>撤销会立刻作废该设备的 cookie。本设备撤销等于退出。</p>
          {hub.devices.map((device) => {
            const mine = device.id === hub.session?.deviceId;
            return (
              <div key={device.id} className={css.deviceRow} data-testid="settings-device-row">
                <div className={css.deviceName}>
                  {device.name}
                  {mine ? <span className={css.you}> 本机</span> : null}
                  <div className={css.deviceId}>{device.id.slice(0, 12)}</div>
                </div>
                <button
                  type="button"
                  className={`${css.action} ${css.danger}`}
                  data-testid="settings-revoke"
                  onClick={() => {
                    void hubStore.revokeDevice(device.id).then(() => {
                      if (mine) navigate("/login", { replace: true });
                    });
                  }}
                >
                  撤销
                </button>
              </div>
            );
          })}
          <div className={css.row}>
            <button
              type="button"
              className={css.action}
              data-testid="settings-pair-code"
              disabled={pairBusy}
              onClick={() => {
                setPairBusy(true);
                void hubStore
                  .issuePairCode()
                  .then((issued) => clipboardIo.write(issued.code))
                  .finally(() => setPairBusy(false));
              }}
            >
              生成配对码
            </button>
          </div>
          {hub.pairCode ? (
            <div>
              <div className={css.pairValue} data-testid="settings-pair-code-value">
                {hub.pairCode.code}
              </div>
              <p className={css.hint}>10 分钟内有效 · 手机打开 /login 选「手机配对」</p>
            </div>
          ) : null}
        </section>

        <section className={css.section}>
          <div className={css.label}>推送</div>
          <p className={css.hint} data-testid="settings-ios-hint">
            {iosStandaloneHint()}
          </p>
          <p className={css.hint} data-testid="settings-push-state">
            当前权限 {push.permission}
            {push.subscribed ? " · 已订阅" : " · 未订阅"}
            {push.needsHomeScreen ? " · 需加到主屏幕" : ""}
          </p>
          <div className={css.row}>
            <button
              type="button"
              className={css.action}
              data-testid="settings-push"
              aria-pressed={push.subscribed}
              onClick={() => {
                void (push.subscribed ? unsubscribePush() : subscribePush()).then(() => readPushStatus().then(setPush));
              }}
            >
              {push.subscribed ? "关闭推送" : "开启推送"}
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
          <div className={css.label}>默认 effort</div>
          <p className={css.hint}>存序号，换 harness 后按新表就近映射。档位名保持英文。最高档为 ember。</p>
          <div className={css.row} data-testid="settings-effort">
            {effortTable("claude").map((tier, index) => {
              const on = settings.defaultEffortIndex === index;
              const top = isEmberTier("claude", index);
              return (
                <button
                  key={tier.name}
                  type="button"
                  className={`${css.chip} ${on ? css.chipOn : ""} ${top ? css.chipEmber : ""}`}
                  data-testid={`settings-effort-${tier.name}`}
                  data-ember={top ? "1" : "0"}
                  data-selected={on ? "1" : "0"}
                  onClick={() => patch({ defaultEffortIndex: index })}
                >
                  {tier.name}
                </button>
              );
            })}
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
  return <LoginPage mode="pair" />;
}
