import type { DiffState } from "../features/session/assemble";
import css from "./diff.module.css";

export function DiffBlock({
  path,
  diff,
  state,
}: {
  path: string;
  diff?: string | null;
  state: DiffState;
}) {
  const lines = (diff ?? "").split("\n").filter((line) => line !== "@@" && !line.startsWith("@@"));
  return (
    <div className={css.diff} data-path={path} data-state={state}>
      {lines.map((line, i) => {
        const add = line.startsWith("+") && !line.startsWith("+++");
        const del = line.startsWith("-") && !line.startsWith("---");
        const gutter = add ? "+" : del ? "−" : "";
        return (
          <div key={i} className={`${css.diffLine} ${add ? css.diffAdd : del ? css.diffDel : ""}`}>
            <span className={css.diffGutter}>{gutter || " "}</span>
            <span className={css.diffBody}>{line.replace(/^[-+]/, "") || " "}</span>
          </div>
        );
      })}
    </div>
  );
}
