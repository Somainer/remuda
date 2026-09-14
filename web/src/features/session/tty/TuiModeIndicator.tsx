import type { TuiMode } from "../../../types/instance";
import css from "./TerminalView.module.css";

/** The snapshot reports terminal state; a launch preference cannot prove it. */
export function TuiModeIndicator({
  altScreen,
  requestedTui,
  hasEngagedAltScreen,
}: {
  altScreen: boolean | undefined;
  requestedTui?: TuiMode | null;
  hasEngagedAltScreen: boolean;
}) {
  return (
    <>
      <span
        className={css.modePill}
        data-testid="tty-alt-screen"
        data-alt-screen={altScreen === undefined ? "unknown" : String(altScreen)}
        title="根据终端实际画面检测"
      >
        {altScreen === undefined ? "渲染方式待检测" : altScreen ? "全屏渲染" : "行内渲染"}
      </span>
      {requestedTui === "fullscreen" && altScreen === false && !hasEngagedAltScreen ? (
        <span className={css.geo} data-testid="tty-tui-mismatch">
          已请求全屏，尚未检测到全屏画面
        </span>
      ) : null}
    </>
  );
}
