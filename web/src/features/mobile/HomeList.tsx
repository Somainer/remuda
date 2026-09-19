import { useEffect, useMemo, useRef, useState } from "react";
import { Link, useNavigate } from "react-router-dom";
import { StateDot } from "../../components/StateDot";
import type { Id } from "../../types/wire";
import { hubStore, useHub } from "../../lib/store";
import { buildSpaces, useSpacesPrefs } from "../spaces/store";
import { fetchChanges } from "../files/filesApi";
import {
  buildHomeGroups,
  readHomeOrder,
  writeHomeOrder,
  type HomeOrder,
} from "./homeRows";
import { ContextRing } from "./ContextRing";
import css from "./home.module.css";

type ResumeState = { busy: boolean; error: string | null };

/**
 * The phone home list mounted at `/m` (D-049, ui-spec §4.7).
 *
 * Rows are grouped by project + git branch; each row is the §2.1/D-038 shape
 * (status dot + title + one next-step sentence, error text in the body slot,
 * remaining-context ring). All derivation lives in the pure homeRows.ts —
 * this component only wires the hub store, per-device ordering, live branch
 * hydration and the one-tap resume path (the same `hubStore.resume` the
 * session page uses, never a second implementation).
 */
export function HomeList() {
  const hub = useHub();
  const navigate = useNavigate();
  const prefs = useSpacesPrefs();
  const [query, setQuery] = useState("");
  const [order, setOrder] = useState<HomeOrder>(() => readHomeOrder(localStorageAccess()));
  const [branches, setBranches] = useState<Record<string, string>>({});
  const [resumeStates, setResumeStates] = useState<Record<string, ResumeState>>({});

  const spaces = useMemo(
    () => buildSpaces(hub.workspaces, hub.instances, prefs),
    [hub.workspaces, hub.instances, prefs],
  );

  useEffect(() => {
    writeHomeOrder(order, localStorageAccess());
  }, [order]);

  // Live git branch per project, through the same read-only changes proxy
  // FilesView uses. Unknown/denied/unreachable simply leaves the branch off
  // the header — the group still renders by project name. Candidates are
  // computed during render; the per-space fetch-once guard (a ref) is only
  // read inside the effect.
  const fetchedBranches = useRef<Set<string>>(new Set());
  const branchCandidates = useMemo(
    () =>
      spaces
        .filter((space) => space.hostId && space.workspaceId)
        .map((space) => ({ id: space.id, hostId: space.hostId!, workspaceId: space.workspaceId! })),
    [spaces],
  );
  useEffect(() => {
    const targets = branchCandidates.filter(
      (target) => !fetchedBranches.current.has(target.id),
    );
    if (!targets.length) return;
    let cancelled = false;
    for (const target of targets) fetchedBranches.current.add(target.id);
    void Promise.all(
      targets.map(async (target) => {
        try {
          const status = await fetchChanges(target.hostId, target.workspaceId);
          const branch = status.branch?.state === "known" ? status.branch.value?.trim() : "";
          return { id: target.id, branch: branch ?? "" };
        } catch {
          return { id: target.id, branch: "" };
        }
      }),
    ).then((results) => {
      if (cancelled) return;
      setBranches((current) => {
        const next = { ...current };
        let changed = false;
        for (const { id, branch } of results) {
          if (branch && next[id] !== branch) {
            next[id] = branch;
            changed = true;
          }
        }
        return changed ? next : current;
      });
    });
    return () => {
      cancelled = true;
    };
  }, [branchCandidates]);

  // Mirror SessionList's row hydration cadence: the store projects live
  // phrases and TTY screen DONE markers the next-step sentence needs.
  const topLevelIds = useMemo(
    () => hub.instances.filter((instance) => instance.parent == null).map((instance) => instance.id),
    [hub.instances],
  );
  useEffect(() => {
    const tick = () => {
      if (topLevelIds.length) {
        void hubStore.refreshScreens(topLevelIds as Id[]);
        void hubStore.hydrateRowSummaries(topLevelIds as Id[]);
      }
    };
    tick();
    const timer = window.setInterval(tick, 2500);
    return () => window.clearInterval(timer);
  }, [topLevelIds]);

  const groups = useMemo(
    () =>
      buildHomeGroups({
        spaces,
        interactions: hub.interactions,
        order,
        query,
        titleOf: (instanceId) => hubStore.titleOf(instanceId as Id),
        hostNameOf: (hostId) => hubStore.hostName((hostId ?? "") as Id),
        branchOf: (spaceId) => branches[spaceId] || null,
        rollupOf: (instanceId) => hubStore.usageRollupOf(instanceId as Id),
        screenOf: (instanceId) => hub.screens[instanceId],
        summaryOf: (instanceId) => hubStore.summaryOf(instanceId as Id),
        eventsOf: (instanceId) => hub.events[instanceId],
      }),
    [spaces, hub.interactions, hub.screens, hub.events, order, query, branches],
  );

  async function onResume(instanceId: string) {
    setResumeStates((current) => ({ ...current, [instanceId]: { busy: true, error: null } }));
    // D-026: resume is a new instance inheriting the native session; on
    // success the row must leave for the new id, on failure it stays put and
    // the Hub's error is shown (the store also surfaces it as a toast).
    const nextId = await hubStore.resume(instanceId as Id, "structured");
    if (nextId) {
      navigate(`/s/${nextId}`);
      return;
    }
    setResumeStates((current) => ({
      ...current,
      [instanceId]: { busy: false, error: "恢复失败，请重试" },
    }));
  }

  return (
    <div className={css.home} data-testid="home-list">
      <div className={css.toolbar}>
        <input
          className={css.search}
          data-testid="home-search"
          type="search"
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          placeholder="搜索 会话 / 项目 / 主机"
          aria-label="搜索会话、项目或主机"
        />
        <div className={css.order} role="group" aria-label="排序方式">
          <button
            type="button"
            data-testid="home-order-clock"
            aria-pressed={order === "clock"}
            onClick={() => setOrder("clock")}
          >
            时钟
          </button>
          <button
            type="button"
            data-testid="home-order-list"
            aria-pressed={order === "list"}
            onClick={() => setOrder("list")}
          >
            列表
          </button>
        </div>
      </div>
      {groups.length === 0 ? (
        <div className={css.empty} data-testid="home-empty">
          {query.trim() ? "没有匹配的会话" : "暂无会话"}
        </div>
      ) : (
        <div className={css.groups}>
          {groups.map((group) => (
            <section key={group.id} className={css.group} data-testid="home-group" data-blocked={group.blockedCount}>
              <header className={css.groupHead}>
                <span className={css.groupProject} title={`${group.project} · ${group.hostName}`}>
                  {group.project}
                </span>
                {group.branch ? <span className={css.groupBranch}>{group.branch}</span> : null}
                <span className={css.groupBlocked} data-zero={group.blockedCount === 0 ? "1" : "0"}>
                  {group.blockedCount} 待处理
                </span>
              </header>
              {group.rows.map((row) => {
                const resumeState = resumeStates[row.id];
                return (
                  <article
                    key={row.id}
                    className={css.row}
                    data-testid="home-row"
                    data-status={row.status}
                    data-blocked={row.blocked ? "1" : "0"}
                  >
                    <Link className={css.rowMain} to={`/s/${row.id}`} data-testid="home-row-link">
                      <span className={css.dot}>
                        <StateDot status={row.status} />
                      </span>
                      <span className={css.rowText}>
                        <span className={css.rowTitle} title={row.title}>
                          {row.title}
                        </span>
                        <span
                          className={css.rowBody}
                          data-testid="home-row-body"
                          data-error={row.bodyIsError ? "1" : "0"}
                          title={row.body}
                        >
                          {row.body}
                        </span>
                      </span>
                      <ContextRing pct={row.contextPct} />
                    </Link>
                    {row.canResume ? (
                      <button
                        type="button"
                        className={css.resume}
                        data-testid="home-resume"
                        disabled={resumeState?.busy === true}
                        onClick={(event) => {
                          event.preventDefault();
                          void onResume(row.id);
                        }}
                      >
                        {resumeState?.busy ? "恢复中…" : "恢复"}
                      </button>
                    ) : null}
                    {resumeState?.error ? (
                      <p className={css.rowError} role="status" data-testid="home-resume-error">
                        {resumeState.error}
                      </p>
                    ) : null}
                  </article>
                );
              })}
            </section>
          ))}
        </div>
      )}
    </div>
  );
}

function localStorageAccess(): Storage | null {
  try {
    return typeof localStorage === "undefined" ? null : localStorage;
  } catch {
    return null;
  }
}
