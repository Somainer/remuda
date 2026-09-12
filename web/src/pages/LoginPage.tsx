import { useNavigate } from "react-router-dom";
import { Button } from "../components/Button";
import { hubStore } from "../lib/store";
import ui from "../styles/ui.module.css";

export function LoginPage() {
  const navigate = useNavigate();
  return (
    <div style={{ minHeight: "100dvh", display: "grid", placeItems: "center", padding: 24 }}>
      <form
        className={ui.card}
        style={{ width: "min(360px, 100%)" }}
        onSubmit={(e) => {
          e.preventDefault();
          hubStore.setAuthed(true);
          void hubStore.bootstrap();
          navigate("/sessions");
        }}
      >
        <h1 style={{ fontSize: 20 }}>runtime</h1>
        <p className={ui.listMeta}>设备登录。Mock 模式无需凭据。</p>
        <label className={ui.field}>
          设备名
          <input className={ui.input} defaultValue="this-device" />
        </label>
        <div className={ui.row} style={{ marginTop: 12, justifyContent: "flex-end" }}>
          <Button variant="primary" type="submit">
            进入
          </Button>
        </div>
      </form>
    </div>
  );
}
