import type { PaymentFlow } from "../types";

/** Decoded `payment-receipt` response header. Fields vary by pattern:
 *  per-call charges carry `settlementSignature`/`signature`, subscriptions
 *  carry `activationSignature` plus plan/period metadata. */
export interface Receipt {
  status?: string;
  method?: string;
  signature?: string;
  txSignature?: string;
  settlementSignature?: string;
  activationSignature?: string;
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

/** Decode the flow's `payment-receipt` response header, or null. */
export function parseReceipt(flow: PaymentFlow): Receipt | null {
  const header = flow.responseHeaders?.["payment-receipt"];
  if (!header) return null;
  const decoded = base64urlDecode(header);
  if (!decoded) return null;
  try {
    return JSON.parse(decoded) as Receipt;
  } catch {
    return null;
  }
}

/** Best settlement transaction signature for a receipt, across patterns. */
export function receiptSignature(receipt: Receipt | null): string | null {
  if (!receipt) return null;
  return (
    receipt.settlementSignature ||
    receipt.signature ||
    receipt.txSignature ||
    receipt.activationSignature ||
    null
  );
}
