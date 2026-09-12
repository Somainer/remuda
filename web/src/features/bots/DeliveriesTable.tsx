import { Link } from "react-router-dom";
import ui from "../../styles/ui.module.css";
import type { BotDelivery } from "./channels";

export function DeliveriesTable({ rows }: { rows: BotDelivery[] }) {
  if (!rows.length) return <p className={ui.listMeta}>尚无投递</p>;
  return (
    <table data-testid="bot-deliveries" style={{ width: "100%", borderCollapse: "collapse", fontSize: 13 }}>
      <thead>
        <tr className={ui.listMeta}>
          <th style={{ textAlign: "left", padding: "6px 8px 6px 0" }}>时间</th>
          <th style={{ textAlign: "left", padding: "6px 8px" }}>来源</th>
          <th style={{ textAlign: "left", padding: "6px 8px" }}>会话键</th>
          <th style={{ textAlign: "left", padding: "6px 0 6px 8px" }}>状态</th>
        </tr>
      </thead>
      <tbody>
        {rows.map((row) => (
          <tr key={`${row.at}:${row.sessionKey}`} data-testid="bot-delivery">
            <td style={{ padding: "6px 8px 6px 0" }}>{row.at}</td>
            <td style={{ padding: "6px 8px" }}>{row.actor}</td>
            <td style={{ padding: "6px 8px", fontFamily: "var(--mono)", fontSize: 12 }}>
              <Link to={`/s/${row.instanceId}`}>{row.sessionKey}</Link>
            </td>
            <td style={{ padding: "6px 0 6px 8px" }}>{row.state}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}
