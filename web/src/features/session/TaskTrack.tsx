import type { ToolNode } from "./assemble";
import { knowledgeValue } from "../../types/command";
import { asRecord, asString } from "../../lib/format";
import css from "./session.module.css";

export function TaskTrack({ tasks }: { tasks: ToolNode[] }) {
  if (!tasks.length) return null;
  return (
    <aside data-testid="task-track" className={css.track}>
      <div className={css.toolHead} style={{ padding: 0, borderBottom: 0 }}>
        <span className={css.toolTitle}>Task</span>
        <span className={css.stat}>{tasks.length}</span>
      </div>
      <ul>
        {tasks.map((task) => {
          const rec = asRecord(knowledgeValue(task.call.input));
          const prompt = asString(rec?.prompt) ?? task.name;
          const running = !task.result || task.result.stage !== "final";
          return (
            <li key={task.id}>
              {prompt} · {running ? "running" : (task.result?.outcome ?? "unknown")}
            </li>
          );
        })}
      </ul>
    </aside>
  );
}
