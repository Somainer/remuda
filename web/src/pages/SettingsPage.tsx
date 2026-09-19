import { useEffect, useMemo, useRef, useState } from "react";
import { Link, useLocation, useNavigate } from "react-router-dom";
import {
  iosStandaloneHint,
  readDeviceSettings,
  writeDeviceSettings,
  type DeviceSettings,
  type PermissionDefault,
} from "../features/settings";
import { effortTable, isEmberTier } from "../features/session/effort";
import { defaultPermissionTable } from "../features/session/permissions";
import css from "../features/settings/settings.module.css";
import { readAccessCode, writeAccessCode } from "../lib/accessCode";
import { clipboardIo } from "../lib/clipboard";
import { MORE_NAV } from "../lib/nav";
import { readPushStatus, subscribePush, unsubscribePush, type PushStatus } from "../lib/push";
import {
  readVoiceInputEnabled,
  speechRecognitionSupported,
  writeVoiceInputEnabled,
} from "../lib/speech";
import { defaultPasskeyName, passkeyErrorText } from "../lib/passkeys";
import { hubStore, useHub } from "../lib/store";
import { LoginPage } from "./LoginPage";
import { api } from "../lib/api";
import { TUI_OPTIONS } from "../lib/sessionOptions";
import type { Host, TuiMode } from "../types/instance";
import { hostRegistry } from "../features/hosts/registry";

/* ------------------------------------------------------------------ */
/* Groups — exploration §5 P1-3: 外观与输入 / 通知 / 连接与登录.        */
/* The hash is the deep-link fact; the back control returns to the     */
/* route the operator came from.                                       */
/* ------------------------------------------------------------------ */

const GROUPS = [
  { id: "appearance", label: "外观与输入" },
  { id: "notifications", label: "通知" },
  { id: "connection", label: "连接与登录" },
  { id: "host-defaults", label: "主机默认值" },
] as const;

type GroupId = (typeof GROUPS)[number]["id"];

function isGroupId(value: string | null | undefined): value is GroupId {
  return value === "appearance" || value === "notifications" || value === "connection" || value === "host-defaults";
}

/* ------------------------------------------------------------------ */
/* Theme. tokens.css already carries the night + ledger palettes; the  */
/* switch only chooses one, in this browser.                          */
/* ------------------------------------------------------------------ */

const THEME_KEY = "runtime.theme.v1";

type ThemeChoice = "night" | "ledger";

function readTheme(): ThemeChoice {
  try {
    return localStorage.getItem(THEME_KEY) === "ledger" ? "ledger" : "night";
  } catch {
    return "night";
  }
}

function writeTheme(theme: ThemeChoice): void {
  try {
    localStorage.setItem(THEME_KEY, theme);
  } catch {
    /* storage denied — throw so the save flow reports failure instead of
       claiming the choice persisted. */
    throw new Error("本地存储不可用");
  }
  document.documentElement.dataset.theme = theme;
}

function applyTheme(theme: ThemeChoice): void {
  document.documentElement.dataset.theme = theme;
}

/* ------------------------------------------------------------------ */
/* Per-group draft + explicit save. Local prefs stop pretending to be  */
/* synced, and a failed save rolls the rejected fields back to the     */
/* last committed values (P1-3: 设置失败可恢复旧有效值).               */
/* ------------------------------------------------------------------ */

type SavePhase = "idle" | "saving" | "saved" | "error";

class GroupSaveError extends Error {
  fields: string[];

  constructor(message: string, fields: string[] = []) {
    super(message);
    this.name = "GroupSaveError";
    this.fields = fields;
  }
}

function shallowDiffers(a: Record<string, unknown>, b: Record<string, unknown>): boolean {
  return Object.keys(a).some((key) => a[key] !== b[key]);
}

/**
 * Yield until the browser has actually painted. A localStorage persist settles
 * inside one microtask, which is faster than one frame — without this yield
 * the 保存中 state below would never reach the screen even though the state
 * machine visits it (the 保存中 affordance P1-3 requires must be observable).
 */
