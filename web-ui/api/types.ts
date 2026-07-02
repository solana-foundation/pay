// ── Protocol & Status ──

// "http" = plain passthrough exchange (inference mode); no payment involved.
export type Protocol = "mpp" | "x402" | "session" | "http";

export type FlowStatus =
  | "in-progress" // Request forwarded upstream, response not yet complete
  | "payment-required" // 402 sent, awaiting retry
  | "payment-received" // Client retried with payment header
  | "resource-delivered" // 200 with receipt / after settle
  | "failed"; // Error or timeout

export type StepStatus = "completed" | "in-progress" | "pending";

// ── Flow Step (sequence diagram) ──

export interface FlowStep {
  key: string; // "request" | "challenge" | "payment" | "delivery"
  label: string; // Human-readable, e.g. "Initial Request"
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

// ── Inference (Pay Inference mode) ──

export interface ProviderSummary {
  slug: string; // "ollama"
  title: string; // "Ollama"
  baseUrl: string; // "http://127.0.0.1:11434"
  up: boolean;
  models: string[];
  version?: string;
  color?: string; // brand hex, e.g. "#22c55e"
}

export interface InferenceInfo {
  provider: string; // slug
  model?: string;
  endpointKind?: "chat" | "completion" | "embeddings" | "other";
  streamed: boolean;
  tokensPrompt?: number;
  tokensCompletion?: number;
  ttftMs?: number;
  tokensPerSec?: number;
}

// ── Payment Flow ──

export interface PaymentFlow {
  id: string; // "flow-1", "flow-2", …
  protocol: Protocol;
  // Sub-scheme within the protocol: "charge"/"session"/"subscription" (mpp),
  // "exact"/"upto"/"batch-settlement" (x402). Rendered as "PROTOCOL:SCHEME".
  scheme?: string;
  resource: string; // URL path, e.g. "/mpp/quote/GOOG"
  status: FlowStatus;
  clientIp: string;
  startedAt: string; // ISO
  updatedAt: string; // ISO
  durationMs: number;
  amount?: string;
  payer?: string;
  session?: SessionInfo;
  inference?: InferenceInfo;
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
  | { type: "flow-updated"; flow: PaymentFlow }
  | { type: "provider-status"; providers: ProviderSummary[] };
