import { Link, useParams } from "react-router-dom";
import { BOT_CHANNELS } from "../features/bots/channels";
import ui from "../styles/ui.module.css";

export function BotsPage() {
  return (
    <div style={{ padding: 16 }}>
      <h1 style={{ fontSize: 18 }}>Bot</h1>
      {BOT_CHANNELS.map((c) => (
        <Link key={c.channelId} to={`/bots/${c.channelId}`} className={ui.listItem}>
          <span>
            <div>
              {c.label} {c.profile}
            </div>
            <div className={ui.listMeta}>会话键 {c.sessionKey}</div>
          </span>
        </Link>
      ))}
    </div>
  );
}

export function BotDetailPage() {
  const { channelId } = useParams();
  const c = BOT_CHANNELS.find((x) => x.channelId === channelId);
  if (!c) return <p style={{ padding: 16 }}>未知通道</p>;
  return (
    <div style={{ padding: 16 }}>
      <h1>
        {c.label} {c.profile}
      </h1>
      <p className={ui.listMeta}>{c.sessionKey}</p>
      <p>独立 dispatcher app。本页不贴 app_secret。</p>
    </div>
  );
}