function paintFrame(): Promise<void> {
  return new Promise((resolve) => {
    if (typeof requestAnimationFrame !== "function") {
      setTimeout(resolve, 0);
      return;
    }
    requestAnimationFrame(() => requestAnimationFrame(() => resolve()));
  });
}

/**
 * Minimum time the 保存中 state stays on screen. A localStorage persist
 * settles inside a frame, so without a floor the feedback would flash past
 * faster than anyone can read it — P1-3 requires the saving affordance to be
 * shown, not merely visited by the state machine.
 */
const SAVING_MIN_MS = 450;

function useGroupDraft<D extends Record<string, unknown>>(committed: D) {
  const [draft, setDraft] = useState<D>(committed);
  const [phase, setPhase] = useState<SavePhase>("idle");
  const [message, setMessage] = useState<string | null>(null);

  const dirty = useMemo(
    () => shallowDiffers(draft as Record<string, unknown>, committed as Record<string, unknown>),
    [draft, committed],
  );

  const patch = (next: Partial<D>) => {
    setDraft((current) => ({ ...current, ...next }));
    setPhase((current) => (current === "saving" ? current : "idle"));
    setMessage(null);
  };

  const save = async (persist: (next: D) => void | Promise<void>): Promise<boolean> => {
    setPhase("saving");
    setMessage(null);
    await paintFrame();
    const started = Date.now();
    try {
      await persist(draft);
      // Hold the saving state for its minimum readable window.
      const elapsed = Date.now() - started;
      if (elapsed < SAVING_MIN_MS) {
        await new Promise((resolve) => setTimeout(resolve, SAVING_MIN_MS - elapsed));
      }
      setPhase("saved");
      return true;
    } catch (err) {
      const error =
        err instanceof GroupSaveError ? err : new GroupSaveError(err instanceof Error ? err.message : String(err));
      setMessage(error.message);
      setPhase("error");
      // Field-level rollback: named fields return to their last committed
      // value; an error without names rolls the whole group back.
      setDraft((current) => {
        const next = { ...current };
        const fields = error.fields.length ? error.fields : Object.keys(next);
        for (const field of fields) {
          (next as Record<string, unknown>)[field] = (committed as Record<string, unknown>)[field];
        }
        return next;
      });
      return false;
    }
  };

  const reset = () => {
    setDraft(committed);
    setPhase("idle");
    setMessage(null);
  };

  return { draft, patch, dirty, phase, message, save, reset };
}

/**
 * Runner for immediate local preferences (exploration §P1-3: 本地即时偏好).
 * A choice commits the instant it is made — there is no draft or save button
 * in the group — but every commit still walks 保存中 → 已保存 / 失败, and a
 * failed commit leaves the caller free to roll its optimistic value back.
 * Newer edits supersede the status of older ones.
 */
function useCommitRunner() {
  const [phase, setPhase] = useState<SavePhase>("idle");
  const [message, setMessage] = useState<string | null>(null);
  const seq = useRef(0);

  const run = async (persist: () => void | Promise<void>): Promise<boolean> => {
    const mine = ++seq.current;
    setPhase("saving");
    setMessage(null);
    await paintFrame();
    const started = Date.now();
    try {
      await persist();
      const elapsed = Date.now() - started;
      if (elapsed < SAVING_MIN_MS) await new Promise((resolve) => setTimeout(resolve, SAVING_MIN_MS - elapsed));
      if (seq.current === mine) setPhase("saved");
      return true;
    } catch (err) {
      if (seq.current === mine) {
        setMessage(err instanceof Error ? err.message : String(err));
        setPhase("error");
      }
      return false;
    }
  };

  return { phase, message, run };
}

