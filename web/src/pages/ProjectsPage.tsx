import { useEffect, useState } from "react";
import { Link, useParams } from "react-router-dom";
import { useHub } from "../lib/store";
import { rest } from "../lib/api";
import ui from "../styles/ui.module.css";
import {
  ProjectSwitcher,
  projectFilterStore,
  projectMemberHostIds,
  projectMemberRows,
  useProjectFilter,
  useProjects,
  type Project,
} from "../features/tasks/ProjectSwitcher";

/**
 * /projects — the Hub Project directory (D-050 §9, ui-spec §1.1). This page
 * reads the authoritative `Project` entity through the generated client; it
 * no longer lists registered workspaces (that directory serves New Session
 * and the file view). The top switcher scopes the task list and the board by
 * projectId, with 全局 showing every project in scope.
 */
export function ProjectsPage() {
  const hub = useHub();
  const { projects, loading, error, reload } = useProjects();
  const selected = useProjectFilter();
  const visible = selected ? projects.filter((project) => project.id === selected) : projects;
  const hostLabel = (hostId: string) =>
    hub.hosts.find((host) => host.id === hostId)?.label ?? hostId;

  return (
    <div style={{ padding: 16 }}>
      <div
        style={{
          display: "flex",
          alignItems: "flex-end",
          justifyContent: "space-between",
          gap: 12,
          flexWrap: "wrap",
        }}
      >
        <h1 style={{ fontSize: 18 }}>项目</h1>
        <ProjectSwitcher projects={projects} navigateOnSelect />
      </div>
      <p className={ui.listMeta}>
        Hub Project 实体 · 成员经 (host, workspace) 映射到 Space，不合并同名/跨主机目录（D-024）
      </p>
      {loading ? <p className={ui.listMeta}>加载中…</p> : null}
      {error ? (
        <p className={ui.listMeta}>
          项目加载失败：{error} <button type="button" onClick={reload}>重试</button>
        </p>
      ) : null}
      {!loading && !error && visible.length === 0 ? (
        <p className={ui.listMeta}>{selected ? "所选项目不在当前范围内。" : "范围内还没有项目。"}</p>
      ) : null}
      {visible.map((project) => {
        const hosts = projectMemberHostIds(project);
        return (
          <Link key={project.id} to={`/projects/${project.id}`} className={ui.listItem} data-testid="project-row">
            <span>
              <div>{project.name}</div>
              <div className={ui.listMeta}>
                {project.members?.length ?? 0} 个成员 · {hosts.length} 台主机 · 基线 {project.defaultBaseBranch ?? "main"}
              </div>
              <div className={ui.listMeta}>
                {hosts.map((hostId) => hostLabel(hostId)).join(" · ") || "尚无成员工作区"}
              </div>
            </span>
          </Link>
        );
      })}
    </div>
  );
}

/**
 * /projects/:id — one authoritative Project. The route param is the project
 * id (the route name predates Project adoption). Members resolve through the
 * exact Space key: a member whose workspace is not registered on that host
 * renders as the bare host/workspace pair instead of being merged away.
 */
export function ProjectDetailPage() {
  const { workspaceId: projectId = "" } = useParams();
  const hub = useHub();
  const [project, setProject] = useState<Project | null>(null);
  const [status, setStatus] = useState<"loading" | "ready" | "missing">("loading");

  useEffect(() => {
    projectFilterStore.select(projectId);
    let cancelled = false;
    setStatus("loading");
    rest<Project>(`/v1/projects/${encodeURIComponent(projectId)}`)
      .then((doc) => {
        if (cancelled) return;
        setProject(doc);
        setStatus("ready");
      })
      .catch(() => {
        if (!cancelled) {
          setProject(null);
          setStatus("missing");
        }
      });
    return () => {
      cancelled = true;
    };
  }, [projectId]);

  if (status === "loading") {
    return (
      <div style={{ padding: 16 }}>
        <p className={ui.listMeta}>加载中…</p>
      </div>
    );
  }
  if (status === "missing" || !project) {
    return (
      <div style={{ padding: 16 }}>
        <h1 style={{ fontSize: 18 }}>项目不存在</h1>
        <p className={ui.listMeta}>
          <Link to="/projects">返回项目列表</Link>
        </p>
      </div>
    );
  }

  const rows = projectMemberRows(project, hub.workspaces);
  const hostLabel = (hostId: string) =>
    hub.hosts.find((host) => host.id === hostId)?.label ?? hostId;

  return (
    <div style={{ padding: 16 }}>
      <div
        style={{
          display: "flex",
          alignItems: "flex-end",
          justifyContent: "space-between",
          gap: 12,
          flexWrap: "wrap",
        }}
      >
        <div>
          <h1 style={{ fontSize: 18 }}>{project.name}</h1>
          <p className={ui.listMeta}>
            <Link to="/projects">项目</Link> · {project.id}
          </p>
        </div>
        <ProjectSwitcher projects={[project]} navigateOnSelect />
      </div>
      <p className={ui.path}>
        基线 {project.defaultBaseBranch ?? "main"} · 分支 {project.branchPattern ?? "wt/{worker}/{topic}"}
        {project.repoRemote ? ` · ${project.repoRemote}` : ""}
        {project.gate?.command ? ` · gate ${project.gate.command}` : ""}
      </p>
      <h2 style={{ fontSize: 14, marginTop: 12 }}>成员工作区（Space 键不合并，D-024）</h2>
      {rows.length === 0 ? <p className={ui.listMeta}>尚无成员工作区。</p> : null}
      {rows.map((row) => (
        <div key={row.key} className={ui.listItem} data-testid="project-member-row" data-space-key={row.key}>
          <span>
            <div>
              {hostLabel(row.member.hostId)} · {row.workspace?.label ?? row.member.workspaceId}
            </div>
            <div className={ui.listMeta}>
              {row.workspace ? row.workspace.rootPath : "工作区尚未注册"} · {row.member.role ?? "member"}
            </div>
          </span>
        </div>
      ))}
    </div>
  );
}
