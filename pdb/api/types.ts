// ── Protocol & Status ──

export type Protocol = "mpp" | "x402" | "session";

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

// ── Session Channel ──

export type SessionState = "opening" | "open" | "settling" | "closed" | "failed";

export interface SessionSplit {
  recipient: string;
  bps: number;
  label?: string;
}

export interface SessionInfo {
  sessionId?: string;
  state: SessionState;
  action?: "open" | "voucher" | "commit" | "topUp" | "close";
  mode?: "push" | "pull";
  currency?: string;
  decimals?: number;
  cap?: string;
  minVoucherDelta?: string;
  deposit?: string;
  approvedAmount?: string;
  cumulative?: string;
  delta?: string;
  voucherCount?: number;
  authorizedSigner?: string;
  owner?: string;
  payer?: string;
  recipient?: string;
  splits?: SessionSplit[];
  deliveryId?: string;
  openedAt?: string | null;
  updatedAt?: string | null;
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
  session?: SessionInfo;
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