function SaveStatus({ testId, phase, message }: { testId: string; phase: SavePhase; message: string | null }) {  if (phase === "idle") return null;
  const glyph = phase === "saving" ? "◌" : phase === "saved" ? "✓" : "⚠";
  const text =
    phase === "saving" ? "保存中…" : phase === "saved" ? "已保存" : `失败${message ? `：${message}` : "，已恢复上一次的有效值"}`;
  return (
    <p
      className={`${css.status} ${phase === "saving" ? css.statusSaving : phase === "saved" ? css.statusSaved : css.statusError}`}
      role="status"
      data-testid={testId}
      data-phase={phase}
    >
      <span className={phase === "saving" ? css.spin : undefined} aria-hidden="true">
        {glyph}
      </span>
      {text}
    </p>
  );
}

function SaveBar({
  testId,
  dirty,
  phase,
  message,
  onSave,
  onReset,
}: {
  testId: string;
  dirty: boolean;
  phase: SavePhase;
  message: string | null;
  onSave: () => void;
  onReset: () => void;
}) {
  const busy = phase === "saving";
  return (
    <div className={css.saveBar}>
      <button
        type="button"
        className={css.saveBtn}
        data-testid={`${testId}-save`}
        disabled={!dirty || busy}
        onClick={onSave}
      >
        保存
      </button>
      <button
        type="button"
        className={css.resetBtn}
        data-testid={`${testId}-reset`}
        disabled={!dirty || busy}
        onClick={onReset}
      >
        重置
      </button>
      <SaveStatus testId={`${testId}-status`} phase={phase} message={message} />
    </div>
  );
}

/** Host preferences share the grouped explicit-save and rollback contract. */
function HostDefaultsField({ host }: { host: Host }) {
  const [confirmedTui, setConfirmedTui] = useState<TuiMode>(host.defaultTui ?? "fullscreen");
  const defaults = useGroupDraft({ defaultTui: confirmedTui });
  const testId = `settings-host-defaults-${host.id}`;

  const save = () => defaults.save(async ({ defaultTui }) => {
    const saved = await api.hostPatch(host.id, { defaultTui });
    const confirmed = saved.defaultTui ?? "fullscreen";
    setConfirmedTui(confirmed);
    hostRegistry.patch(host.id, { defaultTui: saved.defaultTui });
    // The PATCH response confirms persistence. A failed follow-up refresh
    // must not relabel an accepted write as a rejected field value.
    await hubStore.refreshHosts().catch(() => undefined);
  });

  return (
    <div className={css.section} data-testid={testId}>
      <label className={css.field}>
        {host.label} · Claude 默认终端渲染
        <select
          className={css.input}
          data-testid={`settings-host-tui-${host.id}`}
          value={defaults.draft.defaultTui}
          disabled={defaults.phase === "saving"}
          onChange={(event) => defaults.patch({ defaultTui: event.target.value as TuiMode })}
        >
          {TUI_OPTIONS.map((option) => <option key={option.id} value={option.id}>{option.label}</option>)}
        </select>
      </label>
      <SaveBar
        testId={testId}
        dirty={defaults.dirty}
        phase={defaults.phase}
        message={defaults.message}
        onSave={() => void save()}
        onReset={defaults.reset}
      />
    </div>
  );
}

function GroupHeading({ id, label, hint }: { id: GroupId; label: string; hint?: string }) {
  return (
    <header className={css.groupHead}>
      <h2 className={css.groupTitle} id={`${id}-title`}>
        {label}
      </h2>
      {hint ? <p className={css.localNote}>{hint}</p> : null}
    </header>
  );
}

/* ------------------------------------------------------------------ */
/* Passkeys — the D-030 section keeps its testids and behaviour.       */
/* ------------------------------------------------------------------ */

function formatPasskeyTime(iso: string | null | undefined): string {
  if (!iso) return "从未使用";
  const t = Date.parse(iso);
  if (!Number.isFinite(t)) return "—";
  return new Date(t).toLocaleString([], {
    year: "numeric",
    month: "numeric",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });
}

