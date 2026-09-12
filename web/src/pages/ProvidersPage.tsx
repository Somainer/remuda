import { Link, useParams } from "react-router-dom";
import {
  DELEGATION_COPY,
  PROVIDER_PROFILES,
  healthLine,
  profileById,
  redactSecretRef,
  shouldAvoidUnhealthy,
} from "../features/providers";
import css from "../features/providers/providers.module.css";

function healthDot(ok: boolean | undefined) {
  return <span className={`${css.dot} ${ok === false ? css.dotOff : ""}`} aria-hidden />;
}

export function ProvidersPage() {
  const live = PROVIDER_PROFILES.filter((p) => p.delegation !== "direct");
  const direct = PROVIDER_PROFILES.find((p) => p.delegation === "direct");

  return (
    <div className={css.page} data-testid="providers-page">
      <header className={css.head}>
        <h1 className={css.title}>Provider</h1>
        <div className={css.sub}>M0–M2 只有一条网关 + 原生默认</div>
      </header>
      <div className={css.body}>
        {live.map((p) => (
          <Link
            key={p.profileId}
            to={`/providers/${p.profileId}`}
            className={css.card}
            data-testid="provider-row"
            data-delegation={p.delegation}
            data-available={p.available ? "1" : "0"}
          >
            <div className={css.cardHead}>
              {healthDot(p.health?.ok ?? true)}
              <span className={css.id}>{p.profileId}</span>
              <span className={css.proto}>{p.protocol}</span>
            </div>
            <div className={css.grid}>
              <div className={css.label}>baseUrl</div>
              <div className={css.value}>{p.baseUrl ?? "—"}</div>
              <div className={css.label}>健康</div>
              <div className={css.value}>{healthLine(p.health)}</div>
              <div className={css.label}>secretRef</div>
              <div className={css.value}>{redactSecretRef(p.secretRef)} 受限 data-plane key</div>
            </div>
            <div className={css.blurb}>{DELEGATION_COPY[p.delegation].hint}</div>
          </Link>
        ))}
        {direct ? (
          <Link
            to={`/providers/${direct.profileId}`}
            className={css.dashed}
            data-testid="provider-row"
            data-delegation="direct"
            data-available="0"
          >
            <div className={css.dashTitle}>Direct 多 key / 权重 / 冷却</div>
            <div className={css.dashBody}>标记 v2，本规格不实现 —— 位置留着，避免以后另起一页</div>
          </Link>
        ) : null}
        <div className={css.foot}>健康红点不自动切换会话中的 key；只提示「新会话将避开不健康 profile」。</div>
      </div>
    </div>
  );
}

export function ProviderDetailPage() {
  const { profileId } = useParams();
  const p = profileById(profileId);
  if (!p) return <p style={{ padding: 16 }}>未知 profile</p>;
  const copy = DELEGATION_COPY[p.delegation];
  return (
    <div className={css.page} data-testid="provider-detail" data-delegation={p.delegation}>
      <header className={css.head}>
        <Link to="/providers" className={css.back}>
          ←
        </Link>
        <h1 className={css.title}>{p.profileId}</h1>
        <div className={css.sub}>{copy.title}</div>
      </header>
      <div className={css.body}>
        <div className={p.available ? css.card : css.dashed}>
          <div className={css.cardHead}>
            {healthDot(p.health?.ok ?? p.available)}
            <span className={css.id}>{p.profileId}</span>
            <span className={css.proto}>{p.protocol}</span>
          </div>
          <div className={css.grid}>
            <div className={css.label}>baseUrl</div>
            <div className={css.value}>{p.baseUrl ?? "—"}</div>
            <div className={css.label}>健康</div>
            <div className={css.value} data-testid="provider-health">
              {healthLine(p.health)}
              {p.health?.checkedAt ? ` · ${p.health.checkedAt}` : ""}
            </div>
            <div className={css.label}>secretRef</div>
            <div className={css.value} data-testid="provider-secret">
              {redactSecretRef(p.secretRef)} ···· 受限 data-plane key（前 4 位）
            </div>
            <div className={css.label}>models</div>
            <div className={css.value}>{p.models.length ? `${p.models.length} 个 · discovery 开` : "由 CLI 原生目录决定"}</div>
            <div className={css.label}>lastError</div>
            <div className={css.value}>{p.lastError ?? "—"}</div>
          </div>
          <div className={css.blurb}>{copy.hint}</div>
        </div>
        {p.models.length ? (
          <div>
            <div className={css.sectionLabel}>模型名原样透传</div>
            <div className={css.models}>
              {p.models.map((m) => (
                <span key={m} className={css.chip}>
                  {m}
                </span>
              ))}
            </div>
          </div>
        ) : null}
        {shouldAvoidUnhealthy(p) ? (
          <p className={css.foot} style={{ color: "var(--dust)" }} data-testid="provider-unhealthy-hint">
            新会话将避开不健康 profile
          </p>
        ) : (
          <div className={css.foot}>健康红点不自动切换会话中的 key；只提示「新会话将避开不健康 profile」。</div>
        )}
      </div>
    </div>
  );
}
