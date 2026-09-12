import { Link, useParams } from "react-router-dom";
import { useHub } from "../lib/store";
import ui from "../styles/ui.module.css";

export function HostsPage() {
  const hub = useHub();
  return (
    <div style={{ padding: 16 }}>
      <h1 style={{ fontSize: 18 }}>主机</h1>
      {hub.hosts.map((host) => (
        <Link key={host.id} to={`/hosts/${host.id}`} className={ui.listItem}>
          <span>
            <div>{host.label}</div>
            <div className={ui.listMeta}>
              {host.state} · {host.transport.mode}
            </div>
          </span>
        </Link>
      ))}
    </div>
  );
}

export function HostDetailPage() {
  const { hostId = "" } = useParams();
  const hub = useHub();
  const host = hub.hosts.find((h) => h.id === hostId) ?? hub.hosts[0];
  if (!host) return <p style={{ padding: 16 }}>主机不存在</p>;
  return (
    <div style={{ padding: 16 }}>
      <h1>{host.label}</h1>
      <p className={ui.listMeta}>传输 {host.transport.mode}</p>
      <p className={ui.listMeta}>状态 {host.state}</p>
      <p>CLI 按本机绝对路径盘点，不假设与 Mac 同版本。</p>
    </div>
  );
}
