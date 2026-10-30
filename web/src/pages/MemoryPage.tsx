/**
 * `/s/:instanceId/memory` — the lineage's memory (D-060 §12.3, cb-memory-ui).
 *
 * The session page opens this from its ⋯ menu (「记忆」), so the Main seat can
 * reuse the same route later. Entries are grouped by type and searched through
 * the Hub (FTS5 first, LIKE for CJK). Every edit is a revision CAS: a 409 is
 * shown as a choice between the server copy and the owner's text, never an
 * overwrite.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useParams, useSearchParams } from "react-router-dom";
import { api } from "../lib/api";
import { HubHttpError } from "../lib/httpError";
import type { Id } from "../types/wire";
import { PageHeader } from "../components/PageHeader";
import { Button } from "../components/Button";
import { Modal } from "../components/Modal";
import {
  ABOUT_ME_SLUG,
  MEMORY_TYPES,
  MEMORY_TYPE_LABEL,
  createMemory,
  deleteMemory,
  getMemory,
  listMemory,
  restoreMemory,
  searchMemory,
  updateMemory,
  type MemoryDraft,
  type MemoryEntry,
  type MemorySizeWarning,
  type MemoryType,
} from "../features/memory/api";
import css from "./memory.module.css";

const ABOUT_ME_TITLE = "关于我";
const ABOUT_ME_DESCRIPTION = "所有者写下的自我介绍";

type EditorState = {
  slug: string;
  creating: boolean;
  revision: number;
  draft: MemoryDraft;
};

function draftOf(entry: MemoryEntry): MemoryDraft {
  return {
    type: entry.type,
    title: entry.title,
    description: entry.description,
    body: entry.body,
  };
}

function warningText(warnings: readonly MemorySizeWarning[]): string[] {
  return warnings.map((warning) =>
    warning.limit === "entry-soft"
      ? `这条记忆 ${warning.bytes} 字节，超过 ${warning.maxBytes} 字节的软上限（仍已保存）。`
      : `记忆索引 ${warning.bytes} 字节，超过 ${warning.maxBytes} 字节的软上限（仍已保存）。`,
  );
}

/**
 * The Hub's refusal, in the page's own words. A secret rejection's text is
 * never echoed: the Hub's message already omits it, and nothing here adds the
 * submitted body.
 */
function saveErrorText(error: unknown): string {
  if (error instanceof HubHttpError) {
    if (error.code === "MEMORY_ENTRY_TOO_LARGE") {
      return `这条记忆太长，Hub 拒绝保存。${error.message}`;
    }
    if (error.code === "MEMORY_SECRET_REJECTED") {
      return "内容看起来包含秘密（密钥、令牌或密码），Hub 拒绝保存。请删掉那一段再试。";
    }
    if (error.code === "MEMORY_ARCHIVED") {
      return "这条记忆已归档，只能恢复后再编辑。";
    }
    if (error.code === "MEMORY_SLUG_EXISTS") {
      return "这个 slug 已经存在，请直接编辑它。";
    }
    return error.message;
  }
  return error instanceof Error ? error.message : String(error);
}

function isRevisionConflict(error: unknown): error is HubHttpError {
  return error instanceof HubHttpError && error.code === "MEMORY_REVISION_CONFLICT";
}

