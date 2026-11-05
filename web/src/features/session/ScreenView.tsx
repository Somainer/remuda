import type { Instance } from "../../types/instance";
import type { Observation } from "../../types/observation";
import { latestScreenFromObservations } from "../../lib/screen";
import { knowledgeValue } from "../../types/command";
import css from "./screen.module.css";

export function ScreenView({
  instance,
  events,
  live = null,
}: {
  instance: Instance;
  events: Observation[];
  /** D-062: the live viewport of a claimed pane, which journals no screens. */
  live?: string[] | null;
}) {
  const snapshot = latestScreenFromObservations(events);
  const source = live?.length ? live : snapshot.lines;
  const lines = source.length ? source : ["(waiting for screen snapshot…)"];
  const activity = knowledgeValue(instance.activity) ?? "unknown";
  return (
    <div className={css.screen} data-testid="screen-view" data-lifecycle={instance.lifecycle} data-activity={activity}>
      <div className={css.screenMeta} data-testid="screen-lifecycle">
        <span>lifecycle {instance.lifecycle}</span>
        <span className={css.dotSep}>·</span>
        <span>activity {activity}</span>
        <span className={css.dotSep}>·</span>
        <span>{instance.driver}</span>
        {instance.cwd ? (
          <>
            <span className={css.dotSep}>·</span>
            <span>{instance.cwd}</span>
          </>
        ) : null}
      </div>
      <pre className={css.screenPre} data-testid="screen-snapshot">
        {lines.join("\n")}
      </pre>
    </div>
  );
}
