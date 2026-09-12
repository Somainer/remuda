import { Link, useParams } from "react-router-dom";
import { ASTERGATE_DEFAULT } from "../features/providers/astergate";
import ui from "../styles/ui.module.css";

export function ProvidersPage() {
  const p = ASTERGATE_DEFAULT;
  return (
    <div style={{ padding: 16 }}>
      <h1 style={{ fontSize: 18 }}>Provider</h1>
      <p className={ui.listMeta}>M0–M2 只展示 astergate-default。Direct 多 key 是 v2。</p>
      <Link to={`/providers/${p.profileId}`} className={ui.listItem}>
        <span>
          <div>{p.profileId}</div>
          <div className={ui.listMeta}>
            {p.protocol} · 健康 {p.health.ok ? 200 : "err"} {p.health.latencyMs}ms
          </div>
        </span>
      </Link>
    </div>
  );
}

export function ProviderDetailPage() {
  const { profileId } = useParams();
  const p = ASTERGATE_DEFAULT;
  if (profileId && profileId !== p.profileId) return <p style={{ padding: 16 }}>未知 profile</p>;
  return (
    <div style={{ padding: 16 }}>
      <h1>{p.profileId}</h1>
      <p>{p.protocol}</p>
      <p className={ui.listMeta}>endpoint {p.baseUrl}</p>
      <p className={ui.listMeta}>data-plane key {p.secretRef}（前 4 位）</p>
      <p>轮换发生在 AsterGate，不在 runtime。</p>
    </div>
  );
}
