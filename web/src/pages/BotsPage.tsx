import { Link, useParams } from "react-router-dom";
import { BOT_CHANNELS } from "../features/bots";
import { DeliveriesTable } from "../features/bots/DeliveriesTable";
import { PageHeader } from "../components/PageHeader";
import css from "../features/bots/bots.module.css";

export function BotsPage() {
  return (
    <div className={css.page} data-testid="bots-page">
      <PageHeader title="Bot" />
      <div className={css.body}>
        <div className={css.inner}>
          <div className={css.list}>
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
                    <span className={`${css.metaDot} ${c.online ? "" : css.metaDotOff}`} aria-hidden />
                    {c.transport === "long-poll" ? "长连接" : "polling"} · 白名单{" "}
                    {c.kind === "feishu" ? `${c.ownerOpenIds.length} open_id` : `${c.chatAllowlist.length} chat_id`}
                  </span>
                  <span className={css.meta}>会话键 {c.sessionKey}</span>
                </span>
              </Link>
            ))}
          </div>
          <p className={css.foot}>独立 dispatcher app，不和会议纪要共用连接。本页不提供挂到现有 hub。</p>
        </div>
      </div>
    </div>
  );
}

export function BotDetailPage() {
  const { channelId } = useParams();
  const c = BOT_CHANNELS.find((x) => x.channelId === channelId);
  if (!c) {
    return (
      <div className={css.page} data-testid="bot-detail">
        <PageHeader crumbs={[{ label: "Bot", to: "/bots" }]} title="未知通道" />
      </div>
    );
  }
  return (
    <div className={css.page} data-testid="bot-detail">
      <PageHeader crumbs={[{ label: "Bot", to: "/bots" }]} title={`${c.label} ${c.profile}`} />
      <div className={css.body}>
        <div className={css.inner}>
          <p className={css.meta}>
            <span className={`${css.metaDot} ${c.online ? "" : css.metaDotOff}`} aria-hidden />
            {c.transport === "long-poll" ? "长连接" : "polling"}
          </p>
          <section>
            <h2 className={css.sectionLabel}>绑定</h2>
            <dl className={css.grid}>
              <dt className={css.label}>通道</dt>
              <dd>{c.label}</dd>
              <dt className={css.label}>profile</dt>
              <dd>{c.profile}</dd>
              <dt className={css.label}>owner_open_ids</dt>
              <dd data-testid="bot-owners">
                owner_open_ids {c.ownerOpenIds.join(" ") || "—"}
              </dd>
              <dt className={css.label}>chat_allowlist</dt>
              <dd data-testid="bot-allowlist">
                chat_allowlist {c.chatAllowlist.join(" ") || "—"}
              </dd>
              <dt className={css.label}>会话键</dt>
              <dd data-testid="bot-session-key">
                会话键 {c.sessionKey}
              </dd>
              <dt className={css.label}>默认</dt>
              <dd data-testid="bot-defaults">
                默认主机 {c.defaultHost} · 默认项目 {c.defaultProject} · 默认 driver {c.defaultKind}
              </dd>
              <dt className={css.label}>TTL</dt>
              <dd data-testid="bot-ttl">
                TTL {c.sessionTtl} · Interaction ticket {c.ticketTtlMin} min（10–15）
              </dd>
              <dt className={css.label}>群策略</dt>
              <dd data-testid="bot-group-policy">
                群策略 {c.groupPolicy === "mention-only" ? "仅 @bot" : "允许未 @"}
              </dd>
            </dl>
          </section>
          <section>
            <h2 className={css.sectionLabel}>最近投递</h2>
            <DeliveriesTable rows={c.deliveries} />
          </section>
          <p className={css.foot}>命令 /new /host /agent /model 只在 IM 侧覆盖路由。卡片点击再验 operator_id。</p>
        </div>
      </div>
    </div>
  );
}
