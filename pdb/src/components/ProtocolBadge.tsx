import type { Protocol } from "../types";

export function ProtocolBadge({ protocol }: { protocol: Protocol }) {
  return <span className={`badge ${protocol}`}>{protocol}</span>;
}
