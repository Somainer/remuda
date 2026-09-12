import { Link, useParams } from "react-router-dom";
import { BOT_CHANNELS } from "../features/bots";
import { DeliveriesTable } from "../features/bots/DeliveriesTable";
import ui from "../styles/ui.module.css";

export function BotsPage() {
  return (
    <div style={{ padding: 16 }} data-testid="bots-page">
      <h1 style={{ fontSize: 18 }}>Bot</h1>
      <p className={ui.listMeta}>独立 dispatcher app，不和会议纪要共用连接。本页不贴 app_secret，不提供挂到现有 hub。</p>
      {BOT_CHANNELS.map((c) => (
        <Link
          key={c.channelId}
          to={`/bots/${c.channelId}`}
          className={ui.listItem}
          data-testid="bot-row"
          data-channel={c.channelId}
        >
          <span className={`${ui.dot} ${c.online ? ui.dotIdle : ui.dotUnknown}`} aria-hidden />
          <span>
            <div>
              {c.label} {c.profile}
            </div>
            <div className={ui.listMeta}>
              {c.transport === "long-poll" ? "长连接" : "polling"} {c.online ? "●" : "○"}  白名单{" "}
              {c.kind === "feishu" ? `${c.ownerOpenIds.length} open_id` : `${c.chatAllowlist.length} chat_id`}
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
    <div style={{ padding: 16 }} data-testid="bot-detail">
      <p>
        <Link to="/bots">← Bot</Link>
      </p>
      <h1>
        {c.label} {c.profile}
      </h1>
      <p className={ui.listMeta}>
        {c.transport === "long-poll" ? "长连接" : "polling"} {c.online ? "●" : "○"}
      </p>
      <h2 style={{ fontSize: 14 }}>绑定</h2>
      <p className={ui.listMeta}>通道 {c.label} · profile {c.profile}</p>
      <p className={ui.listMeta} data-testid="bot-owners">
        owner_open_ids {c.ownerOpenIds.join(" ") || "—"}
      </p>
      <p className={ui.listMeta} data-testid="bot-allowlist">
        chat_allowlist {c.chatAllowlist.join(" ") || "—"}
      </p>
      <p className={ui.listMeta} data-testid="bot-session-key">
        会话键 {c.sessionKey}
      </p>
      <p className={ui.listMeta} data-testid="bot-defaults">
        默认主机 {c.defaultHost} · 默认项目 {c.defaultProject} · 默认 driver {c.defaultKind}
      </p>
      <p className={ui.listMeta} data-testid="bot-ttl">
        TTL {c.sessionTtl} · Interaction ticket {c.ticketTtlMin} min（10–15）
      </p>
      <p className={ui.listMeta} data-testid="bot-group-policy">
        群策略 {c.groupPolicy === "mention-only" ? "仅 @bot" : "允许未 @"}
      </p>
      <h2 style={{ fontSize: 14 }}>最近投递</h2>
      <DeliveriesTable rows={c.deliveries} />
      <p className={ui.listMeta}>命令 /new /host /agent /model 只在 IM 侧覆盖路由。卡片点击再验 operator_id。</p>
    </div>
  );
}
