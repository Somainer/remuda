import { useLayoutEffect, useRef, useState } from "react";
import type { ToolNode } from "./assemble";
import { knowledgeValue } from "../../types/command";
import { asRecord, asString } from "../../lib/format";
import css from "./session.module.css";
import uiCss from "./transcript.module.css";

/** Prompt text beyond this many characters collapses behind an expand toggle. */
const PROMPT_LIMIT = 140;

function taskPrompt(task: ToolNode): string {
  const rec = asRecord(knowledgeValue(task.call.input));
  return asString(rec?.prompt) ?? task.name;
}

function TaskPrompt({ text }: { text: string }) {
  const [expanded, setExpanded] = useState(false);
  const ref = useRef<HTMLSpanElement>(null);
  const [overflows, setOverflows] = useState(false);
  // Measure, rather than trusting character count: a long unbroken token can
  // overflow even under the limit, and CJK width makes length alone unreliable.
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const check = () => setOverflows(text.length > PROMPT_LIMIT || el.scrollHeight - el.clientHeight > 2);
    check();
  }, [text]);
  const long = expanded || text.length <= PROMPT_LIMIT ? text : `${text.slice(0, PROMPT_LIMIT).trimEnd()}…`;
  return (
    <span className={uiCss.taskPrompt}>
      <span ref={ref} className={expanded ? undefined : uiCss.taskClamp} data-testid="task-prompt-text">
        {long}
      </span>
      {overflows ? (
        <button
          type="button"
          className={uiCss.taskToggle}
          data-testid="task-prompt-toggle"
          aria-expanded={expanded}
          onClick={() => setExpanded((v) => !v)}
        >
          {expanded ? "收起" : "展开"}
        </button>
      ) : null}
    </span>
  );
}

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
          // A foreground subagent has no result until it ends. A backgrounded
          // launch returns immediately with a `partial` result ("agent started
          // in background"); its completion folds a `final` result later.
          const background = task.result?.stage === "partial";
          const running = !task.result || task.result.stage !== "final";
          const failed = task.result?.outcome === "failed" || task.result?.outcome === "denied";
          const outcomeAttr = background ? "background" : (task.result?.outcome ?? "running");
          return (
            <li key={task.id} data-testid="task-track-item" data-task-outcome={outcomeAttr}>
              <TaskPrompt text={taskPrompt(task)} />
              <span className={failed ? uiCss.taskFailed : undefined}>
                {" · "}
                {background
                  ? "running in background"
                  : running
                    ? "running"
                    : (task.result?.outcome ?? "unknown")}
              </span>
            </li>
          );
        })}
      </ul>
    </aside>
  );
}
