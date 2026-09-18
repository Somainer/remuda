import { useState } from "react";
import { applyUpdate, useInstallPrompt, useUpdateAvailable } from "../lib/pwa";
import ui from "../styles/ui.module.css";

export function InstallBar() {
  const offer = useInstallPrompt();
  const updateAvailable = useUpdateAvailable();
  const [hidden, setHidden] = useState(false);

  // An update over a live shell wins the bar: leaving it stranded is the very
  // failure this fixes, so it takes priority over the install offer.
  if (updateAvailable) {
    return (
      <div className={ui.install} data-testid="update-bar" role="region" aria-label="应用更新">
        <span>有新版本，点击刷新</span>
        <span className={ui.installActions}>
          <button
            type="button"
            className={`${ui.btnPrimary} ${ui.installBtn}`}
            data-testid="update-refresh"
            onClick={() => applyUpdate()}
          >
            刷新
          </button>
        </span>
      </div>
    );
  }

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
