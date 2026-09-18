/**
 * Pure helpers for the OAuth consent page (`/authorize?request=…`).
 *
 * pay-cloud's `/oauth/authorize` validates an MCP host's request, parks it,
 * and sends the browser here. The page shows who is asking, and Approve or
 * Deny posts the decision back; the server answers with where to send the
 * browser next (the host's redirect URI with a code, or an error).
 */

/** What `GET /api/oauth/authorize/{request}` returns. */
export interface PendingView {
  client_name: string;
  redirect_host: string;
  scope: string;
  /** This browser already owns a wallet; Approve can be used directly. */
  has_wallet: boolean;
  wallet_address?: string;
  /** Wallet providers that can create one (`openfort`). */
  providers: string[];
  /** Privy login offered by this server: sign in, then approve with the token. */
  privy?: PrivyLogin;
}

export interface PrivyLogin {
  app_id: string;
  /** pay's key quorum id, the signer a user wallet must list. */
  signer_id: string;
  policy_id?: string;
}

/** A pay-cloud JSON error, with its machine-readable code and extras. */
export class ApiRequestError extends Error {
  constructor(
    message: string,
    public readonly code: string | undefined,
    public readonly status: number,
    public readonly details: Record<string, unknown> | undefined,
  ) {
    super(message);
    this.name = "ApiRequestError";
  }
}

/** What the page must add to the user's wallet before approving again. */
export interface SignerRequired {
  address: string;
  signerId: string;
  policyIds: string[];
}

/**
 * Approve answered `signer_required`: the user's Privy wallet predates pay
 * and does not list pay's key as a signer. Returns what `addSigners` needs.
 */
export function signerRequired(err: unknown): SignerRequired | null {
  if (!(err instanceof ApiRequestError) || err.code !== "signer_required") return null;
  const d = err.details ?? {};
  const address = typeof d.address === "string" ? d.address : "";
  const signerId = typeof d.signer_id === "string" ? d.signer_id : "";
  if (!address || !signerId) return null;
  const policyId = typeof d.policy_id === "string" ? d.policy_id : null;
  return { address, signerId, policyIds: policyId ? [policyId] : [] };
}

/** Headers for Approve when the user signed in with Privy. */
export function approveHeaders(privyToken?: string | null): Record<string, string> {
  return privyToken ? { authorization: `Bearer ${privyToken}` } : {};
}

/** Body of `POST /api/onboard/start` from the consent page. */
export interface ConnectorStartRequest {
  provider: string;
  authorization_request: string;
}

/** Human names for provider ids. */
export function providerName(id: string): string {
  return id === "openfort" ? "Openfort" : id;
}

export function buildConnectorStartRequest(
  requestId: string,
  provider: string,
): ConnectorStartRequest {
  if (!requestId) throw new Error("missing authorization request");
  if (!provider) throw new Error("missing provider");
  return { provider, authorization_request: requestId };
}

/** What Approve and Deny return: where to go, or a wallet to fund first. */
export interface Decision {
  redirect?: string;
  /** The wallet was just created and is empty; fund it, then approve again. */
  fund?: { address: string; request: string };
}

/** The funding page for a just-created wallet, continuing to the host afterwards. */
export function fundContinuationUrl(
  fund: { address: string; request: string },
  clientName: string,
): string {
  const q = new URLSearchParams({ address: fund.address, request: fund.request });
  if (clientName) q.set("client", clientName);
  return `/fund?${q.toString()}`;
}

/** Where a decision sends the browser. Throws when the server sent neither field. */
export function decisionTarget(decision: Decision, clientName: string): string {
  if (decision.redirect) return decision.redirect;
  if (decision.fund) return fundContinuationUrl(decision.fund, clientName);
  throw new Error("The server did not say where to go next.");
}

/** `/authorize` or `/authorize/` and nothing else. */
export function isAuthorizePath(pathname: string): boolean {
  return /^\/authorize\/?$/.test(pathname);
}

/** The pending request id from `?request=…`, or null. */
export function parseAuthorizeRequest(search: string): string | null {
  const v = new URLSearchParams(search).get("request");
  return v && /^[A-Za-z0-9_-]{16,128}$/.test(v) ? v : null;
}

/** Human wording for a scope. */
export function describeScope(scope: string): string {
  return scope
    .split(/\s+/)
    .filter(Boolean)
    .map((s) =>
      s === "mcp"
        ? "use the pay tools and pay for API calls from your account"
        : `use the ${s} scope`,
    )
    .join("; ");
}
