import { useEffect, useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import { Button } from "../components/Button";
import { Modal } from "../components/Modal";
import {
  DELEGATION_COPY,
  NATIVE_PROFILE,
  contextChip,
  deliveryClause,
  deliveryHostOffline,
  enabledModels,
  formatSecret,
  fromHub,
  healthLine,
  shouldAvoidUnhealthy,
  type DeliveryHost,
  type ProviderCreate,
  type ProviderDiscoverBody,
  type ProviderModel,
  type ProviderProfile,
  type ProviderTestResult,
} from "../features/providers";
import { ProviderForm } from "../features/providers/ProviderForm";
import css from "../features/providers/providers.module.css";
import { api } from "../lib/api";
import type { Host } from "../types/instance";

function healthDot(ok: boolean | undefined) {
  return <span className={`${css.dot} ${ok === false ? css.dotOff : ""}`} aria-hidden />;
}

/** Host inventory for the delivery control: label plus live state. */
function toDeliveryHosts(hosts: Host[]): DeliveryHost[] {
  return hosts.map((host) => ({
    id: host.id,
    label: host.label,
    online: host.online ?? host.state === "online",
  }));
}

/** Probe a gateway for its catalog; the token is sent once and never stored. */
async function discoverModels(input: ProviderDiscoverBody): Promise<ProviderModel[]> {
  const result = await api.providerDiscover(input);
  if (!result.reachable || !result.ok) throw new Error(result.message);
  return result.models ?? [];
}

function useProviderList() {
  const [items, setItems] = useState<ProviderProfile[]>([NATIVE_PROFILE]);
  const [hosts, setHosts] = useState<DeliveryHost[]>([]);
  const [error, setError] = useState<string | null>(null);
  const reload = () => {
    void api
      .providerList()
      .then((page) => {
        const hub = page.items.map(fromHub);
        setItems([NATIVE_PROFILE, ...hub]);
        setError(null);
      })
      .catch((err: unknown) => setError(err instanceof Error ? err.message : "load failed"));
  };
  useEffect(() => {
    reload();
    void api
      .hostList()
      .then((page) => setHosts(toDeliveryHosts(page.items)))
      .catch(() => setHosts([]));
  }, []);
  return { items, hosts, error, reload };
}

export function ProvidersPage() {
  const { items, hosts, error, reload } = useProviderList();
  const [creating, setCreating] = useState(false);
  const [busy, setBusy] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  const live = items.filter((p) => p.delegation !== "direct");
  const direct = items.find((p) => p.delegation === "direct");

  return (
    <div className={css.page} data-testid="providers-page">
      <header className={css.head}>
        <h1 className={css.title}>Provider</h1>
        <div className={css.sub}>自己的 Anthropic-Messages 网关，不再手写 overlay</div>
        <div style={{ flex: 1 }} />
        <Button variant="primary" data-testid="provider-add" onClick={() => setCreating(true)}>
          添加网关
        </Button>
      </header>
      <div className={css.body}>
        {error ? <p className={css.error}>{error}</p> : null}
        {live.map((p) => (
          <Link
            key={p.profileId}
            to={`/providers/${p.profileId}`}
            className={css.card}
            data-testid="provider-row"
            data-delegation={p.delegation}
            data-available={p.available ? "1" : "0"}
            data-default={p.defaultGateway ? "1" : "0"}
          >
            <div className={css.cardHead}>
              {healthDot(p.health?.ok ?? true)}
              <span className={css.id}>{p.name || p.profileId}</span>
              {p.defaultGateway ? <span className={css.badge}>默认网关</span> : null}
              {p.scope?.startsWith("host:") ? <span className={css.badge}>host</span> : null}
              <span className={css.proto}>{p.protocol}</span>
            </div>
            <div className={css.grid}>
              <div className={css.label}>baseUrl</div>
              <div className={css.value}>{p.baseUrl ?? "—"}</div>
              <div className={css.label}>健康</div>
              <div className={css.value}>{healthLine(p.health)}</div>
              <div className={css.label}>secret</div>
              <div className={css.value}>{formatSecret(p.secret)} · last4</div>
              {p.kind === "gateway" ? (
                <>
                  <div className={css.label}>交付方式</div>
                  <div
                    className={css.value}
                    data-testid="provider-delivery"
                    data-via={p.delivery?.mode === "via" ? "1" : "0"}
                    data-offline={
                      p.delivery?.mode === "via" && deliveryHostOffline(p.delivery, hosts)
                        ? "1"
                        : "0"
                    }
                  >
                    {deliveryClause(p.delivery, hosts)}
                  </div>
                </>
              ) : null}
            </div>
            <div className={css.blurb}>{DELEGATION_COPY[p.delegation].hint}</div>
          </Link>
        ))}
        {direct ? (
          <Link
            to={`/providers/${direct.profileId}`}
            className={direct.available ? css.card : css.dashed}
            data-testid="provider-row"
            data-delegation="direct"
            data-available={direct.available ? "1" : "0"}
          >
            <div className={css.dashTitle}>Direct {direct.available ? direct.name : "多 key / 权重 / 冷却"}</div>
            <div className={css.dashBody}>
              {direct.available ? formatSecret(direct.secret) : "标记 v2，本规格不实现 —— 位置留着，避免以后另起一页"}
            </div>
          </Link>
        ) : (
          <div className={css.dashed} data-testid="provider-row" data-delegation="direct" data-available="0">
            <div className={css.dashTitle}>Direct 多 key / 权重 / 冷却</div>
            <div className={css.dashBody}>标记 v2，本规格不实现 —— 位置留着，避免以后另起一页</div>
          </div>
        )}
        <div className={css.foot}>健康红点不自动切换会话中的 key；只提示「新会话将避开不健康 profile」。</div>
      </div>
      <Modal
        open={creating}
        onClose={() => {
          if (!busy) setCreating(false);
        }}
      >
        <h2 style={{ marginTop: 0, fontSize: 16 }}>添加网关</h2>
        <ProviderForm
          busy={busy}
          error={formError}
          hosts={hosts}
          onDiscover={discoverModels}
          onCancel={() => setCreating(false)}
          onSubmit={(body) => {
            setBusy(true);
            setFormError(null);
            void api
              .providerCreate(body)
              .then(() => {
                setCreating(false);
                reload();
              })
              .catch((err: unknown) => setFormError(err instanceof Error ? err.message : "create failed"))
              .finally(() => setBusy(false));
          }}
        />
      </Modal>
    </div>
  );
}

export function ProviderDetailPage() {
  const { profileId } = useParams();
  const navigate = useNavigate();
  const [p, setP] = useState<ProviderProfile | undefined>(() =>
    profileId === "none" ? NATIVE_PROFILE : undefined,
  );
  const [busy, setBusy] = useState(false);
  const [editing, setEditing] = useState(false);
  const [rotating, setRotating] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  const [test, setTest] = useState<ProviderTestResult | null>(null);
  const [hosts, setHosts] = useState<DeliveryHost[]>([]);

  useEffect(() => {
    void api
      .hostList()
      .then((page) => setHosts(toDeliveryHosts(page.items)))
      .catch(() => setHosts([]));
  }, []);

  useEffect(() => {
    if (!profileId || profileId === "none") {
      setP(NATIVE_PROFILE);
      return;
    }
    let cancelled = false;
    void api
      .providerGet(profileId)
      .then((row) => {
        if (!cancelled) setP(fromHub(row));
      })
      .catch(() => {
        if (!cancelled) setP(undefined);
      });
    return () => {
      cancelled = true;
    };
  }, [profileId]);

  if (!p) return <p style={{ padding: 16 }}>未知 profile</p>;
  const copy = DELEGATION_COPY[p.delegation];
  const editable = p.kind !== "native";

  const save = (body: ProviderCreate, rotate: boolean) => {
    setBusy(true);
    setFormError(null);
    const patch = rotate
      ? { authToken: body.authToken }
      : {
          name: body.name,
          kind: body.kind,
          baseUrl: body.baseUrl,
          models: body.models,
          defaultModel: body.defaultModel ?? null,
          defaultGateway: body.defaultGateway,
          scope: body.scope,
          // D-047: the form always sends the delivery for a gateway; an
          // absent key would leave a stale via object in place on PATCH.
          ...(body.kind === "gateway" && body.delivery ? { delivery: body.delivery } : {}),
          ...(body.authToken ? { authToken: body.authToken } : {}),
        };
    void api
      .providerPatch(p.id, patch)
      .then((row) => {
        setP(fromHub(row));
        setEditing(false);
        setRotating(false);
      })
      .catch((err: unknown) => setFormError(err instanceof Error ? err.message : "save failed"))
      .finally(() => setBusy(false));
  };

  return (
    <div className={css.page} data-testid="provider-detail" data-delegation={p.delegation}>
      <header className={css.head}>
        <Link to="/providers" className={css.back}>
          ←
        </Link>
        <h1 className={css.title}>{p.name || p.profileId}</h1>
        <div className={css.sub}>{copy.title}</div>
      </header>
      <div className={css.body}>
        <div className={p.available ? css.card : css.dashed}>
          <div className={css.cardHead}>
            {healthDot(p.health?.ok ?? p.available)}
            <span className={css.id}>{p.profileId}</span>
            {p.defaultGateway ? <span className={css.badge}>默认网关</span> : null}
            <span className={css.proto}>{p.protocol}</span>
          </div>
          <div className={css.grid}>
            <div className={css.label}>baseUrl</div>
            <div className={css.value}>{p.baseUrl ?? "—"}</div>
            <div className={css.label}>scope</div>
            <div className={css.value}>{p.scope || "universal"}</div>
            <div className={css.label}>健康</div>
            <div className={css.value} data-testid="provider-health">
              {healthLine(p.health)}
              {p.health?.checkedAt ? ` · ${p.health.checkedAt}` : ""}
            </div>
            <div className={css.label}>secret</div>
            <div className={css.value} data-testid="provider-secret">
              {formatSecret(p.secret)} last4
            </div>
            <div className={css.label}>models</div>
            <div className={css.value} data-testid="provider-model-summary">
              {p.models.length
                ? `${enabledModels(p.models).length}/${p.models.length} 已启用`
                : "由 CLI 原生目录决定"}
            </div>
            <div className={css.label}>默认模型</div>
            <div className={css.value} data-testid="provider-default-model">
              {p.defaultModel ?? "—"}
            </div>
            {p.kind === "gateway" ? (
              <>
                <div className={css.label}>交付方式</div>
                <div
                  className={css.value}
                  data-testid="provider-delivery"
                  data-via={p.delivery?.mode === "via" ? "1" : "0"}
                  data-offline={
                    p.delivery?.mode === "via" && deliveryHostOffline(p.delivery, hosts)
                      ? "1"
                      : "0"
                  }
                >
                  {deliveryClause(p.delivery, hosts)}
                </div>
              </>
            ) : null}
            <div className={css.label}>lastError</div>
            <div className={css.value}>{p.lastError ?? "—"}</div>
          </div>
          <div className={css.blurb}>{copy.hint}</div>
        </div>
        {p.models.length ? (
          <div>
            <div className={css.sectionLabel}>模型名原样透传</div>
            <div className={css.models}>
              {p.models.map((m) => {
                const context = contextChip(m.contextWindow);
                return (
                  <span
                    key={m.id}
                    className={css.chip}
                    data-testid="provider-model-chip"
                    data-enabled={m.enabled ? "1" : "0"}
                    style={m.enabled ? undefined : { opacity: 0.5 }}
                  >
                    {m.id}
                    {context ? ` · ${context}` : ""}
                    {m.id === p.defaultModel ? " · 默认" : ""}
                  </span>
                );
              })}
            </div>
          </div>
        ) : null}
        {editable ? (
          <div className={css.actions}>
            <Button
              data-testid="provider-test"
              disabled={busy}
              onClick={() => {
                setBusy(true);
                setTest(null);
                void api
                  .providerTest(p.id)
                  .then((result) => {
                    setTest(result);
                    setP((prev) =>
                      prev
                        ? {
                            ...prev,
                            health: {
                              ok: result.ok,
                              status: result.status,
                              latencyMs: result.latencyMs,
                              message: result.message,
                            },
                            lastError: result.ok ? null : result.message,
                          }
                        : prev,
                    );
                  })
                  .catch((err: unknown) =>
                    setTest({
                      ok: false,
                      reachable: false,
                      message: err instanceof Error ? err.message : "test failed",
                    }),
                  )
                  .finally(() => setBusy(false));
              }}
            >
              测试连通
            </Button>
            <Button data-testid="provider-edit" onClick={() => setEditing(true)}>
              编辑
            </Button>
            <Button data-testid="provider-rotate" onClick={() => setRotating(true)}>
              轮换 token
            </Button>
            <Button
              data-testid="provider-set-default"
              disabled={busy || p.kind !== "gateway" || p.defaultGateway}
              onClick={() => {
                setBusy(true);
                void api
                  .providerPatch(p.id, { defaultGateway: true })
                  .then((row) => setP(fromHub(row)))
                  .finally(() => setBusy(false));
              }}
            >
              设为默认网关
            </Button>
            <Button
              variant="danger"
              data-testid="provider-delete"
              disabled={busy}
              onClick={() => {
                if (!window.confirm(`删除 ${p.name}？`)) return;
                setBusy(true);
                void api
                  .providerDelete(p.id)
                  .then(() => navigate("/providers"))
                  .finally(() => setBusy(false));
              }}
            >
              删除
            </Button>
          </div>
        ) : null}
        {test ? (
          <p className={css.foot} data-testid="provider-test-result" data-ok={test.ok ? "1" : "0"}>
            {test.message}
          </p>
        ) : null}
        {shouldAvoidUnhealthy(p) ? (
          <p className={css.foot} style={{ color: "var(--dust)" }} data-testid="provider-unhealthy-hint">
            新会话将避开不健康 profile
          </p>
        ) : (
          <div className={css.foot}>健康红点不自动切换会话中的 key；只提示「新会话将避开不健康 profile」。</div>
        )}
      </div>
      <Modal open={editing} onClose={() => !busy && setEditing(false)}>
        <h2 style={{ marginTop: 0, fontSize: 16 }}>编辑 {p.name}</h2>
        <ProviderForm
          initial={p}
          busy={busy}
          error={formError}
          hosts={hosts}
          onDiscover={discoverModels}
          onCancel={() => setEditing(false)}
          onSubmit={(body) => save(body, false)}
        />
      </Modal>
      <Modal open={rotating} onClose={() => !busy && setRotating(false)}>
        <h2 style={{ marginTop: 0, fontSize: 16 }}>轮换 token</h2>
        <ProviderForm
          initial={p}
          rotateOnly
          busy={busy}
          error={formError}
          onCancel={() => setRotating(false)}
          onSubmit={(body) => save(body, true)}
        />
      </Modal>
    </div>
  );
}
