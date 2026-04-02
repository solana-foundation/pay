import type { FlowEvent } from "../types";

function fmtTime(iso: string): string {
  const d = new Date(iso);
  const hh = String(d.getHours()).padStart(2, "0");
  const mm = String(d.getMinutes()).padStart(2, "0");
  const ss = String(d.getSeconds()).padStart(2, "0");
  const ms = String(d.getMilliseconds()).padStart(3, "0");
  return `${hh}:${mm}:${ss}.${ms}`;
}

interface Props {
  events: FlowEvent[];
}

export function EventLog({ events }: Props) {
  return (
    <div className="event-log">
      <h3>Events</h3>
      {events.map((ev, i) => (
        <div className="event-entry" key={i}>
          <span className="event-ts">{fmtTime(ev.ts)}</span>
          <div className="event-content">
            <div className="event-msg">{ev.message}</div>
            {ev.detail && <div className="event-detail">{ev.detail}</div>}
          </div>
        </div>
      ))}
      {events.length === 0 && (
        <div className="event-entry">
          <span className="event-ts">--:--:--</span>
          <div className="event-msg" style={{ color: "var(--muted)" }}>
            No events yet
          </div>
        </div>
      )}
    </div>
  );
}
