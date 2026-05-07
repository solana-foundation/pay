import type { PaymentFlow } from "../types";

type ReplayKind = "curl" | "httpie" | "pay-fetch";

const REDACTED = "[REDACTED]";
const REDACTED_HEADERS = new Set([
  "authorization",
  "proxy-authorization",
  "www-authenticate",
  "payment-receipt",
  "x-payment",
  "x-payment-response",
  "x-payment-required",
  "cookie",
  "set-cookie",
]);

const OMITTED_REPLAY_HEADERS = new Set([
  "accept-encoding",
  "connection",
  "content-length",
  "host",
  "origin",
  "referer",
  "user-agent",
]);

export function redactHeaders(
  headers: Record<string, string> | undefined,
): Record<string, string> {
  if (!headers) return {};

  return Object.fromEntries(
    Object.entries(headers).map(([name, value]) => [
      name,
      REDACTED_HEADERS.has(name.toLowerCase()) ? REDACTED : value,
    ]),
  );
}

export function buildRawRequest(flow: PaymentFlow): string {
  const lines = [`${flow.requestMethod} ${flow.requestUrl} HTTP/1.1`];
  const headers = redactHeaders(flow.requestHeaders);

  for (const [name, value] of Object.entries(headers)) {
    lines.push(`${name}: ${value}`);
  }

  if (flow.requestBody) {
    lines.push("", prettyBody(flow.requestBody));
  }

  return lines.join("\n");
}

export function buildRawResponse(flow: PaymentFlow): string {
  const lines = [
    `HTTP/1.1 ${flow.responseStatus ?? 0}`,
  ];
  const headers = redactHeaders(flow.responseHeaders);

  for (const [name, value] of Object.entries(headers)) {
    lines.push(`${name}: ${value}`);
  }

  if (flow.responseBody) {
    lines.push("", prettyBody(flow.responseBody));
  }

  return lines.join("\n");
}

export function buildReplayCommand(
  flow: PaymentFlow,
  kind: ReplayKind,
): string | null {
  const headers = replayHeaders(flow.requestHeaders);
  const body = normalizedBody(flow.requestBody);

  switch (kind) {
    case "curl":
      return buildCurlCommand(flow, headers, body);
    case "httpie":
      return buildHttpieCommand(flow, headers, body);
    case "pay-fetch":
      if (flow.requestMethod !== "GET" || body) {
        return null;
      }
      return buildPayFetchCommand(flow, headers);
    default:
      return null;
  }
}

export function replaySupportMessage(flow: PaymentFlow): string | null {
  if (flow.requestMethod !== "GET" || normalizedBody(flow.requestBody)) {
    return "`pay fetch` replay currently supports GET requests without a body.";
  }

  return null;
}

function buildCurlCommand(
  flow: PaymentFlow,
  headers: Array<[string, string]>,
  body: string | null,
): string {
  const parts = ["curl", "-X", shellQuote(flow.requestMethod), shellQuote(flow.requestUrl)];

  for (const [name, value] of headers) {
    parts.push("-H", shellQuote(`${name}: ${value}`));
  }

  if (body) {
    parts.push("--data", shellQuote(compactBody(body)));
  }

  return joinCommand(parts);
}

function buildHttpieCommand(
  flow: PaymentFlow,
  headers: Array<[string, string]>,
  body: string | null,
): string {
  const parts = ["http", shellQuote(flow.requestMethod), shellQuote(flow.requestUrl)];

  for (const [name, value] of headers) {
    parts.push(shellQuote(`${name}:${value}`));
  }

  if (body) {
    parts.push("--raw", shellQuote(compactBody(body)));
  }

  return joinCommand(parts);
}

function buildPayFetchCommand(
  flow: PaymentFlow,
  headers: Array<[string, string]>,
): string {
  const parts = ["pay", "fetch", shellQuote(flow.requestUrl)];

  for (const [name, value] of headers) {
    parts.push("-H", shellQuote(`${name}: ${value}`));
  }

  return joinCommand(parts);
}

function joinCommand(parts: string[]): string {
  return parts.join(" ");
}

function replayHeaders(
  headers: Record<string, string> | undefined,
): Array<[string, string]> {
  return Object.entries(redactHeaders(headers))
    .filter(([name]) => !OMITTED_REPLAY_HEADERS.has(name.toLowerCase()))
    .sort(([a], [b]) => a.localeCompare(b));
}

function normalizedBody(body: string | null | undefined): string | null {
  if (!body) return null;
  const trimmed = body.trim();
  return trimmed.length > 0 ? trimmed : null;
}

function prettyBody(body: string): string {
  try {
    return JSON.stringify(JSON.parse(body), null, 2);
  } catch {
    return body;
  }
}

function compactBody(body: string): string {
  try {
    return JSON.stringify(JSON.parse(body));
  } catch {
    return body;
  }
}

function shellQuote(value: string): string {
  return `'${value.replace(/'/g, `'\\''`)}'`;
}
