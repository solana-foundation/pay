import type { PaymentFlow } from "../types";
import { useConfig, receiptUrl } from "../hooks/useConfig";
import { parseReceipt, receiptSignature } from "../utils/receipt";

/** Link to the flow's settlement transaction on pay.sh. Renders nothing
 *  when the flow has no receipt signature. Works for every payment pattern
 *  (per-call charge, x402, session, subscription). */
export function hasReceiptLink(flow: PaymentFlow): boolean {
  return !!receiptSignature(parseReceipt(flow));
}

export function ReceiptLink({
  flow,
  label = "View receipt",
}: {
  flow: PaymentFlow;
  label?: string;
}) {
  const config = useConfig();
  const href = receiptUrl(receiptSignature(parseReceipt(flow)), config);
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
