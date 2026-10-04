import type { PaymentFlow } from "../types";
import { currencyLabel, formatUnits } from "./format";

export type ChannelState = "open" | "closed" | "failed" | "unknown";
export type ChannelAction =
  | "open"
  | "topup"
  | "voucher"
  | "authorization"
  | "commit"
  | "close"
  | "refund"
  | "other";

export interface ChannelRequest {
  readonly flow: PaymentFlow;
  readonly action?: ChannelAction;
  readonly actionLabel?: string;
  readonly amount?: string;
}

export interface PaymentChannel {
  readonly id: string;
  readonly protocol: "mpp" | "x402";
  readonly scheme: string;
  readonly state: ChannelState;
  readonly currency: string;
  readonly decimals: number;
  readonly payer?: string;
  readonly recipient?: string;
  readonly deposited?: string;
  readonly consumed?: string;
  readonly remaining?: string;
  readonly usagePercent?: number;
  readonly requestPrice?: string;
  readonly remainingRequests?: number;
  readonly capacityTicks?: number;
  readonly usedTicks?: number;
  readonly requestsPerTick?: number;
  readonly startedAt: string;
  readonly updatedAt: string;
  readonly requests: ChannelRequest[];
}

function channelId(flow: PaymentFlow): string | undefined {
  return flow.payment?.channelId ?? flow.session?.sessionId;
}

function stableBaseUnits(value: string | undefined, decimals: number): bigint | undefined {
  if (!value) return undefined;
  if (/^\d+$/.test(value)) return BigInt(value);

  const match = /^(\d+)(?:\.(\d+))?(?:\s+\S+)?$/.exec(value.trim());
  if (!match) return undefined;
  const fraction = (match[2] ?? "").slice(0, decimals).padEnd(decimals, "0");
  return BigInt(match[1]) * 10n ** BigInt(decimals) + BigInt(fraction || "0");
}

function maximum(values: Array<bigint | undefined>): bigint | undefined {
  const known = values.filter((value): value is bigint => value !== undefined);
  return known.length > 0
    ? known.reduce((highest, value) => (value > highest ? value : highest))
    : undefined;
}

function safeCount(value: bigint): number {
  return Number(value > BigInt(Number.MAX_SAFE_INTEGER) ? BigInt(Number.MAX_SAFE_INTEGER) : value);
}

function normalizeAction(flow: PaymentFlow): Pick<ChannelRequest, "action" | "actionLabel"> {
  const raw = flow.session?.action ?? flow.payment?.action;
  if (!raw) return {};
  const normalized = raw.toLowerCase().replace(/[\s_-]+/g, "");
  if (normalized === "open" || normalized.includes("channelopened")) {
    return { action: "open", actionLabel: "Opened channel" };
  }
  if (normalized === "topup" || normalized.includes("channeltoppedup")) {
    return { action: "topup", actionLabel: "Topped up" };
  }
  if (normalized.includes("voucher")) {
    return { action: "voucher", actionLabel: "Voucher" };
  }
  if (normalized.includes("authorization")) {
    return { action: "authorization", actionLabel: "Authorized" };
  }
  if (normalized === "commit") return { action: "commit", actionLabel: "Committed" };
  if (normalized === "close") return { action: "close", actionLabel: "Closed" };
  if (normalized === "refund") return { action: "refund", actionLabel: "Refund" };
  return { action: "other", actionLabel: raw };
}

function requestAmount(flow: PaymentFlow): string | undefined {
  const action = normalizeAction(flow).action;
  let amount: string | undefined;
  if (action === "open" || action === "topup") {
    amount = flow.payment?.depositAmount ?? flow.session?.deposit;
  } else if (action === "voucher" || action === "commit") {
    amount = flow.payment?.voucherAmount ?? flow.session?.cumulative;
  } else if (action === "authorization") {
    amount =
      flow.payment?.chargeAmount ??
      flow.payment?.authorizedAmount ??
      flow.session?.approvedAmount;
  } else {
    amount = flow.payment?.settlementAmount ?? flow.amount;
  }
  if (amount && /^\d+$/.test(amount)) {
    return formatUnits(
      amount,
      flow.session?.decimals ?? 6,
      flow.session?.currency ?? flow.payment?.asset ?? "USDC",
    );
  }
  return amount;
}

function channelState(flows: PaymentFlow[]): ChannelState {
  const latest = flows.at(-1);
  if (latest?.status === "failed" || latest?.session?.state === "failed") return "failed";
  if (
    latest?.session?.state === "closed" ||
    ["close", "refund"].includes(normalizeAction(latest ?? flows[0]).action ?? "")
  ) {
    return "closed";
  }
  if (flows.some((flow) => flow.status === "resource-delivered")) return "open";
  return "unknown";
}

