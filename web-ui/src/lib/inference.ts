import type { FlowStep, PaymentFlow, ProviderSummary } from "../types";

/**
 * Extract the port from a provider base URL, e.g.
 * "http://127.0.0.1:11434" → "11434". Falls back to the protocol default
 * (80/443) and returns null for unparseable input.
 */
export function portFromBaseUrl(baseUrl: string): string | null {
  try {
    const url = new URL(baseUrl);
    if (url.port) return url.port;
    if (url.protocol === "https:") return "443";
    if (url.protocol === "http:") return "80";
    return null;
  } catch {
    return null;
  }
}

/** Format a throughput value as "41.2 tok/s" (1 decimal); null when absent. */
export function formatTokPerSec(value: number | undefined): string | null {
  if (value == null || !Number.isFinite(value)) return null;
  return `${value.toFixed(1)} tok/s`;
}

/** Brand color for a provider slug, from the live provider list. */
export function providerColor(
  slug: string,
  providers: ProviderSummary[] | undefined,
): string | undefined {
  return providers?.find((p) => p.slug === slug)?.color;
}

/** Only apply brand colors that are safe hex values (per the wire contract). */
export function isHexColor(color: string | undefined): color is string {
  return !!color && /^#[0-9a-fA-F]{6}$/.test(color);
}

/**
 * True when a flow carries payment data — such flows keep the full
 * 402 sequence diagram even in inference mode.
 */
export function hasPaymentData(flow: PaymentFlow): boolean {
  return Boolean(
    flow.amount ||
      flow.session ||
      flow.challengeHeaders ||
      flow.paymentHeaders ||
      flow.steps.some((s) => s.key === "challenge" || s.key === "payment"),
  );
}

/**
 * Simplified sequence diagram steps for un-metered inference flows:
 * request → first token → completed. Derived from `status` + `ttftMs`.
 */
export function inferenceSteps(flow: PaymentFlow): FlowStep[] {
  const done = flow.status === "resource-delivered";
  const failed = flow.status === "failed";
  const ttftMs = flow.inference?.ttftMs;
  const hasFirstToken = ttftMs != null;

  let firstTokenTs: string | null = null;
  if (hasFirstToken) {
    const started = new Date(flow.startedAt).getTime();
    firstTokenTs = Number.isFinite(started)
      ? new Date(started + ttftMs).toISOString()
      : null;
  }

  const firstToken: FlowStep = {
    key: "first-token",
    label: "First Token",
    status:
      hasFirstToken || done ? "completed" : failed ? "pending" : "in-progress",
    ts: firstTokenTs,
  };

  const completed: FlowStep = {
    key: "completed",
    label: "Completed",
    status: done
      ? "completed"
      : !failed && firstToken.status === "completed"
        ? "in-progress"
        : "pending",
    ts: done ? flow.updatedAt : null,
  };

  return [
    {
      key: "request",
      label: "Request Received",
      status: "completed",
      ts: flow.startedAt,
    },
    firstToken,
    completed,
  ];
}
