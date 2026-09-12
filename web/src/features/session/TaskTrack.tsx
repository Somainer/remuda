import type { ToolNode } from "./assemble";
import { knowledgeValue } from "../../types/command";
import { asRecord, asString } from "../../lib/format";
import ui from "../../styles/ui.module.css";

export function TaskTrack({ tasks }: { tasks: ToolNode[] }) {
  if (!tasks.length) return null;
  return (
    <aside data-testid="task-track" className={ui.card} style={{ marginBottom: 8 }}>
      <div className={ui.cardHead}>
        <strong>Task</strong>
        <span>{tasks.length}</span>
      </div>
      <ul style={{ margin: 0, paddingLeft: 18 }}>
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
