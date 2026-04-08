import type {
  PaymentFlow,
  FlowStep,
  FlowEvent,
  FlowStatus,
  Protocol,
  StepStatus,
  SSEMessage,
} from "./types.js";

// ── Internal log entry (mirrors the Express middleware capture) ──

export interface LogEntry {
  id: number;
  ts: string;
  method: string;
  path: string;
  status: number;
  ms: number;
  reqHeaders: Record<string, string>;
  resHeaders: Record<string, string>;
  resBody: string | null;
  clientIp: string;
}

// ── Constants ──

const FLOW_TIMEOUT_MS = 60_000; // Mark stale flows as failed after 60s
const FACILITATOR_WINDOW_MS = 5_000; // Correlate facilitator calls within 5s
const MAX_FLOWS = 200;

// ── Correlation Engine ──

export class FlowCorrelation {
  private flows: PaymentFlow[] = [];
  private flowIndex = new Map<string, PaymentFlow>(); // key → flow
  private flowIdCounter = 0;
  private listeners = new Set<(msg: SSEMessage) => void>();

  /** Subscribe to flow events. Returns unsubscribe function. */
  subscribe(fn: (msg: SSEMessage) => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  /** Get a snapshot of all active flows. */
  snapshot(): PaymentFlow[] {
    return this.flows;
  }

  /** Process a completed HTTP request and correlate it into a flow. */
  ingest(entry: LogEntry): void {
    // Skip non-payment paths
    if (this.isInternalPath(entry.path)) return;

    const detection = this.detect(entry);
    if (!detection) return;

    const { protocol, phase } = detection;

    if (phase === "challenge") {
      this.createFlow(entry, protocol);
    } else if (phase === "retry") {
      this.handleRetry(entry, protocol);
    } else if (phase === "facilitator") {
      this.handleFacilitator(entry);
    }
  }

  /** Run periodic cleanup of stale flows. */
  cleanup(): void {
    const now = Date.now();
    for (const flow of this.flows) {
      if (
        flow.status === "payment-required" &&
        now - new Date(flow.updatedAt).getTime() > FLOW_TIMEOUT_MS
      ) {
        flow.status = "failed";
        flow.updatedAt = new Date().toISOString();
        flow.durationMs = now - new Date(flow.startedAt).getTime();
        flow.events.push({
          ts: flow.updatedAt,
          message: "Flow timed out — no payment received within 60s",
        });
        updateSteps(flow);
        this.emit({ type: "flow-updated", flow });
      }
    }
  }

  // ── Detection ──

  private detect(
    entry: LogEntry,
  ): { protocol: Protocol; phase: "challenge" | "retry" | "facilitator" } | null {
    const { status, reqHeaders, resHeaders, path } = entry;

    // Facilitator internal calls (x402 middleware → facilitator)
    if (path.startsWith("/facilitator/")) {
      return { protocol: "x402", phase: "facilitator" };
    }

    // 402 challenges
    if (status === 402) {
      if (resHeaders["www-authenticate"]?.startsWith("Payment")) {
        return { protocol: "mpp", phase: "challenge" };
      }
      // x402: returns 402 JSON with x402Version in body, no special header
      if (
        path.startsWith("/x402/") ||
        resHeaders["x-payment-required"] ||
        this.isX402Body(entry.resBody)
      ) {
        return { protocol: "x402", phase: "challenge" };
      }
      return null;
    }

    // Payment retries (successful follow-ups)
    if (resHeaders["payment-receipt"]) {
      return { protocol: "mpp", phase: "retry" };
    }
    // x402: client sends X-PAYMENT header on retry
    if (reqHeaders["x-payment"] || reqHeaders["x-payment-response"]) {
      return { protocol: "x402", phase: "retry" };
    }

    return null;
  }

  // ── Flow creation ──

  private createFlow(entry: LogEntry, protocol: Protocol): void {
    const id = `flow-${++this.flowIdCounter}`;
    const now = entry.ts;

    const steps = buildSteps(protocol);
    // Mark first two steps as completed (request + challenge)
    steps[0].status = "completed";
    steps[0].ts = now;
    steps[1].status = "completed";
    steps[1].ts = now;
    // Payment step is now in-progress
    steps[2].status = "in-progress";

    const flow: PaymentFlow = {
      id,
      protocol,
      resource: entry.path,
      status: "payment-required",
      clientIp: entry.clientIp,
      startedAt: now,
      updatedAt: now,
      durationMs: 0,
      steps,
      events: [
        {
          ts: now,
          message: `${entry.method} ${entry.path}`,
          detail: `Client request received`,
        },
        {
          ts: now,
          message: `402 Payment Required`,
          detail:
            protocol === "mpp"
              ? `www-authenticate: ${truncate(resHeader(entry, "www-authenticate"), 120)}`
              : `x-payment-required: ${truncate(resHeader(entry, "x-payment-required"), 120)}`,
        },
      ],
      challengeHeaders: entry.resHeaders,
    };

    this.addFlow(flow);
    this.emit({ type: "flow-created", flow });
  }

  // ── Payment retry ──

  private handleRetry(entry: LogEntry, protocol: Protocol): void {
    // Try exact match (IP + path), then path-only fallback
    let flow = this.flowIndex.get(flowKey(entry.clientIp, entry.path));
    if (!flow || flow.status !== "payment-required") {
      // Path-only fallback: find most recent pending flow for this path
      flow = [...this.flows].reverse().find(
        (f) => f.resource === entry.path && f.status === "payment-required"
      ) ?? null;
    }

    if (!flow || flow.status !== "payment-required") {
      this.createStandaloneDelivery(entry, protocol);
      return;
    }

    const now = entry.ts;

    // Check timeout
    if (
      new Date(now).getTime() - new Date(flow.startedAt).getTime() >
      FLOW_TIMEOUT_MS
    ) {
      flow.status = "failed";
      flow.updatedAt = now;
      flow.events.push({ ts: now, message: "Flow timed out before retry" });
      updateSteps(flow);
      this.emit({ type: "flow-updated", flow });
      return;
    }

    // Update flow
    flow.paymentHeaders = entry.reqHeaders;
    flow.responseHeaders = entry.resHeaders;
    flow.responseBody = entry.resBody;
    flow.updatedAt = now;
    flow.durationMs =
      new Date(now).getTime() - new Date(flow.startedAt).getTime();

    if (entry.status >= 200 && entry.status < 300) {
      flow.status = "resource-delivered";
      flow.events.push({
        ts: now,
        message: `Payment accepted`,
        detail:
          protocol === "mpp"
            ? `payment-receipt: ${truncate(resHeader(entry, "payment-receipt"), 120)}`
            : `x-payment-response verified`,
      });
      flow.events.push({
        ts: now,
        message: `200 Resource Delivered`,
        detail: entry.resBody
          ? truncate(entry.resBody, 200)
          : undefined,
      });
    } else {
      flow.status = "failed";
      flow.events.push({
        ts: now,
        message: `Payment retry failed with ${entry.status}`,
        detail: entry.resBody ? truncate(entry.resBody, 200) : undefined,
      });
    }

    updateSteps(flow);
    this.emit({ type: "flow-updated", flow });
  }

  // ── Facilitator calls (x402 internal) ──

  private handleFacilitator(entry: LogEntry): void {
    // Find most recent x402 flow within the timing window
    const now = new Date(entry.ts).getTime();
    for (let i = this.flows.length - 1; i >= 0; i--) {
      const flow = this.flows[i];
      if (
        flow.protocol === "x402" &&
        (flow.status === "payment-required" ||
          flow.status === "payment-received") &&
        now - new Date(flow.updatedAt).getTime() < FACILITATOR_WINDOW_MS
      ) {
        const action = entry.path.split("/").pop(); // "verify" or "settle"
        flow.events.push({
          ts: entry.ts,
          message: `Facilitator ${action}: ${entry.status === 200 ? "success" : "failed"}`,
          detail: entry.resBody ? truncate(entry.resBody, 200) : undefined,
        });
        flow.updatedAt = entry.ts;
        this.emit({ type: "flow-updated", flow });
        return;
      }
    }
  }

  // ── Standalone delivery (no matching 402 found) ──

  private createStandaloneDelivery(
    entry: LogEntry,
    protocol: Protocol,
  ): void {
    const id = `flow-${++this.flowIdCounter}`;
    const now = entry.ts;

    const steps = buildSteps(protocol);
    for (const step of steps) {
      step.status = "completed";
      step.ts = now;
    }

    const flow: PaymentFlow = {
      id,
      protocol,
      resource: entry.path,
      status: "resource-delivered",
      clientIp: entry.clientIp,
      startedAt: now,
      updatedAt: now,
      durationMs: entry.ms,
      steps,
      events: [
        {
          ts: now,
          message: `${entry.method} ${entry.path} → ${entry.status}`,
          detail: "Payment flow completed (challenge not captured)",
        },
      ],
      responseHeaders: entry.resHeaders,
      responseBody: entry.resBody,
    };

    this.addFlow(flow);
    this.emit({ type: "flow-created", flow });
  }

  // ── Internal helpers ──

  private isX402Body(body: string | null): boolean {
    if (!body) return false;
    try {
      const parsed = JSON.parse(body);
      return "x402Version" in parsed;
    } catch {
      return false;
    }
  }

  private isInternalPath(path: string): boolean {
    return (
      path === "/" ||
      path === "/health" ||
      path.startsWith("/__402")
    );
  }

  private addFlow(flow: PaymentFlow): void {
    this.flows.push(flow);
    if (this.flows.length > MAX_FLOWS) {
      const removed = this.flows.shift()!;
      this.flowIndex.delete(flowKey(removed.clientIp, removed.resource));
    }
    this.flowIndex.set(flowKey(flow.clientIp, flow.resource), flow);
  }

  private emit(msg: SSEMessage): void {
    for (const fn of this.listeners) fn(msg);
  }
}

// ── Pure helpers ──

function flowKey(clientIp: string, path: string): string {
  return `${clientIp}::${path}`;
}

function buildSteps(protocol: Protocol): FlowStep[] {
  return [
    { key: "request", label: "Client Request", status: "pending", ts: null },
    {
      key: "challenge",
      label: "402 Payment Required",
      status: "pending",
      ts: null,
    },
    {
      key: "payment",
      label:
        protocol === "mpp"
          ? "Payment Retry"
          : "Payment Retry",
      status: "pending",
      ts: null,
    },
    {
      key: "delivery",
      label: "Resource Delivered",
      status: "pending",
      ts: null,
    },
  ];
}

function updateSteps(flow: PaymentFlow): void {
  const statusToSteps: Record<FlowStatus, number> = {
    "payment-required": 2, // request + challenge done
    "payment-received": 3, // + payment done
    "resource-delivered": 4, // all done
    failed: -1, // special
  };

  const completedCount = statusToSteps[flow.status];

  if (completedCount === -1) {
    // Failed: mark all pending as pending, in-progress as failed (keep completed)
    for (const step of flow.steps) {
      if (step.status === "in-progress") step.status = "pending";
    }
    return;
  }

  for (let i = 0; i < flow.steps.length; i++) {
    if (i < completedCount) {
      flow.steps[i].status = "completed";
      if (!flow.steps[i].ts) flow.steps[i].ts = flow.updatedAt;
    } else if (i === completedCount) {
      flow.steps[i].status = "in-progress";
    } else {
      flow.steps[i].status = "pending";
    }
  }
}

function resHeader(entry: LogEntry, key: string): string {
  return entry.resHeaders[key] || "";
}

function truncate(s: string, max: number): string {
  return s.length > max ? s.slice(0, max) + "…" : s;
}
