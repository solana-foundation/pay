import type { PaymentFlow } from "../types";
import { SequenceDiagram } from "./SequenceDiagram";
import { EventLog } from "./EventLog";
import { PaymentSplits } from "./PaymentSplits";
import { SessionChannel } from "./SessionChannel";
import { useConfig, receiptUrl } from "../hooks/useConfig";

interface Props {
  flow: PaymentFlow;
}

function base64urlDecode(b64: string): string {
  try {
    const normalized = b64.replace(/-/g, "+").replace(/_/g, "/");
    const padded = normalized + "=".repeat((4 - (normalized.length % 4)) % 4);
    return atob(padded);
  } catch {
    return "";
  }
}

/**
 * Pull the settlement transaction signature out of the flow's
 * `payment-receipt` response header (base64url JSON). Subscriptions emit
 * `activationSignature`; per-call charges emit `settlementSignature` /
 * `signature`.
 */
function receiptSignature(flow: PaymentFlow): string | null {
  const header = flow.responseHeaders?.["payment-receipt"];
  if (!header) return null;
  const decoded = base64urlDecode(header);
  if (!decoded) return null;
  try {
    const r = JSON.parse(decoded);
    return (
      r.activationSignature ||
      r.settlementSignature ||
      r.signature ||
      r.txSignature ||
      null
    );
  } catch {
    return null;
  }
}

export function FlowDetail({ flow }: Props) {
  const success = flow.status === "resource-delivered";
  const config = useConfig();
  const receiptHref = receiptUrl(receiptSignature(flow), config);
  return (
    <div className={`flow-detail${flow.session ? " has-session" : ""}`}>
      <SequenceDiagram steps={flow.steps} failed={flow.status === "failed"} success={success} />
      <div className="flow-middle">
        {flow.session ? (
          <SessionChannel flow={flow} />
        ) : (
          <PaymentSplits flow={flow} success={success} />
        )}
        {receiptHref && (
          <div className="flow-receipt">
            <a
              className="flow-receipt-link"
              href={receiptHref}
              target="_blank"
              rel="noopener"
            >
              View receipt ↗
            </a>
          </div>
        )}
      </div>
      <EventLog events={flow.events} />
    </div>
  );
}
