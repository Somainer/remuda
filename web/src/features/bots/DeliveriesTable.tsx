import { Link } from "react-router-dom";
import css from "./bots.module.css";
import type { BotDelivery } from "./channels";

export function DeliveriesTable({ rows }: { rows: BotDelivery[] }) {
  if (!rows.length) return <p className={css.foot}>尚无投递</p>;
  return (
    <table className={css.table} data-testid="bot-deliveries">
      <thead>
        <tr>
          <th>时间</th>
          <th>来源</th>
          <th>会话键</th>
          <th>状态</th>
        </tr>
      </thead>
      <tbody>
        {rows.map((row) => (
          <tr key={`${row.at}:${row.sessionKey}`} data-testid="bot-delivery">
            <td>{row.at}</td>
            <td>{row.actor}</td>
            <td>
              <Link to={`/s/${row.instanceId}`}>{row.sessionKey}</Link>
            </td>
            <td>{row.state}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}
