import type { PaymentFlow } from "../types";
import type { Config } from "../hooks/useConfig";
import { payReceiptNetwork, useConfig, receiptUrl } from "../hooks/useConfig";
import {
  parseReceipt,
  receiptSignature,
  responseHeader,
} from "../utils/receipt";

function directReceiptUrl(flow: PaymentFlow): string | null {
  const url = responseHeader(flow, "payment-receipt-url")?.trim();
  return url || null;
}

export function receiptLinkHref(
  flow: PaymentFlow,
  config: Config | null,
): string | null {
  const receipt = parseReceipt(flow);
  const signature = receiptSignature(receipt);
  const fallbackConfig =
    receipt?.network && payReceiptNetwork(receipt.network) !== null
      ? { network: receipt.network }
      : null;
  return (
    directReceiptUrl(flow) ??
    (config ? receiptUrl(signature, config) : null) ??
    receiptUrl(signature, fallbackConfig) ??
    receiptUrl(signature, null)
  );
}

/** Link to the flow's settlement transaction on pay.sh. Renders nothing
 *  when the flow has no receipt signature. Works for every payment pattern
 *  (per-call charge, x402, session, subscription). */
export function hasReceiptLink(flow: PaymentFlow): boolean {
  return !!receiptLinkHref(flow, null);
}

export function ReceiptLink({
  flow,
  label = "View receipt",
}: {
  flow: PaymentFlow;
  label?: string;
}) {
  const config = useConfig();
  const href = receiptLinkHref(flow, config);
  if (!href) return null;
  return (
    <div className="flow-receipt">
      <a
        className="flow-receipt-link"
        href={href}
        target="_blank"
        rel="noopener"
      >
        {label} ↗
      </a>
    </div>
  );
}
