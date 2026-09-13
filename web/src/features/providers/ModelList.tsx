import { useId, useState } from "react";
import { Button } from "../../components/Button";
import ui from "../../styles/ui.module.css";
import css from "./providers.module.css";
import { contextChip, type ProviderModel } from "./model";

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
};

/**
 * Structured model catalog: probe a gateway, tick what to expose, and mark one
 * default. Manual ids cover gateways whose `/v1/models` lists nothing.
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
}: Props) {
  const [manual, setManual] = useState("");
  const groupName = useId();
  const enabled = models.filter((m) => m.enabled);

  const toggle = (id: string, on: boolean) => {
    onChange(models.map((m) => (m.id === id ? { ...m, enabled: on } : m)));
    // A model that is no longer offered cannot stay the default.
    if (!on && defaultModel === id) {
      onDefaultChange(enabled.find((m) => m.id !== id)?.id ?? "");
    }
  };

  const addManual = () => {
    const id = manual.trim();
    if (!id) return;
    setManual("");
    if (models.some((m) => m.id === id)) return;
    onChange([...models, { id, enabled: true }]);
    if (!defaultModel) onDefaultChange(id);
  };

  const remove = (id: string) => {
    onChange(models.filter((m) => m.id !== id));
    if (defaultModel === id) {
      onDefaultChange(models.find((m) => m.enabled && m.id !== id)?.id ?? "");
    }
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
      {models.length ? (
        <ul className={css.modelList}>
          {models.map((model) => {
            const isNew = discovered.includes(model.id);
            const context = contextChip(model.contextWindow);
            return (
              <li
                key={model.id}
                className={css.modelRow}
                data-testid="provider-model-row"
                data-model={model.id}
                data-enabled={model.enabled ? "1" : "0"}
                data-new={isNew ? "1" : "0"}
              >
                <label className={css.modelPick}>
                  <input
                    type="checkbox"
                    data-testid="provider-model-enabled"
                    checked={model.enabled}
                    onChange={(e) => toggle(model.id, e.target.checked)}
                  />
                  <span className={css.modelId}>{model.id}</span>
                </label>
                {model.label ? <span className={css.modelLabel}>{model.label}</span> : null}
                {context ? <span className={css.chipSmall}>{context}</span> : null}
                {model.tags?.map((tag) => (
                  <span key={tag} className={css.chipSmall} data-testid="provider-model-tag">
                    {tag}
                  </span>
                ))}
                {isNew ? (
                  <span className={css.badge} data-testid="provider-model-new">
                    新增
                  </span>
                ) : null}
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
              </li>
            );
          })}
        </ul>
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
