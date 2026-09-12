import { SessionList } from "../features/session/SessionList";
import { useHub } from "../lib/store";
import css from "./SessionsPage.module.css";

export function SessionsPage({ dimmed = false }: { dimmed?: boolean }) {
  const hub = useHub();
  return (
    <div className={`${css.page} ${dimmed ? css.dimmed : ""}`} data-testid="sessions-empty">
      {hub.hosts.length === 0 && hub.instances.length === 0 ? (
        <div className={css.placeholder}>无主机。请添加主机。</div>
      ) : null}
      <SessionList variant="full" />
    </div>
  );
}