function PasskeysSection() {
  const hub = useHub();
  const [name, setName] = useState(() => defaultPasskeyName());
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const supported = hubStore.passkeysSupported();

  const run = (action: () => Promise<unknown>) => {
    setBusy(true);
    setError(null);
    void action()
      .catch((err: unknown) => setError(passkeyErrorText(err)))
      .finally(() => setBusy(false));
  };

  return (
    <section className={css.section} data-testid="settings-passkeys">
      <div className={css.label}>Passkeys</div>
      <p className={css.hint}>
        用 Passkey 免访问码登录。Passkey 与来源绑定：内网地址与本地回环地址是两套凭据，需要分别注册。
      </p>
      {supported ? (
        <>
          <label className={css.field}>
            名称
            <input
              className={css.input}
              data-testid="settings-passkey-name"
              value={name}
              maxLength={64}
              onChange={(e) => setName(e.target.value)}
            />
          </label>
          <div className={css.row}>
            <button
              type="button"
              className={css.action}
              data-testid="settings-passkey-add"
              disabled={busy}
              onClick={() =>
                run(async () => {
                  const saved = await hubStore.addPasskey(name.trim() || defaultPasskeyName());
                  setName(saved.name);
                })
              }
            >
              添加 Passkey
            </button>
          </div>
        </>
      ) : (
        <p className={css.hint} data-testid="settings-passkey-unsupported">
          当前环境不支持注册 Passkey（非安全来源或浏览器过旧）。
        </p>
      )}
      {error ? (
        <p className={css.hint} data-testid="settings-passkey-error">
          {error}
        </p>
      ) : null}
      {hub.passkeys.map((passkey) => (
        <div key={passkey.id} className={css.deviceRow} data-testid="settings-passkey-row">
          <div className={css.deviceName}>
            {passkey.name}
            {passkey.thisDevice ? <span className={css.you}> 本机</span> : null}
            <div className={css.deviceId}>
              创建 {formatPasskeyTime(passkey.createdAt)} · 最近使用 {formatPasskeyTime(passkey.lastUsedAt)}
            </div>
          </div>
          <button
            type="button"
            className={css.action}
            data-testid="settings-passkey-rename"
            disabled={busy}
            onClick={() => {
              const next = window.prompt("Passkey 名称", passkey.name);
              if (next && next.trim() && next.trim() !== passkey.name) {
                run(() => hubStore.renamePasskey(passkey.id, next.trim()));
              }
            }}
          >
            重命名
          </button>
          <button
            type="button"
            className={`${css.action} ${css.danger}`}
            data-testid="settings-passkey-delete"
            disabled={busy}
            onClick={() => {
              if (window.confirm(`删除 Passkey「${passkey.name}」？仍可用访问码重新登录。`)) {
                run(() => hubStore.deletePasskey(passkey.id));
              }
            }}
          >
            删除
          </button>
        </div>
      ))}
    </section>
  );
}

const PERMS: { id: PermissionDefault; label: string; native: string; description: string }[] =
  defaultPermissionTable()
    .filter((option) =>
      (["manual", "acceptEdits", "plan", "auto"] as const).includes(
        option.id as PermissionDefault,
      ),
    )
    .map((option) => ({
      id: option.id as PermissionDefault,
      label: option.label,
      native: option.native,
      description: option.description,
    }));

/* ------------------------------------------------------------------ */
/* Page                                                                */
/* ------------------------------------------------------------------ */

