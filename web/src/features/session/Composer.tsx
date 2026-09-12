import { useEffect, useLayoutEffect, useRef, useState, type KeyboardEvent } from "react";
import { readDraft, writeDraft } from "../../lib/drafts";
import { PERMISSION_OPTIONS } from "../../lib/sessionOptions";
import { composing } from "../../lib/viewport";
import type { HostCli } from "../hosts";
import {
  EFFORT_MENU_FOOTER,
  EFFORT_MENU_HEADER,
  effortCaps,
  effortTable,
  HARNESS_META,
  harnessMeta,
  isEmberTier,
  mapEffort,
  modelsFor,
  shortModel,
  type EffortKind,
  type EffortSelection,
} from "./effort";
import css from "./session.module.css";

type MenuId = "harness" | "effort" | "permission" | null;
type Placement = "up" | "down";

export function Composer({
  instanceId,
  mobile,
  disabled,
  sending,
  onSend,
  permissionMode = "manual",
  onPermission,
  kind = "claude",
  model = "opus",
  models,
  effort,
  onEffort,
  onModel,
  onHarness,
  contextLabel,
  hostLabel,
  hostCli = [],
}: {
  instanceId: string;
  mobile: boolean;
  disabled?: boolean;
  sending?: boolean;
  onSend: (text: string) => Promise<void> | void;
  permissionMode?: string;
  onPermission?: (mode: string) => void;
  kind?: EffortKind | string;
  model?: string;
  models?: string[];
  effort?: EffortSelection;
  onEffort?: (next: EffortSelection) => void;
  onModel?: (model: string) => void;
  onHarness?: (kind: EffortKind) => void;
  contextLabel?: string | null;
  hostLabel?: string;
  hostCli?: HostCli[];
}) {
  const [text, setText] = useState(() => readDraft(instanceId));
  const [harnessOverride, setHarnessOverride] = useState<string | null>(null);
  const [menu, setMenu] = useState<MenuId>(null);
  const [placement, setPlacement] = useState<Placement>("down");
  const rootRef = useRef<HTMLFormElement>(null);
  const barRef = useRef<HTMLDivElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const harness = harnessOverride ?? kind;

  const caps = effortCaps(harness);
  const incoming = effort ?? { index: 1, name: "think", kind: (kind as EffortKind) || "claude" };
  const currentEffort =
    incoming.kind === harness ? incoming : mapEffort(incoming, (harness as EffortKind) || "claude");
  const table = effortTable(harness);
  const ember = isEmberTier(harness, currentEffort.index);
  const permLabel = PERMISSION_OPTIONS.find((m) => m.id === permissionMode)?.label ?? permissionMode;
  const modelList = modelsFor(harness, models ?? [model]);
  const installed = new Set(hostCli.filter((c) => c.path || c.version).map((c) => c.kind));
  if (!installed.size) installed.add("claude");
  installed.add(String(kind));
  installed.add(String(harness));

  const submit = async () => {
    const value = text.trim();
    if (!value || disabled || sending) return;
    writeDraft(instanceId, "");
    setText("");
    await onSend(value);
  };

  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (composing(event)) return;
    if (mobile) return;
    if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
      event.preventDefault();
      void submit();
    }
  };

  useEffect(() => {
    if (!menu) return;
    const onDown = (event: PointerEvent) => {
      if (!rootRef.current?.contains(event.target as Node)) setMenu(null);
    };
    const onKey = (event: globalThis.KeyboardEvent) => {
      if (event.key === "Escape") setMenu(null);
    };
    window.addEventListener("pointerdown", onDown);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("pointerdown", onDown);
      window.removeEventListener("keydown", onKey);
    };
  }, [menu]);

  useLayoutEffect(() => {
    if (!menu) return;
    const bar = barRef.current;
    const panel = menuRef.current;
    if (!bar || !panel) return;
    const barRect = bar.getBoundingClientRect();
    const approval = document.querySelector("[data-testid='approval-card'], [data-testid='question-form']");
    const approvalBottom = approval?.getBoundingClientRect().bottom ?? 0;
    const roomAbove = barRect.top - Math.max(approvalBottom, 0);
    const next = roomAbove >= panel.offsetHeight + 8 ? "up" : "down";
    if (next !== placement) setPlacement(next);
  }, [menu, harness, currentEffort.index, placement]);

  const toggle = (id: MenuId) => {
    setPlacement("down");
    setMenu((cur) => (cur === id ? null : id));
  };

  const pickHarness = (next: EffortKind, enabled: boolean) => {
    if (!enabled) return;
    setHarnessOverride(next);
    const mapped = mapEffort({ ...currentEffort, kind: (currentEffort.kind || harness) as EffortKind }, next);
    onHarness?.(next);
    onEffort?.(mapped);
    setMenu(null);
  };

  const pickEffort = (index: number) => {
    const next = { index, name: table[index]?.name ?? "default", kind: (harness as EffortKind) || "claude" };
    onEffort?.(next);
    setMenu(null);
  };

  const harnessChip = harnessMeta(harness);
  const effortChipLabel = caps.model
    ? `${shortModel(model)} ${currentEffort.name}`
    : currentEffort.name;

  return (
    <form
      ref={rootRef}
      data-testid="composer"
      data-harness={harness}
      data-effort={currentEffort.name}
      data-effort-index={String(currentEffort.index)}
      data-model={model}
      className={css.composerRoot}
      onSubmit={(e) => {
        e.preventDefault();
        void submit();
      }}
    >
      <div className={css.composer}>
        <textarea
          className={css.input}
          data-testid="composer-input"
          value={text}
          disabled={disabled}
          placeholder={mobile ? "输入提示词…" : "输入提示词…  Enter 送出 · Shift+Enter 换行 · IME 组字期间不送"}
          onChange={(e) => {
            setText(e.target.value);
            writeDraft(instanceId, e.target.value);
          }}
          onKeyDown={onKeyDown}
        />
        {mobile ? (
          <button
            type="button"
            className={css.sendIcon}
            data-testid="composer-send"
            aria-label="送出"
            disabled={disabled || sending || !text.trim()}
            onClick={() => void submit()}
          >
            ↑
          </button>
        ) : null}
      </div>
      <div className={css.controlBar} ref={barRef} data-testid="composer-bar">
        {caps.harness ? (
          <button
            type="button"
            className={css.chip}
            data-testid="harness-chip"
            aria-expanded={menu === "harness"}
            onClick={() => toggle("harness")}
          >
            <span className={css.chipMark}>{harnessChip.mark}</span>
            <span>{mobile ? harnessChip.label.replace(" Code", "") : harnessChip.label}</span>
            <span className={css.chipCaret}>▾</span>
          </button>
        ) : null}
        {caps.effort ? (
          <button
            type="button"
            className={`${css.chip} ${ember ? css.ember : ""}`}
            data-testid="model-effort-chip"
            data-ember={ember ? "1" : "0"}
            aria-expanded={menu === "effort"}
            onClick={() => toggle("effort")}
          >
            {ember ? (
              <>
                <span className={css.emberSpark} />
                <span className={`${css.emberSpark} ${css.emberSpark2}`} />
                <span className={`${css.emberSpark} ${css.emberSpark3}`} />
              </>
            ) : null}
            <span className={css.chipModel}>{effortChipLabel}</span>
            <span className={css.chipCaret}>▾</span>
          </button>
        ) : null}
        {caps.context ? (
          <span className={css.chip} data-testid="context-chip">
            <span
              className={css.contextRing}
              style={{ ["--ctx-pct" as string]: contextLabel?.endsWith("%") ? contextLabel : "0%" }}
            />
            <span>{contextLabel ?? "—"}</span>
          </span>
        ) : null}
        {caps.permission && onPermission ? (
          <button
            type="button"
            className={css.chip}
            data-testid="permission-chip"
            aria-expanded={menu === "permission"}
            onClick={() => toggle("permission")}
          >
            {mobile ? permLabel : `权限 ${permLabel}`} ▾
          </button>
        ) : null}
        <span className={css.barSpacer} />
        {mobile ? null : (
          <button type="button" className={css.send} data-testid="composer-send" disabled={disabled || sending || !text.trim()} onClick={() => void submit()}>
            {sending ? "发送中" : "送出"}
          </button>
        )}
      </div>
      {menu === "harness" ? (
        <div
          ref={menuRef}
          className={`${css.popover} ${placement === "up" ? css.popoverUp : ""}`}
          data-testid="harness-menu"
          data-placement={placement}
        >
          {HARNESS_META.map((item) => {
            const cli = hostCli.find((c) => c.kind === item.id);
            const enabled = item.id === "terminal" || item.id === "claude" || installed.has(item.id);
            const on = harness === item.id;
            return (
              <button
                key={item.id}
                type="button"
                className={`${css.harnessRow} ${on ? css.harnessOn : ""} ${enabled ? "" : css.harnessOff}`}
                data-testid={`harness-option-${item.id}`}
                data-installed={enabled ? "1" : "0"}
                disabled={!enabled}
                onClick={() => pickHarness(item.id, enabled)}
              >
                <span className={css.chipMark}>{item.mark}</span>
                <span className={css.harnessName}>{item.label}</span>
                {enabled ? (
                  <span className={css.harnessMeta}>
                    {cli?.auth === "logged_in" ? `${hostLabel ?? "host"} · logged_in` : cli?.version ?? ""}
                    {on ? " ✓" : ""}
                  </span>
                ) : (
                  <span className={css.harnessPlus} aria-label="未安装">
                    +
                  </span>
                )}
              </button>
            );
          })}
        </div>
      ) : null}
      {menu === "effort" ? (
        <div
          ref={menuRef}
          className={`${css.popover} ${placement === "up" ? css.popoverUp : ""}`}
          data-testid="effort-menu"
          data-placement={placement}
        >
          {caps.model ? (
            <div className={css.menuSection} data-testid="model-menu">
              {modelList.map((id) => (
                <button
                  key={id}
                  type="button"
                  className={`${css.effortRow} ${model === id || shortModel(model) === shortModel(id) ? css.effortOn : ""}`}
                  data-testid={`model-option-${shortModel(id)}`}
                  onClick={() => {
                    onModel?.(id);
                    setMenu(null);
                  }}
                >
                  <span className={css.radio} />
                  <span className={css.effortName}>{shortModel(id)}</span>
                </button>
              ))}
            </div>
          ) : null}
          <div className={css.menuHead}>{EFFORT_MENU_HEADER}</div>
          {table.map((tier, index) => {
            const on = currentEffort.index === index;
            const top = isEmberTier(harness, index);
            return (
              <button
                key={tier.name}
                type="button"
                className={`${css.effortRow} ${on ? css.effortOn : ""} ${top ? css.ember : ""}`}
                data-testid={`effort-tier-${tier.name}`}
                data-ember={top ? "1" : "0"}
                onClick={() => pickEffort(index)}
              >
                <span className={`${css.radio} ${on ? css.radioOn : ""}`} />
                <span className={css.effortName}>{tier.name}</span>
                <span className={css.effortDesc}>{tier.description}</span>
              </button>
            );
          })}
          <div className={css.menuFoot}>{EFFORT_MENU_FOOTER}</div>
        </div>
      ) : null}
      {menu === "permission" ? (
        <div
          ref={menuRef}
          className={`${css.popover} ${placement === "up" ? css.popoverUp : ""}`}
          data-testid="permission-menu"
          data-placement={placement}
        >
          {PERMISSION_OPTIONS.map((m) => (
            <button
              key={m.id}
              type="button"
              className={`${css.effortRow} ${permissionMode === m.id ? css.effortOn : ""}`}
              data-testid={`permission-option-${m.id}`}
              onClick={() => {
                onPermission?.(m.id);
                setMenu(null);
              }}
            >
              <span className={`${css.radio} ${permissionMode === m.id ? css.radioOn : ""}`} />
              <span className={css.effortName}>{m.label}</span>
              <span className={css.effortDesc}>{m.id}</span>
            </button>
          ))}
        </div>
      ) : null}
    </form>
  );
}
