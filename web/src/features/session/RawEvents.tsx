import { useMemo, useRef, useState } from "react";
import type { Observation, ObservationKind } from "../../types/generated";
import { jsonPreview } from "../../lib/format";
import ui from "../../styles/ui.module.css";

const KINDS: ObservationKind[] = [
  "message",
  "thought",
  "tool_call",
  "tool_result",
  "interaction.requested",
  "interaction.answered",
  "interaction.expired",
  "workflow.run",
  "workflow.phase",
  "workflow.member",
  "lifecycle",
  "usage",
  "artifact",
  "raw_tty",
  "opaque",
];

const ROW = 56;

export function RawEvents({ events }: { events: Observation[] }) {
  const [kind, setKind] = useState<ObservationKind | "">("");
  const [open, setOpen] = useState<string | null>(null);
  const [scroll, setScroll] = useState(0);
  const scroller = useRef<HTMLDivElement>(null);
  const filtered = useMemo(() => (kind ? events.filter((e) => e.kind === kind) : events), [events, kind]);
  const height = 420;
  const start = Math.max(0, Math.floor(scroll / ROW) - 2);
  const visible = Math.ceil(height / ROW) + 4;
  const slice = filtered.slice(start, start + visible);

  return (
    <div data-testid="raw-events" style={{ padding: 12, display: "flex", flexDirection: "column", minHeight: 0, flex: 1 }}>
      <p className={ui.listMeta}>原始事件 · {filtered.length}</p>
      <div className={ui.row} style={{ margin: "8px 0", flexWrap: "wrap" }}>
        <button className={`${ui.chip} ${kind === "" ? ui.chipOn : ""}`} onClick={() => setKind("")}>
          全部
        </button>
        {KINDS.map((k) => (
          <button key={k} className={`${ui.chip} ${kind === k ? ui.chipOn : ""}`} onClick={() => setKind(k)}>
            {k}
          </button>
        ))}
      </div>
      <div
        ref={scroller}
        data-testid="raw-events-list"
        style={{ height, overflow: "auto", position: "relative", border: "1px solid var(--line)", borderRadius: 4 }}
        onScroll={(e) => setScroll((e.target as HTMLDivElement).scrollTop)}
      >
        <div style={{ height: filtered.length * ROW, position: "relative" }}>
          {slice.map((ev, i) => {
            const index = start + i;
            return (
              <button
                key={ev.eventId}
                type="button"
                data-testid="raw-event-row"
                data-kind={ev.kind}
                className={ui.listItem}
                style={{
                  position: "absolute",
                  top: index * ROW,
                  left: 0,
                  right: 0,
                  height: ROW,
                  textAlign: "left",
                }}
                onClick={() => setOpen(open === ev.eventId ? null : ev.eventId)}
              >
                <span>
                  <div>
                    seq {ev.seq} · {ev.kind} · {ev.completeness}
                  </div>
                  <div className={ui.listMeta}>
                    {ev.source.channel} · {ev.source.driverKind} · {ev.source.delivery}
                  </div>
                </span>
              </button>
            );
          })}
        </div>
      </div>
      {open ? (
        <pre className={ui.pre} data-testid="raw-event-json" style={{ marginTop: 8, maxHeight: 240 }}>
          {jsonPreview(filtered.find((e) => e.eventId === open) ?? null)}
        </pre>
      ) : (
        <p className={ui.listMeta}>点一行看 envelope JSON。</p>
      )}
    </div>
  );
}