export function SettingsPage() {
  const hub = useHub();
  const navigate = useNavigate();
  const location = useLocation();
  const [access, setAccess] = useState(() => readAccessCode());
  const [settings, setSettings] = useState(() => readDeviceSettings());
  const [theme, setTheme] = useState<ThemeChoice>(() => readTheme());
  const [push, setPush] = useState<PushStatus>(() => ({
    permission: typeof Notification === "undefined" ? "unsupported" : Notification.permission,
    subscribed: false,
    endpoint: null,
    needsHomeScreen: false,
  }));
  const [pushPhase, setPushPhase] = useState<SavePhase>("idle");
  const [pushMessage, setPushMessage] = useState<string | null>(null);
  const [pairBusy, setPairBusy] = useState(false);
  // §4.8 voice input: off by default, per device. The capability fact never
  // changes for a browser, so it is probed once on mount; iPhone Safari has
  // no SpeechRecognition and gets an explanation instead of a dead switch.
  const [voiceSupported] = useState(() => speechRecognitionSupported());
  const [voiceEnabled, setVoiceEnabled] = useState(() => readVoiceInputEnabled());

  useEffect(() => {
    applyTheme(readTheme());
  }, []);

  useEffect(() => {
    void readPushStatus().then(setPush);
  }, []);

  // Deep link: reveal the named group whenever the hash lands on a known id.
  const hashId = decodeURIComponent(location.hash.slice(1));
  const activeGroup = isGroupId(hashId) ? hashId : null;
  useEffect(() => {
    if (!activeGroup) return;
    const reduced = window.matchMedia?.("(prefers-reduced-motion: reduce)").matches;
    const element = document.getElementById(activeGroup);
    // Optional call: jsdom has no scrolling implementation.
    element?.scrollIntoView?.({ block: "start", behavior: reduced ? "auto" : "smooth" });
    // Move focus too, so the jump is real for assistive tech; preventScroll so
    // the call above owns placement.
    element?.focus({ preventScroll: true });
  }, [activeGroup]);

  /* Group 1 — 外观与输入: immediate local prefs (本地即时偏好). */
  const appearance = useCommitRunner();

  /* Group 3 — 连接与登录 identity fields: explicit save (a name/code is text
     the operator expects to review before it persists). */
  const identity = useGroupDraft({
    deviceName: settings.deviceName,
    accessCode: access,
  });

  const goBack = () => {
    // history.state.idx is populated by the React Router history; a direct
    // deep link lands at idx 0, in which case -1 has nowhere to go and the
    // sessions list is the stable origin instead.
    const idx = (window.history.state as { idx?: number } | null)?.idx;
    if (typeof idx === "number" && idx > 0) navigate(-1);
    else navigate("/sessions");
  };

  const chooseTheme = (choice: ThemeChoice) =>
    appearance.run(() => {
      // Optimistic so the selection reads instantly; a denied write restores
      // the last committed theme and surfaces 失败.
      setTheme(choice);
      applyTheme(choice);
      try {
        writeTheme(choice);
      } catch (err) {
        setTheme(readTheme());
        applyTheme(readTheme());
        throw err;
      }
    });

  const commitDevicePrefs = (patch: Partial<DeviceSettings>) =>
    appearance.run(() => {
      setSettings((current) => ({ ...current, ...patch }));
      try {
        writeDeviceSettings(patch);
        setSettings(readDeviceSettings());
      } catch (err) {
        // Roll the rejected fields back to their last committed values.
        setSettings(readDeviceSettings());
        throw err;
      }
    });

  const toggleCompact = () => hubStore.setCompact(!hub.compact);

  const commitVoice = (enabled: boolean) =>
    appearance.run(() => {
      writeVoiceInputEnabled(enabled);
      // Re-read the stored value: with storage denied it stays off, so the
      // checkbox never shows a choice that did not persist.
      setVoiceEnabled(readVoiceInputEnabled());
    });

  const saveIdentity = () =>
    identity.save((next) => {
      const name = next.deviceName.trim();
      if (!name) throw new GroupSaveError("设备名不能为空", ["deviceName"]);
      try {
        writeDeviceSettings({ deviceName: name });
        writeAccessCode(next.accessCode);
      } catch {
        throw new GroupSaveError("浏览器存储不可用，设置未保存", ["deviceName", "accessCode"]);
      }
      setAccess(next.accessCode);
      setSettings(readDeviceSettings());
    });

  const togglePush = async () => {
    setPushPhase("saving");
    setPushMessage(null);
    try {
      if (push.subscribed) await unsubscribePush();
      else await subscribePush();
      setPush(await readPushStatus());
      setPushPhase("saved");
    } catch (err) {
      setPushMessage(err instanceof Error ? err.message : String(err));
      setPushPhase("error");
      // Rollback: re-read the source of truth so the button never shows the
      // state the server does not have.
      setPush(await readPushStatus());
    }
  };

  return (
    <div className={css.page} data-testid="settings-page">
      <header className={css.head}>
        <button type="button" className={css.back} data-testid="settings-back" onClick={goBack}>
          ← 返回
        </button>
        <h1 className={css.title}>设置</h1>
      </header>
      <div className={css.layout}>
        <nav className={css.nav} aria-label="设置分组" data-testid="settings-nav">
          {GROUPS.map((group) => (
            <Link
              key={group.id}
              to={`/settings#${group.id}`}
              className={css.navLink}
              data-testid={`settings-nav-${group.id}`}
              aria-current={activeGroup === group.id ? "true" : undefined}
            >
              {group.label}
            </Link>
          ))}
        </nav>

        <div className={css.body}>
          <section
            id="appearance"
            className={css.group}
            data-testid="settings-group-appearance"
            tabIndex={-1}
            aria-labelledby="appearance-title"
          >
            <GroupHeading id="appearance" label="外观与输入" hint="本分组偏好只保存在此浏览器，不会同步；选择即时生效。" />

            <div className={css.section}>
              <div className={css.label}>主题</div>
              <p className={css.hint} data-testid="settings-theme">
                双主题：Night Corral（深色）与 Ledger（浅色）。字体与 IBM Plex 不变。
              </p>
              <div className={css.row}>
                {(["night", "ledger"] as const).map((choice) => (
                  <button
                    key={choice}
                    type="button"
                    className={`${css.chip} ${theme === choice ? css.chipOn : ""}`}
                    data-testid={`settings-theme-${choice}`}
                    aria-pressed={theme === choice}
                    onClick={() => void chooseTheme(choice)}
                  >
                    {choice === "night" ? "Night Corral" : "Ledger（浅色）"}
                  </button>
                ))}
              </div>
            </div>

            <div className={css.section}>
              <div className={css.label}>工作台密度</div>
              <div className={css.row}>
                <button
                  type="button"
                  className={css.action}
                  data-testid="settings-compact"
                  aria-pressed={hub.compact}
                  onClick={toggleCompact}
                >
                  Compact {hub.compact ? "开" : "关"}
                </button>
              </div>
            </div>

            <div className={css.section}>
              <div className={css.label}>权限默认</div>
              <p className={css.hint}>新建会话的默认权限（Claude 真实模式：default · acceptEdits · plan · auto）。</p>
              <div className={css.row} data-testid="settings-permission">
                {PERMS.map((opt) => (
                  <button
                    key={opt.id}
                    type="button"
                    className={`${css.chip} ${settings.permissionDefault === opt.id ? css.chipOn : ""}`}
                    data-testid={`settings-perm-${opt.id}`}
                    aria-pressed={settings.permissionDefault === opt.id}
                    onClick={() => void commitDevicePrefs({ permissionDefault: opt.id })}
                  >
                    <span className={css.chipLabel}>{opt.label}</span>
                    <span className={css.chipNative}>{opt.native}</span>
                  </button>
                ))}
              </div>
            </div>

            <div className={css.section}>
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
                      aria-pressed={on}
                      onClick={() => void commitDevicePrefs({ defaultEffortIndex: index })}
                    >
                      {tier.name}
                    </button>
                  );
                })}
              </div>
            </div>

            <div className={css.section}>
              <div className={css.label}>终端</div>
              <label className={css.check}>
                <input
                  type="checkbox"
                  data-testid="settings-auto-reveal-tty"
                  checked={settings.autoRevealTty}
                  onChange={(e) => void commitDevicePrefs({ autoRevealTty: e.target.checked })}
                />
                自动切 tty（autoRevealTty）
              </label>
              <p className={css.hint}>默认关。打开后仍须 capabilities.artifact；M3 才考虑生产打开。</p>
            </div>

            <div className={css.section}>
              <div className={css.label}>语音输入</div>
              <p className={css.hint} data-testid="settings-voice-copy">
                默认用系统键盘自带的听写：Remuda 不录音、不上传音频、不做云端转写，识别出的文字只进入输入框，听写中途绝不自动发送。浏览器增强（SpeechRecognition）默认关，只在浏览器支持时显示麦克风按钮，音频是否离开设备由该浏览器决定，不经过 Remuda 的 Hub / Node。
              </p>
              <p className={css.hint}>
                iOS Safari 没有 SpeechRecognition（WebKit 未实现）。iPhone 上请用系统键盘的听写按钮；把 Remuda 加到主屏幕后的标准 PWA 里键盘听写可用。终端段不提供语音，要说话请切到结构段。
              </p>
              <label className={css.check}>
                <input
                  type="checkbox"
                  data-testid="settings-voice-input"
                  checked={voiceEnabled}
                  disabled={!voiceSupported}
                  onChange={(e) => void commitVoice(e.target.checked)}
                />
                在支持 SpeechRecognition 的浏览器显示麦克风按钮（默认关）
              </label>
              {!voiceSupported ? (
                <p className={css.hint} data-testid="settings-voice-unsupported">
                  此浏览器没有 SpeechRecognition，开关不可用且不会显示麦克风；iPhone 请用系统键盘听写。
                </p>
              ) : null}
            </div>

            <SaveStatus testId="settings-appearance-status" phase={appearance.phase} message={appearance.message} />
          </section>

          <section
            id="notifications"
            className={css.group}
            data-testid="settings-group-notifications"
            tabIndex={-1}
            aria-labelledby="notifications-title"
          >
            <GroupHeading id="notifications" label="通知" hint="推送权限与订阅状态由浏览器和 Hub 共同决定。" />
            <div className={css.section}>
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
                  disabled={pushPhase === "saving"}
                  onClick={() => void togglePush()}
                >
                  {push.subscribed ? "关闭推送" : "开启推送"}
                </button>
              </div>
              <SaveStatus testId="settings-notifications-status" phase={pushPhase} message={pushMessage} />
            </div>
          </section>

          <section
            id="connection"
            className={css.group}
            data-testid="settings-group-connection"
            tabIndex={-1}
            aria-labelledby="connection-title"
          >
            <GroupHeading id="connection" label="连接与登录" hint="设备名与访问码保存在此浏览器；凭据管理在 Hub 生效。" />

            <div className={css.section}>
              <div className={css.label}>设备</div>
              <label className={css.field}>
                设备名
                <input
                  className={css.input}
                  data-testid="settings-device-name"
                  value={identity.draft.deviceName}
                  onChange={(e) => identity.patch({ deviceName: e.target.value })}
                />
              </label>
              <label className={css.field}>
                remuda dev 访问码
                <input
                  className={css.input}
                  type="password"
                  value={identity.draft.accessCode}
                  onChange={(e) => identity.patch({ accessCode: e.target.value })}
                />
              </label>
              <p className={css.hint}>请求头 X-Remuda-Access-Code。也可用 VITE_ACCESS_CODE。不要把长期 token 放进 WebSocket URL。</p>
              <div className={css.row}>
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
              <SaveBar
                testId="settings-identity"
                dirty={identity.dirty}
                phase={identity.phase}
                message={identity.message}
                onSave={() => void saveIdentity()}
                onReset={identity.reset}
              />
            </div>

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

            <PasskeysSection />

            <div className={css.section}>
              <div className={css.label}>更多管理</div>
              {MORE_NAV.map((item) => (
                <Link key={item.id} to={item.to} className={css.link}>
                  {item.label}
                </Link>
              ))}
            </div>
          </section>

          <section
            id="host-defaults"
            className={css.group}
            data-testid="settings-group-host-defaults"
            tabIndex={-1}
            aria-labelledby="host-defaults-title"
          >
            <GroupHeading id="host-defaults" label="主机默认值" hint="保存在 Hub，供该主机的新会话使用；新建会话中的选择优先。" />
            {hub.hosts.map((host) => <HostDefaultsField key={host.id} host={host} />)}
            {!hub.hosts.length ? <p className={css.hint}>添加主机后可设置。</p> : null}
          </section>
        </div>
      </div>
    </div>
  );
}

export function PairPage() {
  return <LoginPage mode="pair" />;
}
