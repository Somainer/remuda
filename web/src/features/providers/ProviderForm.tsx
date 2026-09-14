import { useState } from "react";
import { Button } from "../../components/Button";
import ui from "../../styles/ui.module.css";
import css from "./providers.module.css";
import { ModelList } from "./ModelList";
import {
  mergeDiscovered,
  type ProviderCreate,
  type ProviderModel,
  type ProviderProfile,
} from "./model";

type HostOption = { id: string; label: string };

type Props = {
  initial?: ProviderProfile;
  rotateOnly?: boolean;
  busy?: boolean;
  error?: string | null;
  hosts?: HostOption[];
  /** Probe `{baseUrl}/v1/models` before the profile exists. */
  onDiscover?: (input: { baseUrl: string; token: string; profileId?: string }) => Promise<ProviderModel[]>;
  onSubmit: (body: ProviderCreate) => void;
  onCancel: () => void;
};

function scopeParts(scope: string | undefined): { kind: "universal" | "host"; hostId: string } {
  if (scope?.startsWith("host:")) return { kind: "host", hostId: scope.slice("host:".length) };
  return { kind: "universal", hostId: "" };
}

export function ProviderForm({
  initial,
  rotateOnly,
  busy,
  error,
  hosts = [],
  onDiscover,
  onSubmit,
  onCancel,
}: Props) {
  const editing = Boolean(initial && initial.kind !== "native");
  const initialScope = scopeParts(initial?.scope);
  const [name, setName] = useState(initial?.name ?? "");
  const [kind, setKind] = useState<"gateway" | "direct">(initial?.kind === "direct" ? "direct" : "gateway");
  const [baseUrl, setBaseUrl] = useState(initial?.baseUrl ?? "");
  const [authToken, setAuthToken] = useState("");
  const [models, setModels] = useState<ProviderModel[]>(initial?.models ?? []);
  const [defaultModel, setDefaultModel] = useState(initial?.defaultModel ?? "");
  const [discovered, setDiscovered] = useState<string[]>([]);
  const [discovering, setDiscovering] = useState(false);
  const [discoverError, setDiscoverError] = useState<string | null>(null);
  const [defaultGateway, setDefaultGateway] = useState(initial?.defaultGateway ?? true);
  const [scopeKind, setScopeKind] = useState<"universal" | "host">(initialScope.kind);
  const [scopeHostId, setScopeHostId] = useState(initialScope.hostId || hosts[0]?.id || "");
  const tokenRequired = !editing || rotateOnly;
  const canSave =
    name.trim() &&
    (kind === "direct" || baseUrl.trim()) &&
    (!tokenRequired || authToken.trim()) &&
    (scopeKind !== "host" || Boolean(scopeHostId));

  const discover = async () => {
    setDiscovering(true);
    setDiscoverError(null);
    try {
      const found = await onDiscover!({
        baseUrl: baseUrl.trim(),
        token: authToken.trim(),
        // An edit with no retyped token re-probes with the stored one.
        ...(editing && !authToken.trim() && initial ? { profileId: initial.id } : {}),
      });
      const { models: merged, added } = mergeDiscovered(models, found);
      setModels(merged);
      // Badging every row on a first probe says nothing; "new" only means new
      // relative to a catalog that already existed.
      setDiscovered(models.length ? added : []);
      if (!defaultModel) {
        setDefaultModel(merged.find((m) => m.enabled)?.id ?? "");
      }
      if (!found.length) setDiscoverError("网关未列出任何模型，可手动填入 id");
    } catch (err: unknown) {
      setDiscoverError(err instanceof Error ? err.message : "探测失败");
    } finally {
      setDiscovering(false);
    }
  };

  return (
    <form
      className={css.form}
      data-testid="provider-form"
      onSubmit={(e) => {
        e.preventDefault();
        if (!canSave || busy) return;
        const enabled = models.filter((m) => m.enabled);
        // `defaultModel` must name an enabled model; the Hub rejects it otherwise.
        const fallback = enabled.some((m) => m.id === defaultModel) ? defaultModel : enabled[0]?.id;
        onSubmit({
          name: name.trim(),
          kind,
          baseUrl: baseUrl.trim(),
          models,
          defaultModel: fallback,
          authToken: authToken.trim(),
          defaultGateway: kind === "gateway" && defaultGateway,
          scope: scopeKind === "host" && scopeHostId ? `host:${scopeHostId}` : "universal",
        });
      }}
    >
      {rotateOnly ? (
        <label className={ui.field}>
          新 auth token
          <input
            className={ui.input}
            type="password"
            autoComplete="off"
            data-testid="provider-token"
            value={authToken}
            onChange={(e) => setAuthToken(e.target.value)}
            placeholder="只提交一次，之后只显示 last4"
          />
        </label>
      ) : (
        <>
          <label className={ui.field}>
            名称
            <input className={ui.input} data-testid="provider-name" value={name} onChange={(e) => setName(e.target.value)} />
          </label>
          <fieldset className={ui.field} style={{ border: 0, padding: 0, margin: 0 }}>
            <legend>类型</legend>
            <div className={css.seg}>
              <button
                type="button"
                className={`${css.choice} ${kind === "gateway" ? css.choiceOn : ""}`}
                data-testid="provider-kind-gateway"
                onClick={() => setKind("gateway")}
              >
                网关
              </button>
              <button
                type="button"
                className={`${css.choice} ${kind === "direct" ? css.choiceOn : ""}`}
                data-testid="provider-kind-direct"
                onClick={() => setKind("direct")}
              >
                直连
              </button>
            </div>
          </fieldset>
          <label className={ui.field}>
            Base URL
            <input
              className={ui.input}
              data-testid="provider-base-url"
              placeholder="https://gateway.example/v1"
              value={baseUrl}
              onChange={(e) => setBaseUrl(e.target.value)}
            />
          </label>
          <label className={ui.field}>
            {editing ? "轮换 auth token（留空则不变）" : "Auth token"}
            <input
              className={ui.input}
              type="password"
              autoComplete="off"
              data-testid="provider-token"
              value={authToken}
              onChange={(e) => setAuthToken(e.target.value)}
              placeholder="只提交一次，GET 永不返回"
            />
          </label>
          <ModelList
            models={models}
            defaultModel={defaultModel}
            profileId={initial?.id}
            discovered={discovered}
            discovering={discovering}
            discoverError={discoverError}
            onDiscover={onDiscover && baseUrl.trim() ? () => void discover() : undefined}
            onChange={setModels}
            onDefaultChange={setDefaultModel}
          />
          <fieldset className={ui.field} style={{ border: 0, padding: 0, margin: 0 }}>
            <legend>范围</legend>
            <div className={css.seg}>
              <button
                type="button"
                className={`${css.choice} ${scopeKind === "universal" ? css.choiceOn : ""}`}
                data-testid="provider-scope-universal"
                onClick={() => setScopeKind("universal")}
              >
                全局
              </button>
              <button
                type="button"
                className={`${css.choice} ${scopeKind === "host" ? css.choiceOn : ""}`}
                data-testid="provider-scope-host"
                onClick={() => {
                  setScopeKind("host");
                  if (!scopeHostId && hosts[0]) setScopeHostId(hosts[0].id);
                }}
              >
                仅此主机
              </button>
            </div>
          </fieldset>
          {scopeKind === "host" ? (
            <label className={ui.field}>
              主机
              <select
                className={ui.input}
                data-testid="provider-scope-host-id"
                value={scopeHostId}
                onChange={(e) => setScopeHostId(e.target.value)}
              >
                {hosts.map((host) => (
                  <option key={host.id} value={host.id}>
                    {host.label}
                  </option>
                ))}
              </select>
            </label>
          ) : null}
          {kind === "gateway" ? (
            <label className={css.toggleRow}>
              <input
                type="checkbox"
                data-testid="provider-default-gateway"
                checked={defaultGateway}
                onChange={(e) => setDefaultGateway(e.target.checked)}
              />
              设为该范围的默认网关
            </label>
          ) : null}
        </>
      )}
      {error ? (
        <p className={css.error} data-testid="provider-form-error">
          {error}
        </p>
      ) : null}
      <div className={ui.row} style={{ marginTop: 8 }}>
        <Button type="button" onClick={onCancel}>
          取消
        </Button>
        <Button type="submit" variant="primary" disabled={!canSave || busy} data-testid="provider-save">
          {busy ? "保存中" : editing || rotateOnly ? "保存" : "创建"}
        </Button>
      </div>
    </form>
  );
}
