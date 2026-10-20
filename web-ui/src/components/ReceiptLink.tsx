import type { PaymentFlow } from "../types";
import type { Config } from "../hooks/useConfig";
import { payReceiptNetwork, useConfig, receiptUrl } from "../hooks/useConfig";
import {
  isSubscriptionAccessReceipt,
  parseReceipt,
  receiptSignature,
  responseHeader,
} from "../utils/receipt";

/** Origin of every receipt link Pay itself generates. */
const RECEIPT_ORIGIN = "https://pay.sh";

/** The upstream's `payment-receipt-url`, when it is a pay.sh link. The
 *  header comes from whatever server answered, so anything else (a
 *  `javascript:` URL, another host) is ignored in favour of the link derived
 *  from the settlement signature. */
function directReceiptUrl(flow: PaymentFlow): string | null {
  const raw = responseHeader(flow, "payment-receipt-url")?.trim();
  if (!raw) return null;
  let url: URL;
  try {
    url = new URL(raw);
  } catch {
    return null;
  }
  if (url.origin !== RECEIPT_ORIGIN || url.username || url.password) {
    return null;
  }
  return url.href;
}

export function receiptLinkHref(
  flow: PaymentFlow,
  config: Config | null,
): string | null {
  const receipt = parseReceipt(flow);
  const signature = receiptSignature(
    receipt,
    !isSubscriptionAccessReceipt(flow, receipt),
  );
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
