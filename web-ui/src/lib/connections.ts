import type { ConnectionSummary, PaymentFlow } from "../types";

/** Shorten a wallet pubkey for display, e.g. "F82J…Utog". */
export function shortPayer(payer: string): string {
  if (payer.length <= 10) return payer;
  return `${payer.slice(0, 4)}…${payer.slice(-4)}`;
}

/** Format an aggregated USD amount with 4 decimals, e.g. "$0.0125". */
export function formatPaidUsd(value: number): string {
  return `$${value.toFixed(4)}`;
}

/** Compact token counts for group headers: 999 → "999", 1234 → "1.2k". */
export function formatTokenCount(value: number): string {
  const compact = (n: number, suffix: string) => {
    const s = n.toFixed(1).replace(/\.0$/, "");
    return `${s}${suffix}`;
  };
  if (value < 1_000) return String(value);
  if (value < 1_000_000) return compact(value / 1_000, "k");
  return compact(value / 1_000_000, "M");
}

/** Sort connections by most recent activity first (updatedAt desc). */
export function sortConnections(
  connections: ConnectionSummary[],
): ConnectionSummary[] {
  return [...connections].sort(
    (a, b) => Date.parse(b.updatedAt) - Date.parse(a.updatedAt),
  );
}

export interface FlowGroup {
  id: string; // connection id, or "other" for the trailing unmatched group
  connection: ConnectionSummary | null;
  flows: PaymentFlow[];
}

/**
 * Match a flow to its connection. Rule (connections are expected sorted
 * newest-activity first, so `find` picks the most recently active match):
 *   1. payer ↔ payer exact match when both are set;
 *   2. else the most recently active payer-less connection with the same
 *      clientIp;
 *   3. else the most recently active connection with the same clientIp —
 *      this catches 402 handshake exchanges whose paid twin is keyed by
 *      wallet: the handshake flow has no payer yet, but shares the client
 *      IP with the wallet-keyed connection it belongs to;
 *   4. else undefined → the flow lands in the trailing "other" group.
 */
export function matchConnection(
  flow: PaymentFlow,
  connections: ConnectionSummary[],
): ConnectionSummary | undefined {
  if (flow.payer) {
    const byPayer = connections.find((c) => c.payer === flow.payer);
    if (byPayer) return byPayer;
  }
  return (
    connections.find((c) => !c.payer && c.clientIp === flow.clientIp) ??
    connections.find((c) => c.clientIp === flow.clientIp)
  );
}

/**
 * Group flows by connection. Group order follows the connection order
 * (newest activity first); a trailing "other" group collects flows that
 * match no connection, and is only present when non-empty. Flows keep
 * their incoming (chronological) order within each group.
 */
export function groupFlows(
  flows: PaymentFlow[],
  connections: ConnectionSummary[],
): FlowGroup[] {
  const groups: FlowGroup[] = connections.map((connection) => ({
    id: connection.id,
    connection,
    flows: [],
  }));
  const byId = new Map(groups.map((g) => [g.id, g]));
  const other: FlowGroup = { id: "other", connection: null, flows: [] };

  for (const flow of flows) {
    const match = matchConnection(flow, connections);
    (match ? byId.get(match.id)! : other).flows.push(flow);
  }

  return other.flows.length > 0 ? [...groups, other] : groups;
}
