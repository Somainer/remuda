import { useState } from "react";
import { Navigate, useLocation, useNavigate, useSearchParams } from "react-router-dom";
import { readDeviceSettings } from "../features/settings/prefs";
import { hubStore, useHub } from "../lib/store";
import { useWorkbenchViewport } from "../lib/viewport";
import css from "./LoginPage.module.css";

export function LoginPage({ mode }: { mode?: "bootstrap" | "pair" }) {
  const hub = useHub();
  const navigate = useNavigate();
  const location = useLocation();
  const [params] = useSearchParams();
  const { mobile } = useWorkbenchViewport();
  const from = (location.state as { from?: string } | null)?.from;
  const pref = mode ?? (params.get("code") || params.get("pair") != null || mobile ? "pair" : "bootstrap");
  const [tab, setTab] = useState<"bootstrap" | "pair">(pref);
  const [token, setToken] = useState("");
  const [code, setCode] = useState((params.get("code") ?? "").toUpperCase());
  const [deviceName, setDeviceName] = useState(readDeviceSettings().deviceName);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  if (hub.ready && hub.authed) {
    return <Navigate to={from && from !== "/login" ? from : "/sessions"} replace />;
  }

  const submit = () => {
    const secret = tab === "pair" ? code.trim() : token.trim();
    if (!secret || busy) return;
    setBusy(true);
    setError(null);
    void hubStore
      .login(tab, secret, deviceName.trim() || (tab === "pair" ? "phone" : "device"))
      .then(() => navigate(from && from !== "/login" ? from : "/sessions", { replace: true }))
      .catch((err: unknown) => setError(err instanceof Error ? err.message : "登录失败"))
      .finally(() => setBusy(false));
  };

  return (
    <div className={css.page} data-testid="login-page" data-mode={tab}>
      <form
        className={css.card}
        onSubmit={(e) => {
          e.preventDefault();
          submit();
        }}
      >
        <header className={css.head}>
          <h1 className={css.title}>runtime</h1>
          <p className={css.hint}>
            {tab === "pair" ? "手机用配对码加入已登录的设备。" : "首次启动：用 Hub 的 bootstrap token 换设备 cookie。"}
          </p>
        </header>
        <div className={css.tabs}>
          <button
            type="button"
            className={`${css.tab} ${tab === "bootstrap" ? css.tabOn : ""}`}
            data-testid="login-tab-bootstrap"
            onClick={() => setTab("bootstrap")}
          >
            首次启动
          </button>
          <button
            type="button"
            className={`${css.tab} ${tab === "pair" ? css.tabOn : ""}`}
            data-testid="login-tab-pair"
            onClick={() => setTab("pair")}
          >
            手机配对
          </button>
        </div>
        <div className={css.body}>
          {tab === "bootstrap" ? (
            <label className={css.field}>
              <span className={css.label}>bootstrap token</span>
              <input
                className={css.input}
                data-testid="login-bootstrap-token"
                type="password"
                autoComplete="off"
                value={token}
                onChange={(e) => setToken(e.target.value)}
              />
            </label>
          ) : (
            <label className={css.field}>
              <span className={css.label}>配对码</span>
              <input
                className={`${css.input} ${css.mono}`}
                data-testid="login-pair-code"
                inputMode="text"
                autoCapitalize="characters"
                autoComplete="off"
                maxLength={8}
                value={code}
                onChange={(e) => setCode(e.target.value.toUpperCase())}
              />
            </label>
          )}
          <label className={css.field}>
            <span className={css.label}>设备名</span>
            <input
              className={css.input}
              data-testid="login-device-name"
              value={deviceName}
              onChange={(e) => setDeviceName(e.target.value)}
            />
          </label>
          {error ? (
            <p className={css.error} data-testid="login-error">
              {error}
            </p>
          ) : null}
          <button type="submit" className={css.submit} disabled={busy} data-testid="login-submit">
            {busy ? "登录中" : tab === "pair" ? "用配对码进入" : "进入"}
          </button>
        </div>
      </form>
    </div>
  );
}
