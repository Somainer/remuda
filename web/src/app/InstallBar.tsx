import { useInstallPrompt } from "../lib/pwa";
import { Button } from "../components/Button";
import ui from "../styles/ui.module.css";
import { useState } from "react";

export function InstallBar() {
  const event = useInstallPrompt();
  const [hidden, setHidden] = useState(false);
  if (!event || hidden) return null;
  return (
    <div className={ui.install}>
      <span>添加到主屏幕</span>
      <span>
        <Button
          variant="primary"
          onClick={() => {
            void event.prompt();
            setHidden(true);
          }}
        >
          安装
        </Button>
        <Button variant="ghost" onClick={() => setHidden(true)}>
          稍后
        </Button>
      </span>
    </div>
  );
}
