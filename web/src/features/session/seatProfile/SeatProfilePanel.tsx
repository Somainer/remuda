import { useCallback, useEffect, useState } from "react";
import { codexPolicyTable, launchPermissionTable } from "../permissions";
import {
  CLASS_LABEL,
  EFFORT_LEVELS,
  UNAVAILABLE_LABEL,
  conflictRevision,
  getSeatProfile,
  isProfileConflict,
  putSeatProfile,
  type SeatProfileView,
} from "./api";
import { bodyOf, draftOf, type Draft } from "./draft";
import css from "./seatProfile.module.css";

function modeOptions(kind: string) {
  return kind === "codex" ? codexPolicyTable() : launchPermissionTable(kind);
}

/**
 * cb-seat-profile (D-063, main-agent.md §3.4.2): Main's lineage profile on
 * its session page. A one-line summary (revision, default model, the
 * 「待下一章节生效」 badge while the live chapter runs an older revision, and
 * how many items the last launch could not deliver), details on demand, and
 * the owner's editor: a save is a revision compare-and-set, and a 409 asks to
 * reload rather than overwrite. The PWA signs in only as a Human device; the
 * Hub still refuses any other caller.
 */
export function SeatProfilePanel({
  lineageId,
  kind,
  liveInstanceId,
  currentMode,
}: {
  lineageId: string;
  kind: string;
  /** The live chapter; a new chapter re-reads the profile (the badge clears). */
  liveInstanceId: string;
  /** The live chapter's mode, the editor's default before a first edit. */
  currentMode: string;
}) {
  const [view, setView] = useState<SeatProfileView | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [open, setOpen] = useState(false);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [initial, setInitial] = useState<Draft | null>(null);
  const [saving, setSaving] = useState(false);
  const [conflict, setConflict] = useState<{ revision: number | null } | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const next = await getSeatProfile(lineageId);
      setView(next);
      setLoadError(null);
      return next;
    } catch (error) {
      setLoadError(error instanceof Error ? error.message : "无法读取座位配置");
      return null;
    }
  }, [lineageId]);

  useEffect(() => {
    void load();
  }, [load, liveInstanceId]);

  if (!view) {
    return loadError ? (
      <div className={css.panel} data-testid="seat-profile">
        <p className={css.note}>座位配置读取失败：{loadError}</p>
      </div>
    ) : null;
  }

  const profile = view.profile ?? null;
  const unavailable = view.lastLaunch?.payload.unavailable ?? [];
  const refusal = view.lastLaunch?.payload.refusal ?? null;
  const models = profile?.modelCandidates ?? [];

  const startEdit = () => {
    const base = draftOf(profile, currentMode);
    setDraft(base);
    setInitial(base);
    setConflict(null);
    setSaveError(null);
    setOpen(true);
  };

  const save = async () => {
    if (!draft || !initial) return;
    setSaving(true);
    setSaveError(null);
    try {
      const result = await putSeatProfile(lineageId, bodyOf(draft, initial, profile?.revision ?? 0));
      setView({ ...view, profile: result.profile, pending: result.pending });
      setDraft(null);
    } catch (error) {
      if (isProfileConflict(error)) setConflict({ revision: conflictRevision(error) });
      else setSaveError(error instanceof Error ? error.message : "保存失败");
    } finally {
      setSaving(false);
    }
  };

  const reload = async () => {
    const next = await load();
    setConflict(null);
    if (next && draft) {
      const base = draftOf(next.profile, currentMode);
      setDraft(base);
      setInitial(base);
    }
  };

  return (
    <div className={css.panel} data-testid="seat-profile">
      <div className={css.summary}>
        <span className={css.lead}>座位配置</span>
        {profile ? (
          <>
            <span className={css.item} data-testid="seat-profile-revision">
              修订 {profile.revision}
            </span>
            <span className={css.item} title={models.join("\n")}>
              默认模型 {models[0] ?? "—"}
              {models.length > 1 ? `（另 ${models.length - 1} 个候选）` : ""}
            </span>
          </>
        ) : (
          <span className={css.item}>未设置 · 沿用创建时配置</span>
        )}
        {view.pending ? (
          <span className={css.badge} data-testid="seat-profile-pending" title="编辑在下一章节（重启或恢复）时生效">
            待下一章节生效
          </span>
        ) : null}
        {unavailable.length > 0 ? (
          <span className={css.item} data-tone="attention" data-testid="seat-profile-unavailable-count">
            上次启动 {unavailable.length} 项不可用
          </span>
        ) : null}
        <span className={css.controls}>
          <button
            type="button"
            className={css.btn}
            data-testid="seat-profile-toggle"
            aria-expanded={open}
            onClick={() => setOpen((value) => !value)}
          >
            {open ? "收起" : "详情"}
          </button>
          {draft ? null : (
            <button type="button" className={css.btn} data-testid="seat-profile-edit" onClick={startEdit}>
              编辑
            </button>
          )}
        </span>
      </div>

      {open && !draft ? (
        <dl className={css.details} data-testid="seat-profile-details">
          <dt>名称</dt>
          <dd>{profile?.displayName ?? "—"}</dd>
          <dt>角色</dt>
          <dd>{profile?.role ?? "—"}</dd>
          <dt>模型候选</dt>
          <dd data-testid="seat-profile-models">
            {models.length
              ? models.map((model, index) => (
                  <span key={model} className={css.model}>
                    {model}
                    {index === 0 ? "（默认）" : ""}
                  </span>
                ))
              : "—"}
          </dd>
          <dt>推理强度</dt>
          <dd>{(profile?.effort as { name?: string } | null | undefined)?.name ?? "harness 默认"}</dd>
          <dt>权限模式</dt>
          <dd>{profile?.permissionMode ?? "沿用创建时配置"}</dd>
          <dt>技能</dt>
          <dd>{profile?.skills.length ? profile.skills.join("、") : "—"}</dd>
          <dt>当前章节</dt>
          <dd>
            第 {view.liveChapter.generation} 章 · 已应用修订 {view.liveChapter.appliedRevision ?? "无"}
          </dd>
        </dl>
      ) : null}

      {unavailable.length > 0 ? (
        <ul className={css.unavailable} data-testid="seat-profile-unavailable">
          {unavailable.map((item) => (
            <li key={`${item.class}:${item.name}`}>
              {CLASS_LABEL[item.class] ?? item.class} <code>{item.name}</code>：
              {UNAVAILABLE_LABEL[item.reason] ?? item.reason}
            </li>
          ))}
          {refusal ? <li className={css.refused}>启动被拒绝：{refusal.next?.hint ?? refusal.reason}</li> : null}
        </ul>
      ) : null}

      {draft ? (
        <form
          className={css.form}
          data-testid="seat-profile-form"
          onSubmit={(event) => {
            event.preventDefault();
            void save();
          }}
        >
          <label className={css.field}>
            <span>名称</span>
            <input
              className={css.input}
              value={draft.displayName}
              maxLength={64}
              onChange={(event) => setDraft({ ...draft, displayName: event.target.value })}
            />
          </label>
          <label className={css.field}>
            <span>角色（一行）</span>
            <input
              className={css.input}
              value={draft.role}
              maxLength={160}
              onChange={(event) => setDraft({ ...draft, role: event.target.value })}
            />
          </label>
          <label className={css.field}>
            <span>模型候选（每行一个，第一个是默认，最多 5 个）</span>
            <textarea
              className={css.textarea}
              data-testid="seat-profile-models-input"
              value={draft.models}
              onChange={(event) => setDraft({ ...draft, models: event.target.value })}
            />
          </label>
          <label className={css.field}>
            <span>推理强度</span>
            <select
              className={css.input}
              data-testid="seat-profile-effort"
              value={draft.effort}
              onChange={(event) => setDraft({ ...draft, effort: event.target.value })}
            >
              <option value="">harness 默认</option>
              {EFFORT_LEVELS.map((level) => (
                <option key={level} value={level}>
                  {level}
                </option>
              ))}
            </select>
          </label>
          <label className={css.field}>
            <span>权限模式</span>
            <select
              className={css.input}
              data-testid="seat-profile-mode"
              value={draft.permissionMode}
              onChange={(event) => setDraft({ ...draft, permissionMode: event.target.value })}
            >
              {modeOptions(kind).map((option) => (
                <option key={option.id} value={option.id}>
                  {option.label}（{option.id}）
                </option>
              ))}
            </select>
          </label>
          <label className={css.field}>
            <span>技能目录（每行一个 host 绝对路径）</span>
            <textarea
              className={css.textarea}
              value={draft.skills}
              onChange={(event) => setDraft({ ...draft, skills: event.target.value })}
            />
          </label>
          <p className={css.note}>保存后在下一章节（重启或恢复）生效，不影响正在运行的章节。</p>
          {conflict ? (
            <p className={css.alert} role="alert" data-testid="seat-profile-conflict">
              配置已被其他设备修改{conflict.revision !== null ? `（当前修订 ${conflict.revision}）` : ""}
              ，请重新载入后再保存。
              <button type="button" className={css.btn} data-testid="seat-profile-reload" onClick={() => void reload()}>
                重新载入
              </button>
            </p>
          ) : null}
          {saveError ? (
            <p className={css.alert} role="alert" data-testid="seat-profile-error">
              {saveError}
            </p>
          ) : null}
          <div className={css.actions}>
            <button type="button" className={css.btn} onClick={() => setDraft(null)} disabled={saving}>
              取消
            </button>
            <button
              type="submit"
              className={`${css.btn} ${css.btnPrimary}`}
              data-testid="seat-profile-save"
              disabled={saving || conflict !== null}
            >
              {saving ? "保存中…" : "保存"}
            </button>
          </div>
        </form>
      ) : null}
    </div>
  );
}
