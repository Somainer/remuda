import { Link, useParams } from "react-router-dom";
import { BOT_CHANNELS } from "../features/bots";
import { DeliveriesTable } from "../features/bots/DeliveriesTable";
import css from "../features/bots/bots.module.css";

export function BotsPage() {
  return (
    <div className={css.page} data-testid="bots-page">
      <header className={css.head}>
        <h1 className={css.title}>Bot</h1>
        <div className={css.sub}>独立 dispatcher · 不贴 app_secret</div>
      </header>
      <div className={css.body}>
        {BOT_CHANNELS.map((c) => (
          <Link
            key={c.channelId}
            to={`/bots/${c.channelId}`}
            className={css.row}
            data-testid="bot-row"
            data-channel={c.channelId}
          >
            <span className={`${css.dot} ${c.online ? "" : css.dotOff}`} aria-hidden />
            <span className={css.identity}>
              <span className={css.name}>
                {c.label} {c.profile}
              </span>
              <span className={css.meta}>
                {c.transport === "long-poll" ? "长连接" : "polling"} {c.online ? "●" : "○"} · 白名单{" "}
                {c.kind === "feishu" ? `${c.ownerOpenIds.length} open_id` : `${c.chatAllowlist.length} chat_id`}
              </span>
              <span className={css.meta}>会话键 {c.sessionKey}</span>
            </span>
          </Link>
        ))}
        <p className={css.foot}>独立 dispatcher app，不和会议纪要共用连接。本页不提供挂到现有 hub。</p>
      </div>
    </div>
  );
}

export function BotDetailPage() {
  const { channelId } = useParams();
  const c = BOT_CHANNELS.find((x) => x.channelId === channelId);
  if (!c) return <p style={{ padding: 16 }}>未知通道</p>;
  return (
    <div className={css.page} data-testid="bot-detail">
      <header className={css.head}>
        <Link to="/bots" className={css.back} aria-label="返回 Bot">
          ←
        </Link>
        <h1 className={css.title}>
          {c.label} {c.profile}
        </h1>
        <div className={css.sub}>
          {c.transport === "long-poll" ? "长连接" : "polling"} {c.online ? "●" : "○"}
        </div>
      </header>
      <div className={css.body}>
        <div className={css.section}>绑定</div>
        <div className={css.grid}>
          <div className={css.label}>通道</div>
          <div className={css.value}>{c.label}</div>
          <div className={css.label}>profile</div>
          <div className={css.value}>{c.profile}</div>
          <div className={css.label}>owner_open_ids</div>
          <div className={css.value} data-testid="bot-owners">
            owner_open_ids {c.ownerOpenIds.join(" ") || "—"}
          </div>
          <div className={css.label}>chat_allowlist</div>
          <div className={css.value} data-testid="bot-allowlist">
            chat_allowlist {c.chatAllowlist.join(" ") || "—"}
          </div>
          <div className={css.label}>会话键</div>
          <div className={css.value} data-testid="bot-session-key">
            会话键 {c.sessionKey}
          </div>
          <div className={css.label}>默认</div>
          <div className={css.value} data-testid="bot-defaults">
            默认主机 {c.defaultHost} · 默认项目 {c.defaultProject} · 默认 driver {c.defaultKind}
          </div>
          <div className={css.label}>TTL</div>
          <div className={css.value} data-testid="bot-ttl">
            TTL {c.sessionTtl} · Interaction ticket {c.ticketTtlMin} min（10–15）
          </div>
          <div className={css.label}>群策略</div>
          <div className={css.value} data-testid="bot-group-policy">
            群策略 {c.groupPolicy === "mention-only" ? "仅 @bot" : "允许未 @"}
          </div>
        </div>
        <div className={css.section}>最近投递</div>
        <DeliveriesTable rows={c.deliveries} />
        <p className={css.foot}>命令 /new /host /agent /model 只在 IM 侧覆盖路由。卡片点击再验 operator_id。</p>
      </div>
    </div>
  );
}
