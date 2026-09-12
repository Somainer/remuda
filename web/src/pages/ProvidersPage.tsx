import { Link, useParams } from "react-router-dom";
import {
  DELEGATION_COPY,
  PROVIDER_PROFILES,
  healthLine,
  profileById,
  redactSecretRef,
  shouldAvoidUnhealthy,
} from "../features/providers";
import ui from "../styles/ui.module.css";

export function ProvidersPage() {
  return (
    <div style={{ padding: 16 }} data-testid="providers-page">
      <h1 style={{ fontSize: 18 }}>Provider</h1>
      <p className={ui.listMeta}>delegation：none（默认原生登录）/ gateway / direct（v2）。代码不绑特定网关厂商。</p>
      {PROVIDER_PROFILES.map((p) => (
        <Link
          key={p.profileId}
          to={`/providers/${p.profileId}`}
          className={ui.listItem}
          data-testid="provider-row"
          data-delegation={p.delegation}
          data-available={p.available ? "1" : "0"}
        >
          <span
            className={`${ui.dot} ${p.health?.ok ? ui.dotIdle : p.health ? ui.dotUnknown : ui.dotIdle}`}
            aria-hidden
          />
          <span>
            <div>
              {p.profileId} · {DELEGATION_COPY[p.delegation].title}
            </div>
            <div className={ui.listMeta}>
              {p.protocol}
              {p.baseUrl ? ` · ${p.baseUrl}` : ""}
              {" · "}
              {healthLine(p.health)}
              {p.available ? "" : " · v2"}
            </div>
          </span>
        </Link>
      ))}
    </div>
  );
}

export function ProviderDetailPage() {
  const { profileId } = useParams();
  const p = profileById(profileId);
  if (!p) return <p style={{ padding: 16 }}>未知 profile</p>;
  const copy = DELEGATION_COPY[p.delegation];
  return (
    <div style={{ padding: 16 }} data-testid="provider-detail" data-delegation={p.delegation}>
      <p>
        <Link to="/providers">← Provider</Link>
      </p>
      <h1>
        {p.profileId} · {copy.title}
      </h1>
      <p className={ui.listMeta}>{copy.hint}</p>
      <p>协议 {p.protocol}</p>
      <p className={ui.listMeta}>endpoint {p.baseUrl ?? "—"}</p>
      <p className={ui.listMeta} data-testid="provider-health">
        {healthLine(p.health)}
        {p.health?.checkedAt ? ` · ${p.health.checkedAt}` : ""}
      </p>
      <p className={ui.listMeta} data-testid="provider-secret">
        data-plane key {redactSecretRef(p.secretRef)}（前 4 位）
      </p>
      <p className={ui.listMeta}>轮换权威 {p.rotationOwner}。runtime 只持 secret ref。</p>
      <p className={ui.listMeta}>模型 {p.models.length ? p.models.join(" · ") : "由 CLI 原生目录决定"}</p>
      <p className={ui.listMeta}>最近错误 {p.lastError ?? "—"}</p>
      {shouldAvoidUnhealthy(p) ? (
        <p className={ui.listMeta} style={{ color: "var(--dust)" }} data-testid="provider-unhealthy-hint">
          新会话将避开不健康 profile
        </p>
      ) : null}
      {!p.available ? <p className={ui.listMeta}>直连多 key 是 v2，本页只展示占位。</p> : null}
    </div>
  );
}
