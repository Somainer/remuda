import { useEffect, useRef, useState } from "react";
import { Navigate, useLocation, useNavigate, useSearchParams } from "react-router-dom";
import { readDeviceSettings } from "../features/settings/prefs";
import { passkeyErrorText } from "../lib/passkeys";
import { hubStore, useHub } from "../lib/store";
import { useWorkbenchViewport } from "../lib/viewport";
import { PageHeader } from "../components/PageHeader";
import css from "./LoginPage.module.css";

export function LoginPage({ mode }: { mode?: "bootstrap" | "pair" }) {
  const hub = useHub();
  const navigate = useNavigate();
  const location = useLocation();
  const [params] = useSearchParams();
  const { mobile } = useWorkbenchViewport();
  const from = (location.state as { from?: string } | null)?.from;
  const pairDefault = mode === "pair";
  const [tab, setTab] = useState<"bootstrap" | "pair">(
    pairDefault ? "pair" : (params.get("code") || params.get("pair") != null || mobile ? "pair" : "bootstrap"),
  );
  const [token, setToken] = useState(() => import.meta.env.VITE_ACCESS_CODE ?? "");
  const [code, setCode] = useState((params.get("code") ?? "").toUpperCase());
  const [deviceName, setDeviceName] = useState(readDeviceSettings().deviceName);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const [passkeySupported] = useState(() => hubStore.passkeysSupported());
  const [passkeyBusy, setPasskeyBusy] = useState(false);
  const [conditionalReady, setConditionalReady] = useState(false);
  const [showCodes, setShowCodes] = useState(!passkeySupported || pairDefault);
  // Any pending get() ceremony (conditional autofill or explicit button);
  // aborted before another ceremony or an access-code submit starts.
  const ceremonyAbort = useRef<AbortController | null>(null);

  const destination = () => (from && from !== "/login" ? from : "/sessions");

  useEffect(() => {
    if (!passkeySupported || pairDefault) return;
    let cancelled = false;
    const controller = new AbortController();
    ceremonyAbort.current = controller;
    void hubStore
      .conditionalMediationAvailable()
      .then((available) => {
        if (cancelled || !available) return;
        setConditionalReady(true);
        return hubStore.passkeyLogin(
          "conditional",
          readDeviceSettings().deviceName.trim() || undefined,
          controller.signal,
        );
      })
      .then(() => {
        if (!cancelled && hubStore.stateAuthed()) navigate(destination(), { replace: true });
      })
      .catch(() => {
        // Conditional mediation is an opportunistic autofill hint. It rejects
        // whenever no discoverable credential exists; the explicit button is
        // the authoritative path, so never surface its errors.
      });
    return () => {
      cancelled = true;
      controller.abort();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  if (hub.ready && hub.authed) {
    return <Navigate to={destination()} replace />;
  }

  const abortCeremonies = () => {
    ceremonyAbort.current?.abort();
    ceremonyAbort.current = null;
    setConditionalReady(false);
  };

  const passkeySubmit = () => {
    if (passkeyBusy) return;
    abortCeremonies();
    const controller = new AbortController();
    ceremonyAbort.current = controller;
    setPasskeyBusy(true);
    setError(null);
    void hubStore
      .passkeyLogin("required", deviceName.trim() || undefined, controller.signal)
      .then(() => navigate(destination(), { replace: true }))
      .catch((err: unknown) => {
        if (!controller.signal.aborted) setError(passkeyErrorText(err));
      })
      .finally(() => setPasskeyBusy(false));
  };

  const submit = () => {
    const secret = tab === "pair" ? code.trim() : token.trim();
    if (!secret || busy) return;
    abortCeremonies();
    setBusy(true);
    setError(null);
    void hubStore
      .login(tab, secret, deviceName.trim() || (tab === "pair" ? "phone" : "device"))
      .then(() => navigate(destination(), { replace: true }))
      .catch((err: unknown) => setError(err instanceof Error ? err.message : "登录失败"))
      .finally(() => setBusy(false));
  };

  return (
    <div className={css.page} data-testid="login-page" data-mode={tab}>
      <PageHeader
        testId="login-head"
        crumbs={pairDefault ? [{ label: "登录", to: "/login" }] : []}
        title={pairDefault ? "手机配对" : "Remuda"}
      />
      <div className={css.body}>
        <form
          className={css.card}
          onSubmit={(e) => {
            e.preventDefault();
            submit();
          }}
        >
          <p className={css.hint}>
            {tab === "pair" ? "手机用配对码加入已登录的设备。" : "用 Passkey 直接登录，或首次用访问码进入。"}
          </p>
          <div>
          {passkeySupported ? (
            <section className={css.passkey} data-testid="login-passkey">
              <button
                type="button"
                className={css.passButton}
                data-testid="login-passkey-submit"
                disabled={passkeyBusy}
                onClick={passkeySubmit}
                autoFocus={!pairDefault}
              >
                {passkeyBusy ? "等待验证设备…" : "使用 Passkey 登录"}
              </button>
              {/* Conditional mediation: the browser fills this slot with
                  passkey suggestions; keep it focusable but visually quiet. */}
              <input
                className={css.autofill}
                data-testid="login-passkey-autofill"
                type="text"
                autoComplete="username webauthn"
                placeholder="选择已保存的 Passkey…"
                aria-label="Passkey 自动填充"
              />
              <p className={css.hint}>
                Passkey 与当前来源绑定：内网地址与本地地址需分别注册。
                {conditionalReady ? "也可在用户名自动填充里直接选择 Passkey。" : null}
              </p>
            </section>
          ) : (
            <p className={css.hint} data-testid="login-passkey-unsupported">
              当前环境不支持 Passkey（非安全来源或浏览器过旧），请使用访问码登录。
            </p>
          )}

          {!showCodes ? (
            <button
              type="button"
              className={css.toggle}
              data-testid="login-use-code"
              onClick={() => {
                abortCeremonies();
                setShowCodes(true);
              }}
            >
              使用访问码
            </button>
          ) : (
            <section className={css.codes} data-testid="login-codes">
              <div className={css.tabs} role="tablist" aria-label="登录方式">
                <button
                  type="button"
                  role="tab"
                  className={css.tab}
                  aria-selected={tab === "bootstrap"}
                  data-testid="login-tab-bootstrap"
                  onClick={() => setTab("bootstrap")}
                >
                  首次启动
                </button>
                <button
                  type="button"
                  role="tab"
                  className={css.tab}
                  aria-selected={tab === "pair"}
                  data-testid="login-tab-pair"
                  onClick={() => setTab("pair")}
                >
                  手机配对
                </button>
              </div>
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
              <button type="submit" className={css.submit} disabled={busy} data-testid="login-submit">
                {busy ? "登录中" : tab === "pair" ? "用配对码进入" : "用访问码进入"}
              </button>
            </section>
          )}
          {error ? (
            <p className={css.error} data-testid="login-error">
              {error}
            </p>
          ) : null}
          </div>
        </form>
      </div>
    </div>
  );
}
