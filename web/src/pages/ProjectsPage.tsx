import { Link, useParams } from "react-router-dom";
import { hubStore, useHub } from "../lib/store";
import ui from "../styles/ui.module.css";

export function ProjectsPage() {
  const hub = useHub();
  return (
    <div style={{ padding: 16 }}>
      <h1 style={{ fontSize: 18 }}>项目</h1>
      <p className={ui.listMeta}>Workspace 通讯录，不是独立项目实体。</p>
      {hub.workspaces.map((w) => (
        <Link key={w.id} to={`/projects/${w.id}`} className={ui.listItem}>
          <span>
            <div>{w.label}</div>
            <div className={ui.listMeta}>
              {hubStore.hostName(w.hostId)} · {w.rootPath}
            </div>
          </span>
        </Link>
      ))}
    </div>
  );
}

export function ProjectDetailPage() {
  const { workspaceId = "" } = useParams();
  const hub = useHub();
  const w = hub.workspaces.find((x) => x.id === workspaceId) ?? hub.workspaces[0];
  if (!w) return <p style={{ padding: 16 }}>Workspace 不存在</p>;
  return (
    <div style={{ padding: 16 }}>
      <h1>{w.label}</h1>
      <p className={ui.path}>{w.rootPath}</p>
      <p className={ui.listMeta}>writePolicy {w.writePolicy}</p>
    </div>
  );
}
