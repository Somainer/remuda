import type { DiffState } from "../features/session/assemble";
import ui from "../styles/ui.module.css";

const LABEL: Record<DiffState, string> = {
  proposed: "拟修改",
  applied: "已写入",
  unknown: "结果未知",
};

export function DiffBlock({
  path,
  diff,
  state,
}: {
  path: string;
  diff?: string | null;
  state: DiffState;
}) {
  const lines = (diff ?? "").split("\n");
  return (
    <div className={`${ui.diff} ${state === "unknown" ? ui.diffUnknown : ""}`}>
      <div className={ui.diffHead}>
        <span className={ui.path}>{path}</span>
        <span>{LABEL[state]}</span>
      </div>
      {lines.map((line, i) => {
        const kind = line.startsWith("+") && !line.startsWith("+++") ? ui.diffAdd : line.startsWith("-") && !line.startsWith("---") ? ui.diffDel : "";
        return (
          <div key={i} className={`${ui.diffLine} ${kind}`}>
            {line || " "}
          </div>
        );
      })}
    </div>
  );
}
