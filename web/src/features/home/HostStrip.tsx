import { useEffect, useState, type ReactNode } from "react";
import { Link } from "react-router-dom";
import { useHub } from "../../lib/store";
import { useNowTick } from "../../lib/useNowTick";
import { readPushStatus, type PushStatus } from "../../lib/push";
import { useHomeFreshness } from "./freshness";
import { hostStrip } from "./hostStrip";
import css from "./hostStrip.module.css";

/**
 * ② 主机条 (home.md §1 ②, §9).
 *
 * Phone: a 28px row under ①, scrolling with the page. Desktop: the 32px band
 * under the PageHeader, whose right end carries the 「空间 ▸」 toggle
 * (`trailing`). Offline hosts come first as 「离线 Nm」; when every host is OK
 * the strip collapses to one [N/M 主机在线 ›] chip. The Hub chip appears only
 * while the list is stale (home.md §8) and the 「通知未开」 chip only while
 * notifications are not granted. ★ belongs to home-main-extras and is absent.
 */
export function HostStrip({
  placement,
  trailing,
}: {
  placement: "phone" | "desktop";
  /** Desktop only: the 「空间 ▸」 toggle at the band's right end. */
  trailing?: ReactNode;
}) {
  const hub = useHub();
  const freshness = useHomeFreshness();
  const nowMs = useNowTick();
  const [push, setPush] = useState<PushStatus | null>(null);
  useEffect(() => {
    let live = true;
    void readPushStatus().then((status) => {
      if (live) setPush(status);
    });
    return () => {
      live = false;
    };
  }, []);

  const model = hostStrip(hub.hosts, nowMs);
  // A fresh verdict is the only one without a Hub chip; recovering and
  // unreachable both name what the operator would tap.
  const hubDown = freshness.state === "stale";
  const hubLabel =
    hub.connection === "recovering" || hub.connection === "reconnecting"
      ? "Hub 重连中"
      : "Hub 连不上";
  // An unsupported Notification API offers nothing a tap could enable.
  const notificationsOff =
    push !== null && push.permission !== "granted" && push.permission !== "unsupported";
  const notificationsHref = placement === "phone" ? "/m/inbox" : "/settings#notifications";

  return (
    <div className={css.strip} data-testid="host-strip" data-placement={placement}>
      <div className={css.chips}>
        {model.total === 0 ? (
          <Link className={css.chip} data-testid="host-add" to="/hosts">
            添加主机 ›
          </Link>
        ) : model.collapsed ? (
          <Link className={css.chip} data-testid="host-summary" to="/hosts">
            {model.online}/{model.total} 主机在线 ›
          </Link>
        ) : (
          model.chips.map((chip) => (
            <Link
              key={chip.id}
              className={css.chip}
              data-testid="host-chip"
              data-host-id={chip.id}
              data-offline={chip.offline ? "1" : "0"}
              to={`/hosts/${chip.id}`}
              title={chip.label}
            >
              <span className={css.name}>{chip.label}</span>
              {chip.offline ? (
                <span className={css.offline}>
                  ┄离线{chip.age ? ` ${chip.age}` : ""}
                </span>
              ) : null}
            </Link>
          ))
        )}
        {hubDown ? (
          <span className={css.chip} data-testid="host-hub-chip" data-tone="attention">
            {hubLabel}
          </span>
        ) : null}
        {notificationsOff ? (
          <Link className={css.chip} data-testid="host-notify-chip" to={notificationsHref}>
            通知未开
          </Link>
        ) : null}
      </div>
      {trailing ? <div className={css.trailing}>{trailing}</div> : null}
    </div>
  );
}
