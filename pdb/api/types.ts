// ── Protocol & Status ──

export type Protocol = "mpp" | "x402";

export type FlowStatus =
  | "payment-required" // 402 sent, awaiting retry
  | "payment-received" // Client retried with payment header
  | "resource-delivered" // 200 with receipt / after settle
  | "failed"; // Error or timeout

export type StepStatus = "completed" | "in-progress" | "pending";

// ── Flow Step (sequence diagram) ──

export interface FlowStep {
  key: string; // "request" | "challenge" | "payment" | "delivery"
  label: string; // Human-readable, e.g. "Client Request"
  status: StepStatus;
  ts: string | null; // ISO timestamp when completed
}

// ── Flow Event (log panel) ──

export interface FlowEvent {
  ts: string; // ISO timestamp
  message: string;
  detail?: string; // Extra context (header values, errors, etc.)
}

// ── Payment Flow ──

export interface PaymentFlow {
  id: string; // "flow-1", "flow-2", …
  protocol: Protocol;
  resource: string; // URL path, e.g. "/mpp/quote/GOOG"
  status: FlowStatus;
  clientIp: string;
  startedAt: string; // ISO
  updatedAt: string; // ISO
  durationMs: number;
  amount?: string;
  payer?: string;
  steps: FlowStep[];
  events: FlowEvent[];
  // Raw data for detail inspection
  challengeHeaders?: Record<string, string>;
  paymentHeaders?: Record<string, string>;
  responseHeaders?: Record<string, string>;
  responseBody?: string | null;
}

// ── SSE Messages ──

export type SSEMessage =
  | { type: "init"; viewerIp: string }
  | { type: "snapshot"; flows: PaymentFlow[] }
  | { type: "flow-created"; flow: PaymentFlow }
  | { type: "flow-updated"; flow: PaymentFlow };
