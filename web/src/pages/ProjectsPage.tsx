import { useEffect, useState, type CSSProperties } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import { useHub } from "../lib/store";
import { rest } from "../lib/api";
import { PageHeader } from "../components/PageHeader";
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

/**
 * The §4.10 surface list. Projects has no module stylesheet in this batch, so
 * the one-list wrapper is an inline layout value speaking in role tokens —
 * never a font size or a raw colour.
 */
const SURFACE_LIST: CSSProperties = {
  overflow: "hidden",
  border: "1px solid var(--border)",
  borderRadius: "var(--radius-md)",
  background: "var(--bg-surface)",
};

const BODY: CSSProperties = {
  flex: "1 1 auto",
  minHeight: 0,
  overflow: "auto",
  padding: "var(--space-5) var(--gutter) 40px",
};

const INNER: CSSProperties = {
  display: "flex",
  flexDirection: "column",
  gap: "var(--space-5)",
  maxWidth: 880,
};

export function ProjectsPage() {
  const hub = useHub();
  const navigate = useNavigate();
  const { projects, loading, error, reload } = useProjects();
  const selected = useProjectFilter();
  const visible = selected ? projects.filter((project) => project.id === selected) : projects;
  const hostLabel = (hostId: string) =>
    hub.hosts.find((host) => host.id === hostId)?.label ?? hostId;

  /**
   * Clicking a project row is an explicit scope choice: it selects the
   * project and opens its page. Mounting the detail route (deep link /
   * back-forward) does not touch the global filter — only this action and
   * the switcher change scope.
   */
  function openProject(projectId: string) {
    projectFilterStore.select(projectId);
    navigate(`/projects/${encodeURIComponent(projectId)}`);
  }

  return (
    <div style={{ flex: "1 1 auto", minHeight: 0, display: "flex", flexDirection: "column" }} data-testid="projects-page">
      <PageHeader
        title="项目"
        actions={<ProjectSwitcher projects={projects} navigateOnSelect />}
      />
      <div style={BODY}>
        <div style={INNER}>
          <p className={ui.listMeta}>
            Hub Project 实体 · 成员经 (host, workspace) 映射到 Space，不合并同名或跨主机目录
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
          <div style={SURFACE_LIST}>
            {visible.map((project) => {
              const hosts = projectMemberHostIds(project);
              return (
                <Link
                  key={project.id}
                  to={`/projects/${project.id}`}
                  className={ui.listItem}
                  data-testid="project-row"
                  onClick={() => openProject(project.id)}
                >
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
        </div>
      </div>
    </div>
  );
}

/**
 * /projects/:id — one authoritative Project. The route param is the project
 * id (the route name predates Project adoption). Members resolve through the
 * exact Space key: a member whose workspace is not registered on that host
 * renders as the bare host/workspace pair instead of being merged away.
 *
 * Mounting this route does NOT change the global project filter: a deep link
 * or back/forward navigation must not silently rescope the task list and
 * board. The scope changes only through an explicit switcher action (the
 * list-page switcher both selects and navigates; the detail-page switcher
 * navigates away on global).
 */
export function ProjectDetailPage() {
  const { workspaceId: projectId = "" } = useParams();
  const hub = useHub();
  const [project, setProject] = useState<Project | null>(null);
  const [status, setStatus] = useState<"loading" | "ready" | "missing">("loading");

  useEffect(() => {
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
      <div style={{ flex: "1 1 auto", minHeight: 0, display: "flex", flexDirection: "column" }}>
        <PageHeader crumbs={[{ label: "项目", to: "/projects" }]} title="项目" />
        <div style={BODY}>
          <p className={ui.listMeta}>加载中…</p>
        </div>
      </div>
    );
  }
  if (status === "missing" || !project) {
    return (
      <div style={{ flex: "1 1 auto", minHeight: 0, display: "flex", flexDirection: "column" }}>
        <PageHeader crumbs={[{ label: "项目", to: "/projects" }]} title="项目不存在" />
        <div style={BODY}>
          <p className={ui.listMeta}>
            <Link to="/projects">返回项目列表</Link>
          </p>
        </div>
      </div>
    );
  }

  const rows = projectMemberRows(project, hub.workspaces);
  const hostLabel = (hostId: string) =>
    hub.hosts.find((host) => host.id === hostId)?.label ?? hostId;

  return (
    <div style={{ flex: "1 1 auto", minHeight: 0, display: "flex", flexDirection: "column" }}>
      <PageHeader
        crumbs={[{ label: "项目", to: "/projects" }]}
        title={project.name}
        actions={<ProjectSwitcher projects={[project]} navigateOnSelect />}
      />
      <div style={BODY}>
        <div style={INNER}>
          <p className={ui.path}>
            基线 {project.defaultBaseBranch ?? "main"} · 分支 {project.branchPattern ?? "wt/{worker}/{topic}"}
            {project.repoRemote ? ` · ${project.repoRemote}` : ""}
            {project.gate?.command ? ` · gate ${project.gate.command}` : ""}
          </p>
          <section>
            <h2 className={ui.groupTitle} style={{ padding: 0, margin: "0 0 var(--space-3)" }}>
              成员工作区（Space 键不合并）
            </h2>
            {rows.length === 0 ? <p className={ui.listMeta}>尚无成员工作区。</p> : (
              <div style={SURFACE_LIST}>
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
            )}
          </section>
        </div>
      </div>
    </div>
  );
}
