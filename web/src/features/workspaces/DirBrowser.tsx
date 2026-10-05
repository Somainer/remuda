import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Button } from "../../components/Button";
import { Modal } from "../../components/Modal";
import { api } from "../../lib/api";
import { hubStore } from "../../lib/store";
import type { HostDirsListing } from "./dirs";
import type { Workspace } from "../../types/workspace";
import css from "./workspaces.module.css";

type Props = {
  hostId: string;
  open: boolean;
  disabled?: boolean;
  onClose: () => void;
  onRegistered: (workspace: Workspace) => void;
};

/// One breadcrumb segment.
type Crumb = { label: string; path: string };

/**
 * c-dirpicker: browse the chosen host's filesystem through the Node's
 * directories-only `host.dirs.list` RPC, then register the current folder as
 * a workspace. Typing an absolute path remains available behind 高级选项.
 */
export function DirBrowser({ hostId, open, disabled, onClose, onRegistered }: Props) {
  const [listing, setListing] = useState<HostDirsListing | null>(null);
  const [path, setPath] = useState<string | undefined>(undefined);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const [showHidden, setShowHidden] = useState(false);
  const [manualOpen, setManualOpen] = useState(false);
  const [manualPath, setManualPath] = useState("");
  const [busy, setBusy] = useState(false);
  const requestSeq = useRef(0);
  const filterRef = useRef<HTMLInputElement>(null);

  const load = useCallback((nextPath?: string, hidden = showHidden) => {
    const seq = ++requestSeq.current;
    setLoading(true);
    setError(null);
    void api
      .hostDirsList(hostId, { path: nextPath, showHidden: hidden })
      .then((result) => {
        if (seq !== requestSeq.current) return;
        setListing(result);
        setPath(result.path);
        setFilter("");
      })
      .catch((reason: unknown) => {
        if (seq !== requestSeq.current) return;
        setListing(null);
        setError(reason instanceof Error ? reason.message : "读取目录失败");
      })
      .finally(() => {
        if (seq === requestSeq.current) setLoading(false);
      });
    // showHidden is deliberately read at call sites through the parameter.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [hostId]);

  // Reload whenever the modal is (re)opened or the host changes; no path means
  // "Node default start" (home or the first allowed root).
  useEffect(() => {
    if (!open) return;
    setShowHidden(false);
    setManualOpen(false);
    setManualPath("");
    load(undefined, false);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, hostId]);

  const crumbs = useMemo<Crumb[]>(() => {
    if (!listing) return [];
    const boundary = listing.roots
      .filter((root) => listing.path === root || listing.path.startsWith(`${root}/`))
      .sort((a, b) => b.length - a.length)[0];
    if (!boundary) return [{ label: listing.path, path: listing.path }];
    const rest = listing.path === boundary ? [] : listing.path.slice(boundary.length).split("/").filter(Boolean);
    return [
      { label: boundary, path: boundary },
      ...rest.map((_, index) => {
        const segment = rest.slice(0, index + 1).join("/");
        return { label: rest[index], path: `${boundary}/${segment}` };
      }),
    ];
  }, [listing]);

  const visibleDirs = useMemo(() => {
    if (!listing) return [];
    const needle = filter.trim().toLowerCase();
    const rows = needle
      ? listing.dirs.filter((entry) => entry.name.toLowerCase().includes(needle))
      : listing.dirs;
    return rows;
  }, [listing, filter]);

  const registerPath = useCallback(
    async (absolutePath: string) => {
      const trimmed = absolutePath.trim();
      if (!trimmed.startsWith("/")) {
        setError("请输入这台主机上的绝对路径，例如 /opt/projects/app");
        return;
      }
      setBusy(true);
      setError(null);
      try {
        const workspace = await hubStore.registerWorkspace(hostId, trimmed);
        if (workspace) onRegistered(workspace);
        onClose();
      } catch (reason) {
        setError(reason instanceof Error ? reason.message : "添加目录失败");
      } finally {
        setBusy(false);
      }
    },
    [hostId, onClose, onRegistered],
  );

  if (!open) return null;

  return (
    <Modal open={open} onClose={() => { if (!busy) onClose(); }} initialFocusRef={filterRef}>
      <div className={css.browser} data-testid="dir-browser">
        <h2 className={css.browserTitle}>浏览主机目录</h2>
        <p className={css.browserHint}>
          只显示目录，范围限定在这台主机允许注册的根目录内；使用当前文件夹即完成注册，不会创建或删除任何文件。
        </p>
        <div className={css.browserQuick}>
          {listing?.home ? (
            <Button type="button" variant="ghost" data-testid="dir-browser-home"
              disabled={loading || busy} onClick={() => load(listing.home ?? undefined)}>
              主目录
            </Button>
          ) : null}
          {listing?.workspaces.map((root) => (
            <Button key={root} type="button" variant="ghost" data-testid="dir-browser-workspace"
              disabled={loading || busy} onClick={() => load(root)}>
              {root}
            </Button>
          ))}
        </div>
        <div className={css.browserCrumbs}>
          <Button type="button" variant="ghost" data-testid="dir-browser-up"
            disabled={loading || busy || !listing?.parent}
            onClick={() => listing?.parent && load(listing.parent)}>
            ↑ 上一级
          </Button>
          {crumbs.map((crumb, index) => (
            <span key={crumb.path} className={css.crumbWrap}>
              <button type="button" className={css.crumb} data-testid="dir-browser-crumb"
                disabled={loading || busy || index === crumbs.length - 1}
                onClick={() => load(crumb.path)}>
                {index === 0 ? crumb.label : `/${crumb.label}`}
              </button>
            </span>
          ))}
        </div>
        <div className={css.browserControls}>
          <input ref={filterRef} className={css.browserFilter} data-testid="dir-browser-filter"
            type="search" value={filter} disabled={loading || busy}
            placeholder="过滤当前目录…" onChange={(event) => setFilter(event.target.value)} />
          <label className={css.browserToggle}>
            <input type="checkbox" data-testid="dir-browser-hidden" checked={showHidden}
              disabled={loading || busy}
              onChange={(event) => { setShowHidden(event.target.checked); load(path, event.target.checked); }} />
            显示隐藏目录
          </label>
        </div>
        <div className={css.browserList} role="list" aria-busy={loading}>
          {loading ? <div className={css.browserState}>读取中…</div> : null}
          {!loading && error ? <div className={css.error} role="alert">{error}</div> : null}
          {!loading && !error && !visibleDirs.length ? (
            <div className={css.browserState}>{filter ? "没有匹配的目录" : "没有子目录"}</div>
          ) : null}
          {!loading && !error
            ? visibleDirs.map((entry) => (
                <div key={entry.name} role="listitem" className={css.browserItem}>
                  <button type="button"
                    className={css.browserRow} data-testid="dir-browser-row"
                    disabled={busy}
                    onClick={() => load(path ? `${path.replace(/\/$/, "")}/${entry.name}` : entry.name)}>
                    <span className={css.browserFolder}>📁</span>
                    <span className={css.browserName}>{entry.name}</span>
                  </button>
                </div>
              ))
            : null}
        </div>
        {listing?.truncated ? <p className={css.browserTruncated}>目录过多，仅显示前一部分；请进入子目录或使用过滤。</p> : null}
        {manualOpen ? (
          <div className={css.browserManual}>
            <label className={css.field}>主机上的绝对路径
              <input className={css.input} data-testid="dir-browser-manual-path"
                value={manualPath} disabled={busy} autoFocus placeholder="/opt/projects/app"
                onChange={(event) => setManualPath(event.target.value)}
                onKeyDown={(event) => {
                  if (event.key === "Enter" && !event.nativeEvent.isComposing && !busy && !disabled) {
                    event.preventDefault();
                    void registerPath(manualPath);
                  }
                }} />
            </label>
            <Button type="button" data-testid="dir-browser-manual-cancel"
              disabled={busy} onClick={() => setManualOpen(false)}>
              返回浏览
            </Button>
          </div>
        ) : null}
        <div className={css.browserActions}>
          <Button type="button" data-testid="dir-browser-manual-toggle" variant="ghost"
            disabled={busy} onClick={() => setManualOpen((value) => !value)}>
            {manualOpen ? "收起手动输入" : "高级：手动输入路径"}
          </Button>
          <span className={css.browserSpacer} />
          <Button type="button" data-testid="dir-browser-cancel" disabled={busy} onClick={onClose}>
            取消
          </Button>
          {manualOpen ? (
            <Button type="button" variant="primary" data-testid="dir-browser-manual-submit"
              disabled={busy || disabled || !manualPath.trim()}
              onClick={() => void registerPath(manualPath)}>
              {busy ? "添加中…" : "添加该路径"}
            </Button>
          ) : (
            <Button type="button" variant="primary" data-testid="dir-browser-use"
              disabled={busy || disabled || !listing || loading}
              onClick={() => listing && void registerPath(listing.path)}>
              {busy ? "添加中…" : `使用此文件夹${listing ? `：${listing.path}` : ""}`}
            </Button>
          )}
        </div>
      </div>
    </Modal>
  );
}
