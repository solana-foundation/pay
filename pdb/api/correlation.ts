import type {
  PaymentFlow,
  FlowStep,
  FlowEvent,
  FlowStatus,
  Protocol,
  SessionInfo,
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
        return {
          protocol: isSessionChallenge(entry) ? "session" : "mpp",
          phase: "challenge",
        };
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
    if (isSessionAuthorization(reqHeaders["authorization"])) {
      return { protocol: "session", phase: "retry" };
    }
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

    const session = protocol === "session" ? sessionFromChallenge(entry) : undefined;

    const flow: PaymentFlow = {
      id,
      protocol,
      resource: entry.path,
      status: "payment-required",
      clientIp: entry.clientIp,
      startedAt: now,
      updatedAt: now,
      durationMs: 0,
      session,
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
            protocol === "mpp" || protocol === "session"
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
    const sessionUpdate =
      protocol === "session" ? sessionFromAuthorization(entry, flow.session) : undefined;
    flow.paymentHeaders = entry.reqHeaders;
    flow.responseHeaders = entry.resHeaders;
    flow.responseBody = entry.resBody;
    flow.updatedAt = now;
    flow.durationMs =
      new Date(now).getTime() - new Date(flow.startedAt).getTime();

    if (entry.status >= 200 && entry.status < 300) {
      flow.status = "resource-delivered";
      if (sessionUpdate) flow.session = sessionUpdate;
      flow.events.push({
        ts: now,
        message:
          protocol === "session"
            ? sessionAcceptedMessage(sessionUpdate)
            : `Payment accepted`,
        detail:
          protocol === "mpp"
            ? `payment-receipt: ${truncate(resHeader(entry, "payment-receipt"), 120)}`
            : protocol === "session"
              ? sessionEventDetail(sessionUpdate)
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
      if (sessionUpdate) flow.session = { ...sessionUpdate, state: "failed" };
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

    const session =
      protocol === "session" ? sessionFromAuthorization(entry, undefined) : undefined;

    const flow: PaymentFlow = {
      id,
      protocol,
      resource: entry.path,
      status: "resource-delivered",
      clientIp: entry.clientIp,
      startedAt: now,
      updatedAt: now,
      durationMs: entry.ms,
      session,
      steps,
      events: [
        {
          ts: now,
          message:
            protocol === "session"
              ? sessionAcceptedMessage(session)
              : `${entry.method} ${entry.path} → ${entry.status}`,
          detail:
            protocol === "session"
              ? sessionEventDetail(session)
              : "Payment flow completed (challenge not captured)",
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
  const challengeLabel =
    protocol === "session" ? "402 Session Intent" : "402 Payment Required";
  const paymentLabel =
    protocol === "session" ? "Open / Voucher" : "Payment Retry";

  return [
    { key: "request", label: "Client Request", status: "pending", ts: null },
    {
      key: "challenge",
      label: challengeLabel,
      status: "pending",
      ts: null,
    },
    {
      key: "payment",
      label: paymentLabel,
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

function isSessionChallenge(entry: LogEntry): boolean {
  return paymentChallengeFromHeader(resHeader(entry, "www-authenticate"))?.intent === "session";
}

function sessionFromChallenge(entry: LogEntry): SessionInfo | undefined {
  const challenge = paymentChallengeFromHeader(resHeader(entry, "www-authenticate"));
  if (challenge?.intent !== "session") return undefined;

  const request = decodeJson<Record<string, unknown>>(challenge.request);
  const modes = Array.isArray(request?.modes) ? request.modes : [];
  const mode = modes.find((m): m is "push" | "pull" => m === "push" || m === "pull");

  return {
    state: "opening",
    mode,
    currency: stringValue(request?.currency),
    decimals: numberValue(request?.decimals),
    cap: stringValue(request?.cap),
    minVoucherDelta: stringValue(request?.minVoucherDelta),
    recipient: stringValue(request?.recipient),
    splits: sessionSplits(request?.splits),
    updatedAt: entry.ts,
  };
}

function isSessionAuthorization(auth: string | undefined): boolean {
  const credential = paymentCredentialFromAuthorization(auth);
  return credential?.challenge?.intent === "session";
}

function sessionFromAuthorization(
  entry: LogEntry,
  previous: SessionInfo | undefined,
): SessionInfo | undefined {
  const credential = paymentCredentialFromAuthorization(entry.reqHeaders["authorization"]);
  if (credential?.challenge?.intent !== "session") return undefined;

  const payload = credential.payload;
  const action = sessionAction(payload?.action);
  const receipt = parseCommitReceipt(entry.resBody);
  const voucher = payload?.voucher as Record<string, unknown> | undefined;
  const voucherData = voucher?.data as Record<string, unknown> | undefined;
  const cumulative =
    receipt?.cumulative ??
    stringValue(voucherData?.cumulativeAmount) ??
    stringValue(voucherData?.cumulative) ??
    previous?.cumulative;
  const sessionId =
    receipt?.sessionId ??
    stringValue(payload?.channelId) ??
    stringValue(payload?.tokenAccount) ??
    stringValue(voucherData?.channelId) ??
    previous?.sessionId;
  const hasVoucher = Boolean(voucherData);
  const state =
    entry.status >= 200 && entry.status < 300
      ? action === "close"
        ? "closed"
        : "open"
      : "failed";

  return {
    ...previous,
    sessionId,
    state,
    action,
    mode:
      sessionMode(payload?.mode) ??
      previous?.mode,
    currency: receipt?.currency ?? previous?.currency,
    deposit: stringValue(payload?.deposit) ?? previous?.deposit,
    approvedAmount: stringValue(payload?.approvedAmount) ?? previous?.approvedAmount,
    cumulative,
    delta: receipt?.amount ?? previous?.delta,
    voucherCount: (previous?.voucherCount ?? 0) + (hasVoucher ? 1 : 0),
    authorizedSigner:
      stringValue(payload?.authorizedSigner) ?? previous?.authorizedSigner,
    owner: stringValue(payload?.owner) ?? previous?.owner,
    payer: stringValue(payload?.payer) ?? stringValue(credential.source) ?? previous?.payer,
    deliveryId: receipt?.deliveryId ?? stringValue(payload?.deliveryId) ?? previous?.deliveryId,
    openedAt: previous?.openedAt ?? (action === "open" ? entry.ts : null),
    updatedAt: entry.ts,
  };
}

function sessionAcceptedMessage(session: SessionInfo | undefined): string {
  switch (session?.action) {
    case "open":
      return "Session channel opened";
    case "voucher":
      return "Session voucher accepted";
    case "commit":
      return "Session delivery committed";
    case "topUp":
      return "Session channel topped up";
    case "close":
      return "Session channel closed";
    default:
      return "Session action accepted";
  }
}

function sessionEventDetail(session: SessionInfo | undefined): string | undefined {
  if (!session) return undefined;
  const parts = [
    session.sessionId ? `session=${shorten(session.sessionId)}` : undefined,
    session.mode ? `mode=${session.mode}` : undefined,
    session.cumulative ? `cumulative=${session.cumulative}` : undefined,
    session.delta ? `delta=${session.delta}` : undefined,
    session.deliveryId ? `delivery=${shorten(session.deliveryId)}` : undefined,
  ].filter(Boolean);
  return parts.length > 0 ? parts.join(" · ") : undefined;
}

function paymentChallengeFromHeader(header: string): Record<string, string> | undefined {
  const challenge = header
    .split(/\n(?=Payment\s+)/)
    .find((part) => part.startsWith("Payment") && part.includes("intent=\"session\""))
    ?? (header.startsWith("Payment") ? header : undefined);
  if (!challenge) return undefined;
  return parseHeaderParams(challenge.replace(/^Payment\s*/i, ""));
}

function paymentCredentialFromAuthorization(
  auth: string | undefined,
): { challenge?: Record<string, string>; payload?: Record<string, unknown>; source?: unknown } | undefined {
  if (!auth) return undefined;
  const token = auth.replace(/^Payment\s+/i, "").trim();
  if (!token || token === auth) return undefined;
  return decodeJson(token);
}

function parseHeaderParams(value: string): Record<string, string> {
  const params: Record<string, string> = {};
  const re = /([A-Za-z][A-Za-z0-9_-]*)=(?:"([^"]*)"|([^,\s]+))/g;
  let match: RegExpExecArray | null;
  while ((match = re.exec(value))) {
    params[match[1]] = match[2] ?? match[3] ?? "";
  }
  return params;
}

function decodeJson<T>(encoded: string | undefined): T | undefined {
  if (!encoded) return undefined;
  try {
    const normalized = encoded.replace(/-/g, "+").replace(/_/g, "/");
    const padded = normalized + "=".repeat((4 - (normalized.length % 4)) % 4);
    return JSON.parse(Buffer.from(padded, "base64").toString("utf8")) as T;
  } catch {
    return undefined;
  }
}

function parseCommitReceipt(body: string | null):
  | {
      amount?: string;
      cumulative?: string;
      currency?: string;
      deliveryId?: string;
      sessionId?: string;
    }
  | undefined {
  if (!body) return undefined;
  try {
    const parsed = JSON.parse(body) as Record<string, unknown>;
    if (!("sessionId" in parsed) && !("cumulative" in parsed)) return undefined;
    return {
      amount: stringValue(parsed.amount),
      cumulative: stringValue(parsed.cumulative),
      currency: stringValue(parsed.currency),
      deliveryId: stringValue(parsed.deliveryId),
      sessionId: stringValue(parsed.sessionId),
    };
  } catch {
    return undefined;
  }
}

function sessionSplits(value: unknown): SessionInfo["splits"] {
  if (!Array.isArray(value)) return undefined;
  return value
    .map((split): NonNullable<SessionInfo["splits"]>[number] | null => {
      if (!split || typeof split !== "object") return null;
      const obj = split as Record<string, unknown>;
      const recipient = stringValue(obj.recipient);
      const bps = numberValue(obj.bps);
      if (!recipient || bps === undefined) return null;
      const label = stringValue(obj.label);
      return label ? { recipient, bps, label } : { recipient, bps };
    })
    .filter((split): split is NonNullable<SessionInfo["splits"]>[number] => split !== null);
}

function sessionAction(value: unknown): SessionInfo["action"] | undefined {
  return value === "open" ||
    value === "voucher" ||
    value === "commit" ||
    value === "topUp" ||
    value === "close"
    ? value
    : undefined;
}

function sessionMode(value: unknown): "push" | "pull" | undefined {
  return value === "push" || value === "pull" ? value : undefined;
}

function stringValue(value: unknown): string | undefined {
  if (typeof value === "string") return value;
  if (typeof value === "number" && Number.isFinite(value)) return String(value);
  return undefined;
}

function numberValue(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

function shorten(value: string): string {
  return value.length > 16 ? `${value.slice(0, 6)}…${value.slice(-6)}` : value;
}
