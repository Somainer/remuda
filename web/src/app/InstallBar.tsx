import { useState } from "react";
import { useInstallPrompt } from "../lib/pwa";
import ui from "../styles/ui.module.css";

export function InstallBar() {
  const offer = useInstallPrompt();
  const [hidden, setHidden] = useState(false);
  if (!offer || hidden) return null;

  const ios = offer.kind === "ios";
  const copy = ios ? "添加到主屏幕 · 分享 → 加到主屏幕" : "添加到主屏幕";

  return (
    <div className={ui.install} data-testid="install-bar" role="region" aria-label="安装应用">
      <span>{copy}</span>
      <span className={ui.installActions}>
        {offer.kind === "prompt" || offer.kind === "demo" ? (
          <button
            type="button"
            className={`${ui.btnPrimary} ${ui.installBtn}`}
            onClick={() => {
              if (offer.kind === "prompt") void offer.prompt();
              setHidden(true);
            }}
          >
            安装
          </button>
        ) : null}
        <button type="button" className={`${ui.btnGhost} ${ui.installBtn}`} onClick={() => setHidden(true)}>
          {ios ? "知道了" : "稍后"}
        </button>
      </span>
    </div>
  );
}
