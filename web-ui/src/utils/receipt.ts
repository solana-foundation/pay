import type { PaymentFlow } from "../types";

/** Decoded `payment-receipt` response header. Fields vary by pattern:
 *  per-call charges carry `settlementSignature`/`signature`, subscriptions
 *  carry `activationSignature` plus plan/period metadata. */
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
  planId?: string;
  periodIndex?: string;
  periodStartTs?: string;
  periodEndTs?: string;
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

/** Best settlement transaction signature for a receipt, across patterns.
 *  Per-call charges put the settlement signature in `reference`; that's the
 *  last fallback so subscriptions (whose `reference` is the subscriptionId,
 *  not a tx) still resolve to `activationSignature` first. */
export function receiptSignature(receipt: Receipt | null): string | null {
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
    receipt.reference ||
    null
  );
}