function summarizeChannel(id: string, flows: PaymentFlow[]): PaymentChannel {
  const ordered = [...flows].sort((a, b) => a.startedAt.localeCompare(b.startedAt));
  const latest = ordered.reduce((current, flow) =>
    flow.updatedAt > current.updatedAt ? flow : current,
  );
  const isX402 = ordered.some((flow) => flow.payment?.channelId === id);
  const decimals = latest.session?.decimals ?? 6;
  const rawCurrency = latest.session?.currency ?? latest.payment?.asset ?? "USDC";
  const currency = currencyLabel(rawCurrency);

  const paymentDeposits = ordered.map((flow) =>
    stableBaseUnits(flow.payment?.depositAmount, decimals),
  );
  const reportedBalance = maximum(
    ordered.map((flow) => stableBaseUnits(flow.payment?.channelBalance, decimals)),
  );
  const deposited = isX402
    ? reportedBalance ??
      paymentDeposits
        .filter((value): value is bigint => value !== undefined)
        .reduce<bigint | undefined>((sum, value) => (sum ?? 0n) + value, undefined)
    : maximum(
        ordered.map((flow) =>
          stableBaseUnits(
            flow.session?.deposit ?? flow.session?.approvedAmount ?? flow.session?.cap,
            decimals,
          ),
        ),
      );
  const consumed = maximum(
    ordered.map((flow) =>
      stableBaseUnits(
        flow.payment?.chargedCumulativeAmount ??
          flow.payment?.voucherAmount ??
          flow.session?.cumulative,
        decimals,
      ),
    ),
  );
  const remaining =
    deposited !== undefined && consumed !== undefined
      ? deposited > consumed
        ? deposited - consumed
        : 0n
      : deposited;
  const usagePercent =
    deposited !== undefined && deposited > 0n && consumed !== undefined
      ? Math.min(100, Number((consumed * 10_000n) / deposited) / 100)
      : undefined;
  const latestCharge = [...ordered]
    .reverse()
    .map((flow) => stableBaseUnits(flow.payment?.chargeAmount ?? flow.amount, decimals))
    .find((amount) => amount !== undefined && amount > 0n);
  const totalRequests =
    isX402 && deposited !== undefined && latestCharge !== undefined
      ? deposited / latestCharge
      : undefined;
  const remainingRequests =
    remaining !== undefined && latestCharge !== undefined
      ? remaining / latestCharge
      : undefined;
  const capacityTicks =
    totalRequests !== undefined && totalRequests > 0n
      ? Math.min(24, safeCount(totalRequests))
      : undefined;
  const requestsPerTick =
    capacityTicks !== undefined && totalRequests !== undefined
      ? Math.max(1, Math.ceil(safeCount(totalRequests) / capacityTicks))
      : undefined;
  const usedTicks =
    capacityTicks !== undefined && totalRequests !== undefined && remainingRequests !== undefined
      ? Math.min(
          capacityTicks,
          Math.round(
            ((safeCount(totalRequests) - safeCount(remainingRequests)) / safeCount(totalRequests)) *
              capacityTicks,
          ),
        )
      : undefined;

  return {
    id,
    protocol: isX402 ? "x402" : "mpp",
    scheme: isX402 ? "batch-settlement" : "session",
    state: channelState(ordered),
    currency,
    decimals,
    payer: latest.payer ?? latest.session?.payer,
    recipient: latest.payment?.recipient ?? latest.session?.recipient,
    deposited: deposited === undefined ? undefined : formatUnits(deposited.toString(), decimals, currency),
    consumed: consumed === undefined ? undefined : formatUnits(consumed.toString(), decimals, currency),
    remaining: remaining === undefined ? undefined : formatUnits(remaining.toString(), decimals, currency),
    usagePercent,
    requestPrice:
      latestCharge === undefined
        ? undefined
        : formatUnits(latestCharge.toString(), decimals, currency),
    remainingRequests:
      remainingRequests === undefined ? undefined : safeCount(remainingRequests),
    capacityTicks,
    usedTicks,
    requestsPerTick,
    startedAt: ordered[0].startedAt,
    updatedAt: latest.updatedAt,
    requests: ordered.map((flow) => ({
      flow,
      ...normalizeAction(flow),
      amount: requestAmount(flow),
    })),
  };
}

/** Group captured payment flows by their concrete channel identifier. */
export function paymentChannels(flows: PaymentFlow[]): PaymentChannel[] {
  const grouped = new Map<string, PaymentFlow[]>();
  for (const flow of flows) {
    const id = channelId(flow);
    if (!id) continue;
    grouped.set(id, [...(grouped.get(id) ?? []), flow]);
  }
  return [...grouped.entries()]
    .map(([id, channelFlows]) => summarizeChannel(id, channelFlows))
    .sort((a, b) => b.updatedAt.localeCompare(a.updatedAt));
}

/** Match a channel or any of its requests against debugger search text. */
export function channelMatches(channel: PaymentChannel, query: string): boolean {
  const normalized = query.trim().toLowerCase();
  if (!normalized) return true;
  return [
    channel.id,
    channel.protocol,
    channel.scheme,
    channel.currency,
    channel.payer,
    channel.recipient,
    ...channel.requests.flatMap(({ flow }) => [
      flow.resource,
      flow.inference?.model,
      flow.inference?.provider,
    ]),
  ].some((value) => value?.toLowerCase().includes(normalized));
}
