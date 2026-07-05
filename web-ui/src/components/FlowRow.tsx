import type { PaymentFlow, ProviderSummary } from "../types";
import { Amount } from "./Amount";
import { ProtocolBadge } from "./ProtocolBadge";
import { StatusIndicator } from "./StatusIndicator";
import type { SessionInfo } from "../types";
import { formatUnits } from "../lib/format";
import { formatTokPerSec } from "../lib/inference";
import { ModelBadge } from "./ModelBadge";

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

function sessionRowAmount(session: SessionInfo | undefined): string | undefined {
  if (!session) return undefined;
  const decimals = session.decimals ?? 6;
  const currency = session.currency ?? "USDC";
  if (session.cumulative) {
    return `paid ${formatUnits(session.cumulative, decimals, currency)}`;
  }
  const cap = session.deposit ?? session.approvedAmount ?? session.cap;
  if (cap) return `cap ${formatUnits(cap, decimals, currency)}`;
  return undefined;
}

interface Props {
  flow: PaymentFlow;
  selected: boolean;
  onClick: () => void;
  // Live provider list (inference mode) — used for badge brand colors.
  providers?: ProviderSummary[];
}

export function FlowRow({ flow, selected, onClick, providers }: Props) {
  const channelOpen = flow.session?.state === "open";
  const sessionAmount = sessionRowAmount(flow.session);
  const tokPerSec = formatTokPerSec(flow.inference?.tokensPerSec);

  return (
    <div
      className={`flow-row${selected ? " selected" : ""}${channelOpen ? " channel-open" : ""}`}
      onClick={onClick}
    >
      {flow.inference ? (
        // Model is the primary (badged) element; provider is the dim label.
        <ModelBadge
          model={flow.inference.model}
          provider={flow.inference.provider}
          providers={providers}
        />
      ) : (
        <ProtocolBadge protocol={flow.protocol} scheme={flow.scheme} />
      )}
      <span className="resource">{flow.resource}</span>
      {flow.inference?.model && (
        <span className="provider-label" title={flow.inference.provider}>
          {flow.inference.provider}
        </span>
      )}
      {channelOpen && (
        <span className="session-inline session-inline-status">
          <span className="session-inline-dot" />
          open
        </span>
      )}
      {!channelOpen && <StatusIndicator status={flow.status} />}
      <span className="amount-slot">
        {flow.inference
          ? tokPerSec && <span className="toks">{tokPerSec}</span>
          : flow.session
            ? sessionAmount && <span className="session-row-amount">{sessionAmount}</span>
            : flow.amount && <Amount value={parseFloat(flow.amount)} />}
      </span>
      <span className="duration">{fmtDuration(flow.durationMs)}</span>
      <span className="timestamp">{fmtTime(flow.startedAt)}</span>
    </div>
  );
}
