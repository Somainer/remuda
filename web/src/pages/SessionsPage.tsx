import { useHub } from "../lib/store";

export function SessionsPage() {
  const hub = useHub();
  return (
    <div style={{ padding: 24, color: "var(--mute)" }} data-testid="sessions-empty">
      {hub.hosts.length === 0 ? "无主机。请添加主机。" : hub.instances.length === 0 ? "还没有会话。" : "选择一个会话"}
    </div>
  );
}
