import { useState } from "react";
import { Button } from "../../components/Button";
import ui from "../../styles/ui.module.css";
import css from "./providers.module.css";
import { ModelList } from "./ModelList";
import {
  DELIVERY_ROUTE_LABELS,
  DEFAULT_DELIVERY,
  effectiveDelivery,
  mergeDiscovered,
  type ApiRouteMode,
  type DeliveryHost,
  type ProviderCreate,
  type ProviderDelivery,
  type ProviderModel,
  type ProviderProfile,
} from "./model";

type Props = {
  initial?: ProviderProfile;
  rotateOnly?: boolean;
  busy?: boolean;
  error?: string | null;
  /** Hosts a `via` delivery can name; `online` drives the offline warning. */
  hosts?: DeliveryHost[];
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
  // D-047 交付方式: direct (default) or via a named host, plus the route
  // sub-mode the launch decides once and echoes back.
  const [delivery, setDelivery] = useState<ProviderDelivery>(
    effectiveDelivery(initial?.delivery),
  );
  // Keep the selected proxy host where possible when the host list arrives
  // after the form opened, the way the scope selector does.
  const viaHostId = delivery.mode === "via"
    ? (delivery.viaHostId || hosts[0]?.id || "")
    : "";
  const viaHost = hosts.find((host) => host.id === viaHostId);
  const viaHostOffline = delivery.mode === "via" && viaHost?.online === false;
  const tokenRequired = !editing || rotateOnly;
  const canSave =
    name.trim() &&
    (kind === "direct" || baseUrl.trim()) &&
    (!tokenRequired || authToken.trim()) &&
    (scopeKind !== "host" || Boolean(scopeHostId)) &&
    (kind !== "gateway" || delivery.mode !== "via" || Boolean(viaHostId));

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
          // Always send the delivery on a gateway save so an edit that flips
          // via back to direct actually clears the stored object (PATCH
          // replaces, never merges, the delivery).
          ...(kind === "gateway"
            ? {
                delivery: {
                  mode: delivery.mode,
                  route: delivery.route,
                  ...(delivery.mode === "via" && viaHostId
                    ? { viaHostId: viaHostId }
                    : {}),
                } satisfies ProviderDelivery,
              }
            : {}),
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
          <fieldset className={css.plainFieldset}>
            <legend>类型</legend>
            <div className={css.seg}>
              <button
                type="button"
                className={css.choice}
                aria-pressed={kind === "gateway"}
                data-testid="provider-kind-gateway"
                onClick={() => setKind("gateway")}
              >
                网关
              </button>
              <button
                type="button"
                className={css.choice}
                aria-pressed={kind === "direct"}
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
          <fieldset className={css.plainFieldset}>
            <legend>范围</legend>
            <div className={css.seg}>
              <button
                type="button"
                className={css.choice}
                aria-pressed={scopeKind === "universal"}
                data-testid="provider-scope-universal"
                onClick={() => setScopeKind("universal")}
              >
                全局
              </button>
              <button
                type="button"
                className={css.choice}
                aria-pressed={scopeKind === "host"}
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
          {kind === "gateway" ? (
            <fieldset
              className={css.plainFieldset}
              data-testid="provider-delivery"
              data-mode={delivery.mode}
            >
              <legend>交付方式</legend>
              <div className={css.seg}>
                <button
                  type="button"
                  className={css.choice}
                  aria-pressed={delivery.mode === "direct"}
                  data-testid="provider-delivery-direct"
                  onClick={() => setDelivery(DEFAULT_DELIVERY)}
                >
                  直连
                </button>
                <button
                  type="button"
                  className={css.choice}
                  aria-pressed={delivery.mode === "via"}
                  data-testid="provider-delivery-via"
                  onClick={() => {
                    const hostId = delivery.viaHostId || hosts[0]?.id || "";
                    setDelivery({
                      mode: "via",
                      route: delivery.route,
                      ...(hostId ? { viaHostId: hostId } : {}),
                    });
                  }}
                >
                  经主机
                </button>
              </div>
              {delivery.mode === "via" ? (
                <>
                  <label className={ui.field}>
                    代理主机
                    <select
                      className={ui.input}
                      data-testid="provider-delivery-host"
                      value={viaHostId}
                      onChange={(e) =>
                        setDelivery((current) => ({ ...current, mode: "via", viaHostId: e.target.value }))
                      }
                    >
                      {hosts.length === 0 ? <option value="">（无已登记主机）</option> : null}
                      {hosts.map((host) => (
                        <option key={host.id} value={host.id}>
                          {host.label}
                        </option>
                      ))}
                    </select>
                  </label>
                  <label className={ui.field}>
                    路由
                    <select
                      className={ui.input}
                      data-testid="provider-delivery-route"
                      value={delivery.route}
                      onChange={(e) =>
                        setDelivery((current) => ({
                          ...current,
                          route: e.target.value as ApiRouteMode,
                        }))
                      }
                    >
                      <option value="auto">{DELIVERY_ROUTE_LABELS.auto}（先探直连，回落 Hub 中转）</option>
                      <option value="hub-relay">{DELIVERY_ROUTE_LABELS["hub-relay"]}（走 Hub↔Node 链路）</option>
                      <option value="direct-net">{DELIVERY_ROUTE_LABELS["direct-net"]}（不通则拒绝）</option>
                    </select>
                  </label>
                  {/* D-047: an offline proxy host is a launch refusal
                      (api-via-host-offline); warn before the save, never
                      silently reroute. */}
                  {viaHostOffline ? (
                    <p className={css.warning} data-testid="provider-delivery-host-offline">
                      代理主机 {viaHost?.label || viaHostId} 当前离线：经它的派发会在启动时被 Hub 拒绝（api-via-host-offline），不会改道直连。
                    </p>
                  ) : null}
                </>
              ) : null}
            </fieldset>
          ) : null}
        </>
      )}
      {error ? (
        <p className={css.error} data-testid="provider-form-error">
          {error}
        </p>
      ) : null}
      <div className={css.formActions}>
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
