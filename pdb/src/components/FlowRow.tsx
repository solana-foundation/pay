import type { PaymentFlow } from "../types";
import { Amount } from "./Amount";
import { ProtocolBadge } from "./ProtocolBadge";
import { StatusIndicator } from "./StatusIndicator";
import type { SessionInfo } from "../types";

const U64_MAX = "18446744073709551615";
const USDC_MAINNET_MINT = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

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

function shortAddr(value: string): string {
  return value.length > 12 ? `${value.slice(0, 4)}...${value.slice(-4)}` : value;
}

function currencyLabel(currency: string | undefined): string {
  if (!currency || currency === USDC_MAINNET_MINT) return "USDC";
  return currency.length > 12 ? shortAddr(currency) : currency;
}

function formatSessionUnits(
  raw: string | undefined,
  decimals = 6,
  currency = "USDC",
): string | undefined {
  if (!raw) return undefined;
  if (raw === U64_MAX) return "unbounded";
  if (!/^\d+$/.test(raw)) return raw;

  try {
    const value = BigInt(raw);
    const scale = 10n ** BigInt(decimals);
    const whole = value / scale;
    const fraction = value % scale;
    if (fraction === 0n) return `${whole.toString()} ${currencyLabel(currency)}`;

    const fractionText = fraction
      .toString()
      .padStart(decimals, "0")
      .replace(/0+$/, "");
    return `${whole.toString()}.${fractionText} ${currencyLabel(currency)}`;
  } catch {
    return `${raw} ${currencyLabel(currency)}`;
  }
}

function sessionRowAmount(session: SessionInfo | undefined): string | undefined {
  if (!session) return undefined;
  const decimals = session.decimals ?? 6;
  const currency = session.currency ?? "USDC";
  if (session.cumulative) {
    return `paid ${formatSessionUnits(session.cumulative, decimals, currency)}`;
  }
  const cap = session.deposit ?? session.approvedAmount ?? session.cap;
  if (cap) return `cap ${formatSessionUnits(cap, decimals, currency)}`;
  return undefined;
}

interface Props {
  flow: PaymentFlow;
  selected: boolean;
  onClick: () => void;
}

export function FlowRow({ flow, selected, onClick }: Props) {
  const channelOpen = flow.session?.state === "open";
  const sessionAmount = sessionRowAmount(flow.session);

  return (
    <div
      className={`flow-row${selected ? " selected" : ""}${channelOpen ? " channel-open" : ""}`}
      onClick={onClick}
    >
      <ProtocolBadge protocol={flow.protocol} />
      <span className="resource">{flow.resource}</span>
      {channelOpen && (
        <span className="session-inline session-inline-status">
          <span className="session-inline-dot" />
          open
        </span>
      )}
      {!channelOpen && <StatusIndicator status={flow.status} />}
      <span className="amount-slot">
        {flow.session
          ? sessionAmount && <span className="session-row-amount">{sessionAmount}</span>
          : flow.amount && <Amount value={parseFloat(flow.amount)} />}
      </span>
      <span className="duration">{fmtDuration(flow.durationMs)}</span>
      <span className="timestamp">{fmtTime(flow.startedAt)}</span>
    </div>
  );
}
