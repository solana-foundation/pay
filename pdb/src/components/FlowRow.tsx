import type { PaymentFlow } from "../types";
import { ProtocolBadge } from "./ProtocolBadge";
import { StatusIndicator } from "./StatusIndicator";

function fmtTime(iso: string): string {
  const d = new Date(iso);
  const hh = String(d.getHours()).padStart(2, "0");
  const mm = String(d.getMinutes()).padStart(2, "0");
  const ss = String(d.getSeconds()).padStart(2, "0");
  const ms = String(d.getMilliseconds()).padStart(3, "0");
  return `${hh}:${mm}:${ss}.${ms}`;
}

function fmtDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  return `${(ms / 1000).toFixed(1)}s`;
}

interface Props {
  flow: PaymentFlow;
  selected: boolean;
  onClick: () => void;
}

export function FlowRow({ flow, selected, onClick }: Props) {
  return (
    <div
      className={`flow-row${selected ? " selected" : ""}`}
      onClick={onClick}
    >
      <ProtocolBadge protocol={flow.protocol} />
      <span className="resource">{flow.resource}</span>
      <StatusIndicator status={flow.status} />
      {flow.amount && <span className="amount">{flow.amount}</span>}
      <span className="duration">{fmtDuration(flow.durationMs)}</span>
      <span className="timestamp">{fmtTime(flow.startedAt)}</span>
    </div>
  );
}
