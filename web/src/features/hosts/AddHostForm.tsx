import { useState } from "react";
import { Button } from "../../components/Button";
import { Modal } from "../../components/Modal";
import { api } from "../../lib/api";
import { hubStore } from "../../lib/store";
import ui from "../../styles/ui.module.css";
import css from "./hosts.module.css";

export function AddHostForm({ open, onClose }: { open: boolean; onClose: () => void }) {
  const [target, setTarget] = useState("");
  const [label, setLabel] = useState("");
  const [labels, setLabels] = useState("");
  const [policy, setPolicy] = useState<"require_installed" | "upload_if_missing">("require_installed");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  return (
    <Modal open={open} onClose={() => { if (!busy) onClose(); }}>
      <form className={css.fields} onSubmit={(event) => {
        event.preventDefault();
        setBusy(true);
        setError(null);
        void api.hostSshAdd({
          target: target.trim(), label: label.trim() || target.trim(),
          labels: labels.split(",").map((value) => value.trim()).filter(Boolean),
          remuda_binary_policy: policy,
        }).then(async () => {
          await hubStore.refreshHosts();
          setTarget(""); setLabel(""); setLabels("");
          onClose();
        }).catch((reason: unknown) => setError(reason instanceof Error ? reason.message : "添加失败"))
          .finally(() => setBusy(false));
      }}>
        <h2 style={{ margin: 0, fontSize: 18 }}>添加主机</h2>
        <p className={css.meta}>填写 Hub 所在机器能够 SSH 登录的地址或 SSH 配置别名。连接中断后自动重连。</p>
        <label className={ui.field}>SSH 目标
          <input autoFocus required autoCapitalize="none" autoCorrect="off" spellCheck={false} className={ui.input} data-testid="add-host-target" placeholder="dev@host 或 SSH 别名" maxLength={255} value={target} onChange={(e) => setTarget(e.target.value)} />
        </label>
        <label className={ui.field}>显示名
          <input className={ui.input} data-testid="add-host-label" placeholder="默认使用 SSH 目标" maxLength={128} value={label} onChange={(e) => setLabel(e.target.value)} />
        </label>
        <label className={ui.field}>标签（逗号分隔）
          <input className={ui.input} data-testid="add-host-labels" placeholder="egress:gateway, region:sg" value={labels} onChange={(e) => setLabels(e.target.value)} />
        </label>
        <label className={ui.field}>Remuda 安装策略
          <select className={`${ui.select} ${ui.touchSelect}`} data-testid="add-host-policy" value={policy} onChange={(e) => setPolicy(e.target.value as typeof policy)}>
            <option value="require_installed">使用已安装版本</option>
            <option value="upload_if_missing">缺少时上传临时副本</option>
          </select>
        </label>
        <p className={css.meta}>上传和运行数据保存在远端专属临时目录。版本不兼容时显示错误。</p>
        {error ? <p role="alert" className={css.sshError}>{error}</p> : null}
        <div className={ui.row} style={{ justifyContent: "flex-end", marginTop: 12 }}>
          <Button type="button" disabled={busy} onClick={onClose}>取消</Button>
          <Button type="submit" variant="primary" disabled={busy || !target.trim()} data-testid="add-host-submit">{busy ? "正在添加…" : "添加主机"}</Button>
        </div>
      </form>
    </Modal>
  );
}
