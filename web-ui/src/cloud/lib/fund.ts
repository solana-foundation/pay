/**
 * Pure helpers for the pay-cloud funding page (`/fund`).
 *
 * The CLI (`pay topup`, `pay setup`) opens `/fund?address=…&callback=…&state=…`.
 * The page asks pay-cloud for a Coinflow checkout link fixed to that
 * address and amount, embeds it in an iframe, listens for the iframe's
 * completion message, and returns to the CLI's loopback listener with the
 * payment id (and the on-chain signature when pay-cloud already has it).
 */

/** Query parameters the CLI places on the funding URL. */
export interface FundParams {
  /** Solana address receiving the USDC. */
  address?: string;
  /** Loopback URL to return to, e.g. `http://127.0.0.1:53211/callback`. */
  callback?: string;
  /** Opaque CSRF token echoed back to the CLI. */
  state?: string;
  /** Account name (informational). */
  account?: string;
  /** CLI version (informational). */
  cli?: string;
  /** Preselected amount in cents, when the CLI passes one. */
  cents?: number;
  /**
   * Connector origin: the pending OAuth request to approve once the wallet
   * is funded (or funding is skipped). Set by the consent page, not the CLI.
   */
  request?: string;
  /** Connector origin: the host's name, for "Continue to Claude". */
  client?: string;
}

/** Amounts offered on the page, in USD cents. */
export const AMOUNT_PRESETS_CENTS = [1000, 2000, 5000] as const;
export const DEFAULT_CENTS = 2000;

/** JSON body for `POST /api/fund/start`. */
export interface FundStartRequest {
  address: string;
  cents: number;
  callback?: string;
  state?: string;
}

/** Fee quote from `POST /api/fund/start`, in cents. */
export interface Quote {
  subtotal_cents: number;
  card_fee_cents: number;
  protection_fee_cents: number;
  other_fee_cents: number;
  total_cents: number;
}

/** Response of `POST /api/fund/start`. */
export interface FundStartResponse {
  link: string;
  checkout_origin: string;
  env: "sandbox" | "production";
  quote: Quote;
  settlement: "customer" | "merchant";
  payment_methods: string[];
  expires_in_minutes: number;
}

export type PaymentStatus = "pending" | "authorized" | "settled" | "disbursed" | "failed";

/** Response of `GET /api/fund/{payment_id}`. */
export interface FundStatusResponse {
  payment_id: string;
  status: PaymentStatus;
  signature?: string;
  wallet?: string;
}

/** Parse `window.location.search` into {@link FundParams}. Empty values are dropped. */
export function parseFundParams(search: string): FundParams {
  const qs = new URLSearchParams(search);
  const pick = (key: string): string | undefined => {
    const v = qs.get(key);
    return v && v.length > 0 ? v : undefined;
  };
  const rawCents = pick("cents");
  const cents = rawCents !== undefined ? Number.parseInt(rawCents, 10) : Number.NaN;
  return {
    address: pick("address"),
    callback: pick("callback"),
    state: pick("state"),
    account: pick("account"),
    cli: pick("cli"),
    cents: Number.isInteger(cents) && cents > 0 ? cents : undefined,
    request: pick("request"),
    client: pick("client"),
  };
}

/** The page was reached from the consent flow and must approve when done. */
export function isConnectorFunding(params: FundParams): boolean {
  return !!params.request && /^[A-Za-z0-9_-]{16,128}$/.test(params.request);
}

/** Where the connector flow goes after funding, for the page's wording. */
export function continueLabel(params: FundParams): string {
  return `Continue to ${params.client?.trim() || "your MCP client"}`;
}

/** `/fund` or `/fund/` and nothing else. */
export function isFundPath(pathname: string): boolean {
  return /^\/fund\/?$/.test(pathname);
}

/**
 * Build the start request. Throws when the address is missing so the caller
 * never posts a half-formed body.
 */
export function buildFundStartRequest(params: FundParams, cents: number): FundStartRequest {
  if (!params.address) {
    throw new Error("missing address");
  }
  const req: FundStartRequest = { address: params.address, cents };
  if (params.callback) req.callback = params.callback;
  if (params.state) req.state = params.state;
  return req;
}

/**
 * The URL the page sends the browser to once the purchase completes:
 * the CLI callback with `state`, `payment_id` and, when known, `signature`
 * appended. Existing query parameters on the callback are kept.
 */
export function buildReturnUrl(
  callback: string,
  state: string | undefined,
  paymentId: string,
  signature?: string,
): string {
  const url = new URL(callback);
  if (state) url.searchParams.set("state", state);
  url.searchParams.set("payment_id", paymentId);
  if (signature) url.searchParams.set("signature", signature);
  return url.toString();
}

/** What the Coinflow iframe told the page. */
export type CoinflowMessage =
  | { kind: "height"; px: number }
  | { kind: "success"; paymentId?: string }
  | { kind: "error"; message?: string };

/**
 * Interpret a `message` event from the Coinflow iframe. Messages from any
 * other origin are dropped. Coinflow posts JSON strings (sometimes objects):
 * `{ method: "heightChange", data: <px> }` while the form resizes, and
 * `{ data: "success", info: { paymentId } }` when the purchase completes.
 */
export function parseCoinflowMessage(
  raw: unknown,
  origin: string,
  expectedOrigin: string,
): CoinflowMessage | null {
  if (origin !== expectedOrigin) return null;
  let msg: unknown = raw;
  if (typeof raw === "string") {
    try {
      msg = JSON.parse(raw);
    } catch {
      return null;
    }
  }
  if (typeof msg !== "object" || msg === null) return null;
  const m = msg as Record<string, unknown>;
  if (m.method === "heightChange") {
    const px = typeof m.data === "number" ? m.data : Number.parseInt(String(m.data), 10);
    return Number.isFinite(px) && px > 0 ? { kind: "height", px } : null;
  }
  if (m.data === "success") {
    const info = (m.info ?? {}) as Record<string, unknown>;
    const paymentId = typeof info.paymentId === "string" ? info.paymentId : undefined;
    return { kind: "success", paymentId };
  }
  if (m.data === "error" || m.method === "error") {
    const info = (m.info ?? {}) as Record<string, unknown>;
    const message =
      typeof info.message === "string"
        ? info.message
        : typeof m.message === "string"
          ? m.message
          : undefined;
    return { kind: "error", message };
  }
  return null;
}

/** `2170` → `$21.70`. */
export function formatUsd(cents: number): string {
  const sign = cents < 0 ? "-" : "";
  const abs = Math.abs(cents);
  const dollars = Math.floor(abs / 100);
  const rest = String(abs % 100).padStart(2, "0");
  return `${sign}$${dollars.toLocaleString("en-US")}.${rest}`;
}

/** `CcZFhGwF…KCy3Z` for display. */
export function shortAddress(address: string): string {
  return address.length <= 12 ? address : `${address.slice(0, 4)}…${address.slice(-4)}`;
}

/** Solana explorer link for a mainnet signature. */
export function explorerTxUrl(signature: string, env: "sandbox" | "production"): string {
  const cluster = env === "production" ? "" : "?cluster=devnet";
  return `https://explorer.solana.com/tx/${signature}${cluster}`;
}
