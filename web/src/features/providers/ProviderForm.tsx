import { useState } from "react";
import { Button } from "../../components/Button";
import ui from "../../styles/ui.module.css";
import css from "./providers.module.css";
import { parseModels, type ProviderCreate, type ProviderProfile } from "./model";

type HostOption = { id: string; label: string };

type Props = {
  initial?: ProviderProfile;
  rotateOnly?: boolean;
  busy?: boolean;
  error?: string | null;
  hosts?: HostOption[];
  onSubmit: (body: ProviderCreate) => void;
  onCancel: () => void;
};

function scopeParts(scope: string | undefined): { kind: "universal" | "host"; hostId: string } {
  if (scope?.startsWith("host:")) return { kind: "host", hostId: scope.slice("host:".length) };
  return { kind: "universal", hostId: "" };
}

export function ProviderForm({ initial, rotateOnly, busy, error, hosts = [], onSubmit, onCancel }: Props) {
  const editing = Boolean(initial && initial.kind !== "native");
  const initialScope = scopeParts(initial?.scope);
  const [name, setName] = useState(initial?.name ?? "");
  const [kind, setKind] = useState<"gateway" | "direct">(initial?.kind === "direct" ? "direct" : "gateway");
  const [baseUrl, setBaseUrl] = useState(initial?.baseUrl ?? "");
  const [authToken, setAuthToken] = useState("");
  const [models, setModels] = useState((initial?.models ?? []).join("\n"));
  const [defaultModel, setDefaultModel] = useState(initial?.defaultModel ?? "");
  const [defaultGateway, setDefaultGateway] = useState(initial?.defaultGateway ?? true);
  const [scopeKind, setScopeKind] = useState<"universal" | "host">(initialScope.kind);
  const [scopeHostId, setScopeHostId] = useState(initialScope.hostId || hosts[0]?.id || "");
  const tokenRequired = !editing || rotateOnly;
  const canSave =
    name.trim() &&
    (kind === "direct" || baseUrl.trim()) &&
    (!tokenRequired || authToken.trim()) &&
    (scopeKind !== "host" || Boolean(scopeHostId));

  return (
    <form
      className={css.form}
      data-testid="provider-form"
      onSubmit={(e) => {
        e.preventDefault();
        if (!canSave || busy) return;
        const list = parseModels(models);
        onSubmit({
          name: name.trim(),
          kind,
          baseUrl: baseUrl.trim(),
          models: list,
          defaultModel: defaultModel.trim() || list[0],
          authToken: authToken.trim(),
          defaultGateway: kind === "gateway" && defaultGateway,
          scope: scopeKind === "host" && scopeHostId ? `host:${scopeHostId}` : "universal",
        });
      }}
    >
      {rotateOnly ? (
        <label className={ui.field}>
          新 auth token
          <input
            className={ui.input}
            type="password"
            autoComplete="off"
            data-testid="provider-token"
            value={authToken}
            onChange={(e) => setAuthToken(e.target.value)}
            placeholder="只提交一次，之后只显示 last4"
          />
        </label>
      ) : (
        <>
          <label className={ui.field}>
            名称
            <input className={ui.input} data-testid="provider-name" value={name} onChange={(e) => setName(e.target.value)} />
          </label>
          <fieldset className={ui.field} style={{ border: 0, padding: 0, margin: 0 }}>
            <legend>类型</legend>
            <div className={css.seg}>
              <button
                type="button"
                className={`${css.choice} ${kind === "gateway" ? css.choiceOn : ""}`}
                data-testid="provider-kind-gateway"
                onClick={() => setKind("gateway")}
              >
                网关
              </button>
              <button
                type="button"
                className={`${css.choice} ${kind === "direct" ? css.choiceOn : ""}`}
                data-testid="provider-kind-direct"
                onClick={() => setKind("direct")}
              >
                直连
              </button>
            </div>
          </fieldset>
          <label className={ui.field}>
            Base URL
            <input
              className={ui.input}
              data-testid="provider-base-url"
              placeholder="https://gateway.example/v1"
              value={baseUrl}
              onChange={(e) => setBaseUrl(e.target.value)}
            />
          </label>
          <label className={ui.field}>
            {editing ? "轮换 auth token（留空则不变）" : "Auth token"}
            <input
              className={ui.input}
              type="password"
              autoComplete="off"
              data-testid="provider-token"
              value={authToken}
              onChange={(e) => setAuthToken(e.target.value)}
              placeholder="只提交一次，GET 永不返回"
            />
          </label>
          <label className={ui.field}>
            模型列表（逗号或换行）
            <textarea className={ui.textarea} data-testid="provider-models" value={models} onChange={(e) => setModels(e.target.value)} />
          </label>
          <label className={ui.field}>
            默认模型
            <input className={ui.input} data-testid="provider-default-model" value={defaultModel} onChange={(e) => setDefaultModel(e.target.value)} />
          </label>
          <fieldset className={ui.field} style={{ border: 0, padding: 0, margin: 0 }}>
            <legend>范围</legend>
            <div className={css.seg}>
              <button
                type="button"
                className={`${css.choice} ${scopeKind === "universal" ? css.choiceOn : ""}`}
                data-testid="provider-scope-universal"
                onClick={() => setScopeKind("universal")}
              >
                全局
              </button>
              <button
                type="button"
                className={`${css.choice} ${scopeKind === "host" ? css.choiceOn : ""}`}
                data-testid="provider-scope-host"
                onClick={() => {
                  setScopeKind("host");
                  if (!scopeHostId && hosts[0]) setScopeHostId(hosts[0].id);
                }}
              >
                仅此主机
              </button>
            </div>
          </fieldset>
          {scopeKind === "host" ? (
            <label className={ui.field}>
              主机
              <select
                className={ui.input}
                data-testid="provider-scope-host-id"
                value={scopeHostId}
                onChange={(e) => setScopeHostId(e.target.value)}
              >
                {hosts.map((host) => (
                  <option key={host.id} value={host.id}>
                    {host.label}
                  </option>
                ))}
              </select>
            </label>
          ) : null}
          {kind === "gateway" ? (
            <label className={css.toggleRow}>
              <input
                type="checkbox"
                data-testid="provider-default-gateway"
                checked={defaultGateway}
                onChange={(e) => setDefaultGateway(e.target.checked)}
              />
              设为该范围的默认网关
            </label>
          ) : null}
        </>
      )}
      {error ? (
        <p className={css.error} data-testid="provider-form-error">
          {error}
        </p>
      ) : null}
      <div className={ui.row} style={{ marginTop: 8 }}>
        <Button type="button" onClick={onCancel}>
          取消
        </Button>
        <Button type="submit" variant="primary" disabled={!canSave || busy} data-testid="provider-save">
          {busy ? "保存中" : editing || rotateOnly ? "保存" : "创建"}
        </Button>
      </div>
    </form>
  );
}
