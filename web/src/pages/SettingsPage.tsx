import { Link } from "react-router-dom";
import { Button } from "../components/Button";
import { subscribePush } from "../lib/push";
import { MORE_NAV } from "../lib/nav";
import ui from "../styles/ui.module.css";

export function SettingsPage() {
  return (
    <div style={{ padding: 16 }}>
      <h1 style={{ fontSize: 18 }}>设置</h1>
      <p className={ui.listMeta}>v1 无浅色开关。主题 Night Corral。</p>
      <p className={ui.listMeta}>iOS 需加到主屏幕后才有 Notification。</p>
      <div className={ui.row} style={{ margin: "12px 0" }}>
        <Button
          onClick={() => {
            void subscribePush();
          }}
        >
          开启推送
        </Button>
      </div>
      <h2 style={{ fontSize: 14 }}>更多</h2>
      {MORE_NAV.map((item) => (
        <Link key={item.id} to={item.to} className={ui.listItem}>
          {item.label}
        </Link>
      ))}
    </div>
  );
}

export function PairPage() {
  return (
    <div style={{ padding: 24 }}>
      <h1>设备配对</h1>
      <p className={ui.listMeta}>第一里程碑占位。</p>
    </div>
  );
}