export function MemoryPage() {
  const { instanceId = "" } = useParams();
  const [params, setParams] = useSearchParams();
  const q = params.get("q") ?? "";
  const slugParam = params.get("slug");
  const [lineageId, setLineageId] = useState<string | null>(null);
  const [entries, setEntries] = useState<MemoryEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [noLineage, setNoLineage] = useState(false);
  const [editor, setEditor] = useState<EditorState | null>(null);
  const [editorError, setEditorError] = useState<string | null>(null);
  const [editorWarnings, setEditorWarnings] = useState<string[]>([]);
  const [saving, setSaving] = useState(false);
  const [conflict, setConflict] = useState<{
    mine: MemoryDraft;
    server: MemoryEntry;
    kind: "entry" | "about";
  } | null>(null);
  const [confirmDelete, setConfirmDelete] = useState<MemoryEntry | null>(null);
  const [aboutDraft, setAboutDraft] = useState("");
  const [aboutError, setAboutError] = useState<string | null>(null);
  const [aboutWarnings, setAboutWarnings] = useState<string[]>([]);
  const [aboutSaving, setAboutSaving] = useState(false);
  const [restoreError, setRestoreError] = useState<string | null>(null);
  const [highlight, setHighlight] = useState<string | null>(null);
  const searchRef = useRef<HTMLInputElement>(null);
  const rowRefs = useRef(new Map<string, HTMLDivElement>());

  // The memory route is keyed by lineage; a chapter instance resolves first.
  useEffect(() => {
    let live = true;
    setLineageId(null);
    (async () => {
      try {
        const instance = await api.instanceGet(instanceId as Id);
        if (!live) return;
        setLineageId(instance.lineageId ?? instanceId);
      } catch (error) {
        if (!live) return;
        // A retired lineage has no session row any more, but its memory is
        // archived, not deleted (D-060 §12.4): a first chapter's instance id
        // IS its lineage id, so the owner can still reach the archived
        // entries (and 恢复 them) from a bookmarked route.
        if (error instanceof HubHttpError && error.status === 404) {
          setLineageId(instanceId);
          return;
        }
        setLoadError(`读取会话失败：${saveErrorText(error)}`);
        setLoading(false);
      }
    })();
    return () => {
      live = false;
    };
  }, [instanceId]);

  const reload = useCallback(
    async (lineage: string, query: string) => {
      setLoading(true);
      setLoadError(null);
      try {
        const items = query.trim()
          ? await searchMemory(lineage, query.trim(), true)
          : await listMemory(lineage, true);
        setEntries(items);
        setNoLineage(false);
      } catch (error) {
        // A plain (non-continuity) instance has no lineage row yet, so it has
        // no memory to show: that is a state, not a failure.
        if (error instanceof HubHttpError && error.status === 404) {
          setEntries([]);
          setNoLineage(true);
        } else {
          setLoadError(saveErrorText(error));
        }
      } finally {
        setLoading(false);
      }
    },
    [],
  );

  useEffect(() => {
    if (!lineageId) return;
    void reload(lineageId, q);
  }, [lineageId, q, reload]);

  const live = useMemo(() => entries.filter((entry) => entry.archivedAt == null), [entries]);
  const archived = useMemo(() => entries.filter((entry) => entry.archivedAt != null), [entries]);
  const aboutMe = useMemo(() => live.find((entry) => entry.slug === ABOUT_ME_SLUG) ?? null, [live]);
  const aboutMeArchived = useMemo(
    () => archived.find((entry) => entry.slug === ABOUT_ME_SLUG) ?? null,
    [archived],
  );
  const grouped = useMemo(() => {
    const groups = new Map<MemoryType, MemoryEntry[]>();
    for (const entry of live) {
      if (entry.slug === ABOUT_ME_SLUG) continue;
      const rows = groups.get(entry.type) ?? [];
      rows.push(entry);
      groups.set(entry.type, rows);
    }
    return groups;
  }, [live]);

  useEffect(() => {
    setAboutDraft(aboutMe?.body ?? "");
  }, [aboutMe]);

  // A 「已记住」 row links here with ?slug=: scroll to it and mark it once.
  useEffect(() => {
    if (!slugParam || loading) return;
    const node = rowRefs.current.get(slugParam);
    if (!node) return;
    setHighlight(slugParam);
    node.scrollIntoView({ block: "center" });
    const timer = window.setTimeout(() => setHighlight(null), 2400);
    return () => window.clearTimeout(timer);
  }, [slugParam, loading, entries]);

  function setQuery(next: string) {
    const copy = new URLSearchParams(params);
    if (next) copy.set("q", next);
    else copy.delete("q");
    copy.delete("slug");
    setParams(copy, { replace: true });
  }

  function openEditor(entry: MemoryEntry) {
    if (entry.archivedAt != null) return;
    setEditorError(null);
    setEditorWarnings([]);
    setEditor({
      slug: entry.slug,
      creating: false,
      revision: entry.revision,
      draft: draftOf(entry),
    });
  }

  function openCreate(kind: MemoryType) {
    setEditorError(null);
    setEditorWarnings([]);
    setEditor({
      slug: "",
      creating: true,
      revision: 0,
      draft: { type: kind, title: "", description: "", body: "" },
    });
  }

  async function saveEditor() {
    if (!lineageId || !editor) return;
    if (editor.creating && !/^[a-z0-9][a-z0-9._-]*$/.test(editor.slug.trim())) {
      setEditorError("slug 只能用小写字母、数字、'.'、'_' 或 '-'，且以字母或数字开头。");
      return;
    }
    setSaving(true);
    setEditorError(null);
    setEditorWarnings([]);
    try {
      const result = editor.creating
        ? await createMemory(lineageId, { ...editor.draft, slug: editor.slug.trim() })
        : await updateMemory(lineageId, editor.slug, editor.revision, editor.draft);
      const warnings = warningText(result.warnings);
      await reload(lineageId, q);
      if (warnings.length) {
        setEditorWarnings(warnings);
        setEditor({ ...editor, creating: false, revision: result.entry.revision });
      } else {
        setEditor(null);
      }
    } catch (error) {
      if (isRevisionConflict(error) && !editor.creating) {
        try {
          const server = await getMemory(lineageId, editor.slug);
          setEditor(null);
          setConflict({ mine: editor.draft, server, kind: "entry" });
        } catch (readError) {
          setEditorError(saveErrorText(readError));
        }
      } else {
        setEditorError(saveErrorText(error));
      }
    } finally {
      setSaving(false);
    }
  }

  async function resolveConflict(choice: "latest" | "mine") {
    if (!lineageId || !conflict) return;
    if (choice === "latest") {
      const { server, kind } = conflict;
      setConflict(null);
      if (kind === "about") {
        setAboutDraft(server.body);
        setAboutError("已载入服务器上的最新版本。");
        return;
      }
      setEditorError(null);
      setEditorWarnings([]);
      setEditor({
        slug: server.slug,
        creating: false,
        revision: server.revision,
        draft: draftOf(server),
      });
      return;
    }
    setSaving(true);
    const kind = conflict.kind;
    try {
      const result = await updateMemory(
        lineageId,
        conflict.server.slug,
        conflict.server.revision,
        // The inline about-me editor only owns the body; never reset its
        // title/description to the defaults on a conflict resolution.
        kind === "about" ? { body: conflict.mine.body } : conflict.mine,
      );
      setConflict(null);
      await reload(lineageId, q);
      const warnings = warningText(result.warnings);
      if (kind === "about") {
        setAboutDraft(result.entry.body);
        setAboutError(null);
        setAboutWarnings(warnings);
        return;
      }
      setEditorWarnings(warnings);
      setEditorError(null);
      if (warnings.length) {
        setEditor({
          slug: result.entry.slug,
          creating: false,
          revision: result.entry.revision,
          draft: draftOf(result.entry),
        });
      } else {
        setEditor(null);
      }
    } catch (error) {
      setConflict(null);
      if (kind === "about") {
        setAboutDraft(conflict.mine.body);
        setAboutError(saveErrorText(error));
        return;
      }
      setEditorError(saveErrorText(error));
      setEditor({
        slug: conflict.server.slug,
        creating: false,
        revision: conflict.server.revision,
        draft: conflict.mine,
      });
    } finally {
      setSaving(false);
    }
  }

  async function removeEntry() {
    if (!lineageId || !confirmDelete) return;
    setSaving(true);
    try {
      await deleteMemory(lineageId, confirmDelete.slug, confirmDelete.revision);
      setConfirmDelete(null);
      setEditor(null);
      await reload(lineageId, q);
    } catch (error) {
      setConfirmDelete(null);
      setEditorError(saveErrorText(error));
      setEditor({
        slug: confirmDelete.slug,
        creating: false,
        revision: confirmDelete.revision,
        draft: draftOf(confirmDelete),
      });
    } finally {
      setSaving(false);
    }
  }

  async function saveAbout() {
    if (!lineageId) return;
    setAboutSaving(true);
    setAboutError(null);
    setAboutWarnings([]);
    try {
      const result = aboutMe
        ? await updateMemory(lineageId, ABOUT_ME_SLUG, aboutMe.revision, { body: aboutDraft })
        : await createMemory(lineageId, {
            slug: ABOUT_ME_SLUG,
            type: "user",
            title: ABOUT_ME_TITLE,
            description: ABOUT_ME_DESCRIPTION,
            body: aboutDraft,
          });
      setAboutWarnings(warningText(result.warnings));
      await reload(lineageId, q);
    } catch (error) {
      if (isRevisionConflict(error)) {
        // Another tab of the owner changed it (about-me is owner-authored):
        // show both bodies and let the owner choose, never overwrite.
        try {
          const server = await getMemory(lineageId, ABOUT_ME_SLUG);
          setConflict({
            mine: {
              type: "user",
              title: ABOUT_ME_TITLE,
              description: ABOUT_ME_DESCRIPTION,
              body: aboutDraft,
            },
            server,
            kind: "about",
          });
        } catch (readError) {
          setAboutError(saveErrorText(readError));
        }
      } else {
        setAboutError(saveErrorText(error));
      }
    } finally {
      setAboutSaving(false);
    }
  }

  async function restoreEntry(entry: MemoryEntry) {
    if (!lineageId) return;
    setRestoreError(null);
    try {
      await restoreMemory(lineageId, [entry.slug]);
      await reload(lineageId, q);
    } catch (error) {
      setRestoreError(`恢复失败：${saveErrorText(error)}`);
    }
  }

  function row(entry: MemoryEntry, readonly: boolean) {
    const archivedRow = entry.archivedAt != null;
    return (
      <div
        key={entry.slug}
        ref={(node) => {
          if (node) rowRefs.current.set(entry.slug, node);
          else rowRefs.current.delete(entry.slug);
        }}
        className={[
          css.row,
          archivedRow ? css.rowStatic : "",
          highlight === entry.slug ? css.rowHighlight : "",
        ]
          .join(" ")
          .trim()}
        data-testid="memory-row"
        data-slug={entry.slug}
        data-type={entry.type}
        data-archived={archivedRow ? "1" : "0"}
        role={readonly || archivedRow ? undefined : "button"}
        tabIndex={readonly || archivedRow ? undefined : 0}
        aria-label={readonly || archivedRow ? undefined : `编辑记忆 ${entry.title}`}
        onClick={() => {
          if (!readonly && !archivedRow) openEditor(entry);
        }}
        onKeyDown={(event) => {
          if (readonly || archivedRow) return;
          if (event.key === "Enter" || event.key === " ") {
            event.preventDefault();
            openEditor(entry);
          }
        }}
      >
        <div className={css.rowHead}>
          <span className={css.rowTitle}>{entry.title}</span>
          {archivedRow ? <span className={css.tag}>已归档</span> : null}
        </div>
        <p className={css.desc}>{entry.description}</p>
        <p className={[css.bodyText, highlight === entry.slug ? css.bodyFull : ""].join(" ").trim()}>
          {entry.body}
        </p>
        <div className={css.rowMeta}>
          <span>rev {entry.revision}</span>
          <span>{entry.origin === "human" ? "所有者" : entry.writerInstance ?? "席位"}</span>
          {entry.turnRef ? <span>turn {entry.turnRef.seq}</span> : null}
          {archivedRow ? (
            <span className={css.actions}>
              <Button
                data-testid="memory-restore"
                onClick={(event) => {
                  event.stopPropagation();
                  void restoreEntry(entry);
                }}
              >
                恢复
              </Button>
            </span>
          ) : null}
        </div>
      </div>
    );
  }

  const hasAnything = live.length > 0 || archived.length > 0;

  return (
    <div className={css.page} data-testid="memory-page" data-lineage={lineageId ?? ""}>
      <PageHeader
        testId="memory-header"
        crumbs={[{ label: "会话", to: `/s/${instanceId}` }]}
        title="记忆"
        actions={
          <Button data-testid="memory-new" onClick={() => openCreate("user")}>
            新建
          </Button>
        }
      />
      <div className={css.body}>
        <div className={css.inner}>
          <div className={css.toolbar}>
            <input
              ref={searchRef}
              className={css.search}
              type="search"
              value={q}
              placeholder="搜索记忆（FTS5，中文按子串）"
              aria-label="搜索记忆"
              data-testid="memory-search"
              onChange={(event) => setQuery(event.target.value)}
            />
            <span className={css.meta} data-testid="memory-count">
              {loading ? "加载中…" : `${live.length} 条${archived.length ? ` · ${archived.length} 条已归档` : ""}`}
            </span>
          </div>

          {loadError ? (
            <p className={css.error} role="alert" data-testid="memory-error">
              {loadError}
            </p>
          ) : null}
          {restoreError ? (
            <p className={css.error} role="alert" data-testid="memory-restore-error">
              {restoreError}
            </p>
          ) : null}

          <section className={css.aboutCard} data-testid="memory-about" aria-labelledby="memory-about-title">
            <div className={css.aboutHead}>
              <h2 className={css.sectionTitle} id="memory-about-title">
                {ABOUT_ME_TITLE}
              </h2>
              <span className={css.meta}>
                {aboutMeArchived
                  ? "已归档（只读）；恢复后可编辑"
                  : aboutMe
                    ? `rev ${aboutMe.revision} · 只有所有者能写`
                    : "还没有；保存即创建"}
              </span>
            </div>
            {aboutMeArchived ? (
              <>
                <p className={css.bodyText} data-testid="memory-about-archived">
                  {aboutMeArchived.body}
                </p>
                <div className={css.editorActions}>
                  <Button data-testid="memory-about-restore" onClick={() => void restoreEntry(aboutMeArchived)}>
                    恢复
                  </Button>
                </div>
              </>
            ) : (
              <>
                <textarea
                  className={`${css.textarea} ${css.aboutInput}`}
                  value={aboutDraft}
                  placeholder="写下关于你自己的信息，席位会在章节开始时读到索引。"
                  aria-label="关于我"
                  data-testid="memory-about-body"
                  onChange={(event) => setAboutDraft(event.target.value)}
                />
                {aboutError ? (
                  <p className={css.error} role="alert" data-testid="memory-about-error">
                    {aboutError}
                  </p>
                ) : null}
                {aboutWarnings.map((warning) => (
                  <p key={warning} className={css.warnings} data-testid="memory-about-warning">
                    {warning}
                  </p>
                ))}
                <div className={css.editorActions}>
                  <Button
                    variant="primary"
                    data-testid="memory-about-save"
                    disabled={aboutSaving || !lineageId}
                    onClick={() => void saveAbout()}
                  >
                    {aboutSaving ? "保存中…" : "保存"}
                  </Button>
                </div>
              </>
            )}
          </section>

          {!loading && !loadError && !hasAnything ? (
            <p className={css.empty} data-testid="memory-empty">
              {noLineage
                ? "这个会话还不是连续血缘，记忆从连续会话开始记录。"
                : q.trim()
                  ? "没有匹配的记忆。"
                  : "这个会话还没有记忆。"}
            </p>
          ) : null}

          {MEMORY_TYPES.map((kind) => {
            const rows = grouped.get(kind) ?? [];
            if (!rows.length) return null;
            return (
              <section
                key={kind}
                className={css.group}
                data-testid={`memory-group-${kind}`}
                aria-labelledby={`memory-group-title-${kind}`}
              >
                <h2 className={css.sectionTitle} id={`memory-group-title-${kind}`}>
                  {MEMORY_TYPE_LABEL[kind]}
                </h2>
                <div className={css.list}>{rows.map((entry) => row(entry, false))}</div>
              </section>
            );
          })}

          {archived.length > 0 ? (
            <section className={css.group} data-testid="memory-group-archived" aria-labelledby="memory-archived-title">
              <h2 className={css.sectionTitle} id="memory-archived-title">
                已归档
              </h2>
              <div className={css.list}>{archived.map((entry) => row(entry, true))}</div>
            </section>
          ) : null}
        </div>
      </div>

      <Modal open={editor !== null} onClose={() => !saving && setEditor(null)} labelledBy="memory-editor-title">
        {editor ? (
          <form
            className={css.editor}
            data-testid="memory-editor"
            onSubmit={(event) => {
              event.preventDefault();
              void saveEditor();
            }}
          >
            <div className={css.editorHead}>
              <h2 className={css.editorTitle} id="memory-editor-title">
                {editor.creating ? "新建记忆" : "编辑记忆"}
              </h2>
              <span className={css.meta}>
                {editor.creating ? "revision 0" : `rev ${editor.revision}`}
              </span>
            </div>
            {editor.creating ? (
              <label className={css.field}>
                <span className={css.label}>slug</span>
                <input
                  className={css.input}
                  value={editor.slug}
                  data-testid="memory-editor-slug"
                  onChange={(event) => setEditor({ ...editor, slug: event.target.value })}
                />
              </label>
            ) : null}
            <label className={css.field}>
              <span className={css.label}>类型</span>
              <select
                className={css.select}
                value={editor.draft.type}
                data-testid="memory-editor-type"
                onChange={(event) =>
                  setEditor({
                    ...editor,
                    draft: { ...editor.draft, type: event.target.value as MemoryType },
                  })
                }
              >
                {MEMORY_TYPES.map((kind) => (
                  <option key={kind} value={kind}>
                    {MEMORY_TYPE_LABEL[kind]}
                  </option>
                ))}
              </select>
            </label>
            <label className={css.field}>
              <span className={css.label}>标题</span>
              <input
                className={css.input}
                value={editor.draft.title}
                data-testid="memory-editor-title-input"
                onChange={(event) =>
                  setEditor({ ...editor, draft: { ...editor.draft, title: event.target.value } })
                }
              />
            </label>
            <label className={css.field}>
              <span className={css.label}>一句话描述</span>
              <input
                className={css.input}
                value={editor.draft.description}
                data-testid="memory-editor-description"
                onChange={(event) =>
                  setEditor({
                    ...editor,
                    draft: { ...editor.draft, description: event.target.value },
                  })
                }
              />
            </label>
            <label className={css.field}>
              <span className={css.label}>正文</span>
              <textarea
                className={css.textarea}
                value={editor.draft.body}
                data-testid="memory-editor-body"
                onChange={(event) =>
                  setEditor({ ...editor, draft: { ...editor.draft, body: event.target.value } })
                }
              />
            </label>
            {editorError ? (
              <p className={css.error} role="alert" data-testid="memory-editor-error">
                {editorError}
              </p>
            ) : null}
            {editorWarnings.map((warning) => (
              <p key={warning} className={css.warnings} data-testid="memory-editor-warning">
                {warning}
              </p>
            ))}
            <div className={css.editorActions}>
              {!editor.creating ? (
                <Button
                  data-testid="memory-editor-delete"
                  disabled={saving}
                  onClick={() => {
                    const entry = entries.find((row) => row.slug === editor.slug);
                    if (entry) setConfirmDelete(entry);
                  }}
                >
                  删除
                </Button>
              ) : null}
              <Button data-testid="memory-editor-cancel" disabled={saving} onClick={() => setEditor(null)}>
                取消
              </Button>
              <Button variant="primary" type="submit" data-testid="memory-editor-save" disabled={saving}>
                {saving ? "保存中…" : "保存"}
              </Button>
            </div>
          </form>
        ) : null}
      </Modal>

      <Modal
        open={conflict !== null}
        onClose={() => !saving && setConflict(null)}
        labelledBy="memory-conflict-title"
      >
        {conflict ? (
          <div className={css.conflict} data-testid="memory-conflict">
            <h2 className={css.editorTitle} id="memory-conflict-title">
              这条记忆已被别处修改
            </h2>
            <p className={css.meta}>
              服务器现在是 rev {conflict.server.revision}；你手上的草稿基于旧版本。
              选择保留哪一份，绝不静默覆盖。
            </p>
            <div className={css.conflictGrid}>
              <div className={css.conflictPane}>
                <span className={css.label}>服务器版本</span>
                <pre className={css.conflictBody} data-testid="memory-conflict-server">
                  {conflict.server.body}
                </pre>
              </div>
              <div className={css.conflictPane}>
                <span className={css.label}>我的版本</span>
                <pre className={css.conflictBody} data-testid="memory-conflict-mine">
                  {conflict.mine.body}
                </pre>
              </div>
            </div>
            <div className={css.editorActions}>
              <Button
                data-testid="memory-conflict-latest"
                disabled={saving}
                onClick={() => void resolveConflict("latest")}
              >
                使用最新
              </Button>
              <Button
                variant="primary"
                data-testid="memory-conflict-mine-keep"
                disabled={saving}
                onClick={() => void resolveConflict("mine")}
              >
                保留我的
              </Button>
            </div>
          </div>
        ) : null}
      </Modal>

      <Modal
        open={confirmDelete !== null}
        onClose={() => !saving && setConfirmDelete(null)}
        labelledBy="memory-delete-title"
      >
        {confirmDelete ? (
          <div className={css.confirm} data-testid="memory-delete-confirm">
            <h2 className={css.editorTitle} id="memory-delete-title">
              删除这条记忆？
            </h2>
            <p className={css.confirmText}>
              「{confirmDelete.title}」（rev {confirmDelete.revision}）会被删除，不能撤销。
            </p>
            <div className={css.editorActions}>
              <Button data-testid="memory-delete-cancel" disabled={saving} onClick={() => setConfirmDelete(null)}>
                取消
              </Button>
              <Button variant="danger" data-testid="memory-delete-confirm-yes" disabled={saving} onClick={() => void removeEntry()}>
                {saving ? "删除中…" : "删除"}
              </Button>
            </div>
          </div>
        ) : null}
      </Modal>
    </div>
  );
}
