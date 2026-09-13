import { useCallback, useEffect, useId, useMemo, useRef, useState } from "react";
import { Button } from "../../components/Button";
import ui from "../../styles/ui.module.css";
import css from "./providers.module.css";
import { readCollapsedGroups, writeCollapsedGroups } from "./groupPrefs";
import {
  contextChip,
  filterModels,
  groupModels,
  invertEnabled,
  nextDefaultModel,
  setEnabled,
  splitModelId,
  triState,
  type ProviderModel,
} from "./model";

type Props = {
  models: ProviderModel[];
  defaultModel: string;
  /** Ids the last discovery reported that were not already in the list. */
  discovered?: string[];
  discovering?: boolean;
  discoverError?: string | null;
  /** Absent when there is nothing to probe (no base URL yet). */
  onDiscover?: () => void;
  onChange: (models: ProviderModel[]) => void;
  onDefaultChange: (id: string) => void;
  /** Scopes the persisted collapse state. `new` until the profile is saved. */
  profileId?: string;
};

/** A checkbox that can also render the indeterminate ("some") state. */
function TriBox({
  state,
  ...rest
}: { state: "all" | "some" | "none" } & React.InputHTMLAttributes<HTMLInputElement>) {
  const ref = useRef<HTMLInputElement>(null);
  useEffect(() => {
    // `indeterminate` is a DOM property with no HTML attribute, so React
    // cannot set it from JSX.
    if (ref.current) ref.current.indeterminate = state === "some";
  }, [state]);
  return <input ref={ref} type="checkbox" checked={state === "all"} {...rest} />;
}

/**
 * Structured model catalog: probe a gateway, tick what to expose, and mark one
 * default. Catalogs run to a few hundred ids, so the list is searchable,
 * grouped by prefix, and editable in bulk — per group or across the whole
 * filtered view — rather than one checkbox at a time. Manual ids cover
 * gateways whose `/v1/models` lists nothing.
 */
