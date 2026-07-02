import { useState } from "react";
import type { FlowEvent } from "../types";

function fmtTime(iso: string): string {
  const d = new Date(iso);
  const hh = String(d.getHours()).padStart(2, "0");
  const mm = String(d.getMinutes()).padStart(2, "0");
  const ss = String(d.getSeconds()).padStart(2, "0");
  const ms = String(d.getMilliseconds()).padStart(3, "0");
  return `${hh}:${mm}:${ss}.${ms}`;
}

function tryPrettyJson(s: string): { formatted: string; isJson: boolean } {
  try {
    const parsed = JSON.parse(s);
    return { formatted: JSON.stringify(parsed, null, 2), isJson: true };
  } catch {
    return { formatted: s, isJson: false };
  }
}

function DetailBlock({ detail }: { detail: string }) {
  const [expanded, setExpanded] = useState(false);
  const { formatted, isJson } = tryPrettyJson(detail);
  const isLong = formatted.length > 200;

  return (
    <div
      className={`event-detail ${expanded ? "event-detail-expanded" : ""}`}
      onClick={() => isLong && setExpanded(!expanded)}
      style={{ cursor: isLong ? "pointer" : "default" }}
    >
      <pre className="event-detail-pre">{formatted}</pre>
      {isLong && !expanded && (
        <div className="event-detail-fade">click to expand</div>
      )}
    </div>
  );
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
            {ev.detail && <DetailBlock detail={ev.detail} />}
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
