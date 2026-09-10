import type { PaymentFlow } from "../types";

/** Decoded `payment-receipt` response header. Fields vary by pattern. */
export interface Receipt {
  status?: string;
  method?: string;
  signature?: string;
  txSignature?: string;
  transaction?: string;
  transactionId?: string;
  settlementTransaction?: string;
  settlementSignature?: string;
  activationSignature?: string;
  network?: string;
  receipt?: {
    signature?: string;
    transaction?: string;
    transactionId?: string;
    settlementSignature?: string;
  };
  settlement?: {
    signature?: string;
    transaction?: string;
    transactionId?: string;
    settlementSignature?: string;
  };
  subscriptionId?: string;
  subscriptionDelegation?: string;
  periodIndex?: number;
  periodStart?: string;
  periodEnd?: string;
  expiresAt?: string;
  reference?: string;
  timestamp?: string;
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

export function responseHeader(flow: PaymentFlow, name: string): string | null {
  const headers = flow.responseHeaders;
  if (!headers) return null;
  const direct = headers[name];
  if (direct != null) return direct;
  const lower = name.toLowerCase();
  for (const [key, value] of Object.entries(headers)) {
    if (key.toLowerCase() === lower) return value;
  }
  return null;
}

/** Decode the flow's settlement response header, or null. */
export function parseReceipt(flow: PaymentFlow): Receipt | null {
  const header =
    responseHeader(flow, "payment-receipt") ||
    responseHeader(flow, "payment-response") ||
    responseHeader(flow, "x-payment-response");
  if (!header) return null;
  const decoded = base64urlDecode(header);
  for (const candidate of [decoded, header]) {
    if (!candidate) continue;
    try {
      return JSON.parse(candidate) as Receipt;
    } catch {
      // Continue: x402 exact may return a direct settlement reference.
    }
  }
  return { reference: header };
}

/** Whether this flow used a reusable subscription proof rather than activation. */
export function isSubscriptionAccessReceipt(
  flow: PaymentFlow,
  receipt: Receipt | null,
): boolean {
  const isSubscription = !!(
    receipt?.subscriptionId ||
    receipt?.subscriptionDelegation ||
    receipt?.periodEnd
  );
  if (!isSubscription) return false;

  const authorization = Object.entries(flow.paymentHeaders ?? {}).find(
    ([name]) => name.toLowerCase() === "authorization",
  )?.[1];
  const encoded = authorization?.replace(/^Payment\s+/i, "").trim();
  const decoded = encoded ? base64urlDecode(encoded) : "";
  if (!decoded) return false;
  try {
    const credential = JSON.parse(decoded) as {
      payload?: { type?: unknown };
    };
    return credential.payload?.type === "proof";
  } catch {
    return false;
  }
}

/** Best settlement transaction signature for a receipt, across patterns. */
export function receiptSignature(
  receipt: Receipt | null,
  includeReference = true,
): string | null {
  if (!receipt) return null;
  return (
    receipt.settlementSignature ||
    receipt.settlementTransaction ||
    receipt.signature ||
    receipt.txSignature ||
    receipt.transaction ||
    receipt.transactionId ||
    receipt.settlement?.settlementSignature ||
    receipt.settlement?.signature ||
    receipt.settlement?.transaction ||
    receipt.settlement?.transactionId ||
    receipt.receipt?.settlementSignature ||
    receipt.receipt?.signature ||
    receipt.receipt?.transaction ||
    receipt.receipt?.transactionId ||
    receipt.activationSignature ||
    (includeReference ? receipt.reference : null) ||
    null
  );
}