export function ModelList({
  models,
  defaultModel,
  discovered = [],
  discovering,
  discoverError,
  onDiscover,
  onChange,
  onDefaultChange,
  profileId = "new",
}: Props) {
  const [manual, setManual] = useState("");
  const [query, setQuery] = useState("");
  // Collapsed groups by key, restored per profile: an operator who folded a
  // 200-id catalog down to the two groups they use keeps that shape.
  const [collapsed, setCollapsed] = useState<Set<string>>(() => readCollapsedGroups(profileId));
  // Which profile `collapsed` was read for, so switching profiles re-reads it
  // during render rather than after a throwaway pass with the wrong shape.
  const [loadedFor, setLoadedFor] = useState(profileId);
  // One step of history so a mis-aimed bulk action is a click away from undone.
  const [undo, setUndo] = useState<{ text: string; models: ProviderModel[]; defaultModel: string } | null>(null);
  const groupName = useId();
  const enabled = models.filter((m) => m.enabled);
  const visible = useMemo(() => filterModels(models, query), [models, query]);
  const groups = useMemo(() => groupModels(visible), [visible]);
  const filtering = query.trim().length > 0;

  if (loadedFor !== profileId) {
    setLoadedFor(profileId);
    setCollapsed(readCollapsedGroups(profileId));
  }

  /**
   * Apply an edit and settle the default in one commit. Splitting the two
   * lets the parent re-render between them, which is how a disabled model
   * used to survive as the default.
   */
  const commit = useCallback(
    (next: ProviderModel[]) => {
      onChange(next);
      const settled = nextDefaultModel(next, defaultModel);
      if (settled !== defaultModel) onDefaultChange(settled);
      return settled;
    },
    [defaultModel, onChange, onDefaultChange],
  );

  /** A bulk edit: same commit, plus one undo entry and a notice. */
  const bulk = (next: ProviderModel[], text: string) => {
    const before = { models, defaultModel };
    const settled = commit(next);
    setUndo({
      ...before,
      text:
        settled !== defaultModel && settled
          ? `${text}；默认模型改为 ${settled}`
          : text,
    });
  };

  const toggle = (id: string, on: boolean) => {
    commit(models.map((m) => (m.id === id ? { ...m, enabled: on } : m)));
  };

  const setGroup = (key: string, ids: string[], on: boolean) => {
    bulk(setEnabled(models, ids, on), `${on ? "已启用" : "已停用"} ${key} 的 ${ids.length} 个模型`);
  };

  const visibleIds = visible.map((m) => m.id);
  const scope = filtering ? `筛选结果 ${visible.length}` : `全部 ${models.length}`;

  const addManual = () => {
    const id = manual.trim();
    if (!id) return;
    setManual("");
    if (models.some((m) => m.id === id)) return;
    onChange([...models, { id, enabled: true }]);
    if (!defaultModel) onDefaultChange(id);
  };

  const remove = (id: string) => {
    commit(models.filter((m) => m.id !== id));
  };

  const toggleGroupOpen = (key: string) => {
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      writeCollapsedGroups(profileId, next);
      return next;
    });
  };

  return (
    <fieldset className={css.modelField} data-testid="provider-models">
      <legend className={ui.label}>模型列表</legend>
      <div className={css.modelHead}>
        {onDiscover ? (
          <Button
            type="button"
            data-testid="provider-discover"
            disabled={discovering}
            onClick={onDiscover}
          >
            {discovering ? "探测中…" : "探测模型"}
          </Button>
        ) : null}
        <span className={css.modelCount} data-testid="provider-models-count">
          {models.length ? `${enabled.length}/${models.length} 已启用` : "尚未探测"}
        </span>
        {models.length ? (
          <div className={css.modelBulk} role="group" aria-label="批量启用">
            <button
              type="button"
              className={css.modelBulkBtn}
              data-testid="provider-models-all"
              disabled={!visible.length || visible.every((m) => m.enabled)}
              onClick={() => bulk(setEnabled(models, visibleIds, true), `已启用 ${visible.length} 个模型`)}
            >
              全选（{scope}）
            </button>
            <button
              type="button"
              className={css.modelBulkBtn}
              data-testid="provider-models-none"
              disabled={!visible.length || visible.every((m) => !m.enabled)}
              onClick={() => bulk(setEnabled(models, visibleIds, false), `已停用 ${visible.length} 个模型`)}
            >
              全不选
            </button>
            <button
              type="button"
              className={css.modelBulkBtn}
              data-testid="provider-models-invert"
              disabled={!visible.length}
              onClick={() => bulk(invertEnabled(models, visibleIds), `已反选 ${visible.length} 个模型`)}
            >
              反选
            </button>
          </div>
        ) : null}
      </div>
      {discovering ? (
        <p className={css.modelHint} role="status" data-testid="provider-discover-busy">
          正在读取 /v1/models…
        </p>
      ) : null}
      {discoverError ? (
        <p className={css.error} role="alert" data-testid="provider-discover-error">
          {discoverError}
        </p>
      ) : null}
      {undo ? (
        <p className={css.modelUndo} role="status" data-testid="provider-models-undo">
          <span>{undo.text}</span>
          <button
            type="button"
            className={css.modelBulkBtn}
            data-testid="provider-models-undo-button"
            onClick={() => {
              onChange(undo.models);
              onDefaultChange(undo.defaultModel);
              setUndo(null);
            }}
          >
            撤销
          </button>
          <button
            type="button"
            className={css.modelRemove}
            data-testid="provider-models-undo-dismiss"
            aria-label="关闭提示"
            onClick={() => setUndo(null)}
          >
            ×
          </button>
        </p>
      ) : null}
      {models.length ? (
        <>
          <input
            className={ui.input}
            type="search"
            data-testid="provider-model-search"
            value={query}
            placeholder="筛选模型 id 或名称"
            aria-label="筛选模型"
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={(e) => {
              // The field lives inside the profile form; Enter must filter,
              // not submit.
              if (e.key === "Enter") e.preventDefault();
            }}
          />
          {visible.length ? (
            <ul className={css.modelList}>
              {groups.map((group) => {
                // A group of one is its own id: a header saying `1/1 solo` over
                // a single `solo` row is pure noise, so it renders as a row.
                const alone = group.models.length === 1 && group.key === group.models[0].id;
                const shut = !alone && collapsed.has(group.key);
                const on = group.models.filter((m) => m.enabled).length;
                const state = triState(group.models);
                const ids = group.models.map((m) => m.id);
                return (
                  <li
                    key={group.key}
                    className={alone ? css.modelGroupSolo : css.modelGroup}
                    data-testid="provider-model-group"
                    data-group={group.key}
                    data-collapsed={shut ? "1" : "0"}
                    data-solo={alone ? "1" : "0"}
                    data-state={state}
                  >
                    {alone ? null : (
                      <div className={css.modelGroupHead}>
                        <label className={css.modelGroupPick} title={`启用/停用 ${group.key}`}>
                          <TriBox
                            state={state}
                            data-testid="provider-model-group-enabled"
                            aria-label={`启用 ${group.key} 全部 ${group.models.length} 个模型`}
                            onChange={(e) => setGroup(group.key, ids, e.target.checked)}
                          />
                        </label>
                        <button
                          type="button"
                          className={css.modelGroupToggle}
                          data-testid="provider-model-group-toggle"
                          aria-expanded={!shut}
                          onClick={() => toggleGroupOpen(group.key)}
                        >
                          <span aria-hidden>{shut ? "▸" : "▾"}</span>
                          <span className={css.modelGroupKey}>{group.key}</span>
                          <span className={css.modelGroupCount} data-testid="provider-model-group-count">
                            {on}/{group.models.length} 已启用
                          </span>
                        </button>
                      </div>
                    )}
                    {shut ? null : (
                      <ul className={css.modelGroupList}>
                        {group.models.map((model) => {
                          const isNew = discovered.includes(model.id);
                          const context = contextChip(model.contextWindow);
                          // The Hub tags a 1M window "1m", which the context
                          // chip already shows; render each fact once.
                          const tags = model.tags?.filter((tag) => tag !== context) ?? [];
                          const [head, tail] = splitModelId(model.id);
                          return (
                            <li
                              key={model.id}
                              className={css.modelRow}
                              data-testid="provider-model-row"
                              data-model={model.id}
                              data-enabled={model.enabled ? "1" : "0"}
                              data-new={isNew ? "1" : "0"}
                            >
                              <label className={css.modelPick} title={model.id}>
                                <input
                                  type="checkbox"
                                  data-testid="provider-model-enabled"
                                  checked={model.enabled}
                                  aria-label={model.id}
                                  onChange={(e) => toggle(model.id, e.target.checked)}
                                />
                                {/* Two spans so the head ellipsizes and the
                                    tail — where ids actually differ — stays. */}
                                <span className={css.modelId}>
                                  <span className={css.modelIdHead}>{head}</span>
                                  {tail ? <span className={css.modelIdTail}>{tail}</span> : null}
                                </span>
                              </label>
                              <span className={css.modelMeta}>
                                {model.label ? (
                                  <span className={css.modelLabel}>{model.label}</span>
                                ) : null}
                                {context ? (
                                  <span className={css.chipSmall} data-testid="provider-model-context">
                                    {context}
                                  </span>
                                ) : null}
                                {tags.map((tag) => (
                                  <span
                                    key={tag}
                                    className={css.chipSmall}
                                    data-testid="provider-model-tag"
                                  >
                                    {tag}
                                  </span>
                                ))}
                                {/* Which gateway listing reported this id. */}
                                {(model.surfaces ?? []).map((surface) => (
                                  <span
                                    key={surface}
                                    className={css.chipSurface}
                                    data-testid="provider-model-surface"
                                    data-surface={surface}
                                  >
                                    {surface}
                                  </span>
                                ))}
                                {isNew ? (
                                  <span className={css.badge} data-testid="provider-model-new">
                                    新增
                                  </span>
                                ) : null}
                              </span>
                              <span className={css.modelTail}>
                                <label className={css.modelDefault} title="设为默认模型">
                                  <input
                                    type="radio"
                                    name={groupName}
                                    data-testid="provider-model-default"
                                    disabled={!model.enabled}
                                    checked={defaultModel === model.id}
                                    onChange={() => onDefaultChange(model.id)}
                                  />
                                  默认
                                </label>
                                <button
                                  type="button"
                                  className={css.modelRemove}
                                  data-testid="provider-model-remove"
                                  aria-label={`移除 ${model.id}`}
                                  onClick={() => remove(model.id)}
                                >
                                  ×
                                </button>
                              </span>
                            </li>
                          );
                        })}
                      </ul>
                    )}
                  </li>
                );
              })}
            </ul>
          ) : (
            <p className={css.modelHint} data-testid="provider-models-no-match">
              没有匹配 “{query.trim()}” 的模型。
            </p>
          )}
        </>
      ) : (
        <p className={css.modelHint} data-testid="provider-models-empty">
          探测网关，或手动填入模型 id。
        </p>
      )}
      <div className={css.modelAdd}>
        <input
          className={ui.input}
          data-testid="provider-model-manual"
          value={manual}
          placeholder="手动填入模型 id"
          aria-label="手动填入模型 id"
          onChange={(e) => setManual(e.target.value)}
          onKeyDown={(e) => {
            // Enter adds a chip; it must not submit the surrounding form.
            if (e.key === "Enter" || e.key === ",") {
              e.preventDefault();
              addManual();
            }
          }}
        />
        <Button
          type="button"
          data-testid="provider-model-add"
          disabled={!manual.trim()}
          onClick={addManual}
        >
          添加
        </Button>
      </div>
    </fieldset>
  );
}
