/**
 * Pure helpers for the pay-cloud onboarding page.
 *
 * The CLI (`pay setup`) opens `/onboard?callback=…&state=…&code_challenge=…`
 * in the browser. This page collects an email, posts it together with the
 * CLI's PKCE parameters to `/api/onboard/start`, and follows the returned
 * redirect back to the CLI's loopback listener.
 */

/** Query parameters the CLI places on the onboarding URL. */
export interface OnboardParams {
  /** Loopback URL the server redirects to, e.g. `http://127.0.0.1:53211/callback`. */
  callback?: string;
  /** Opaque CSRF token echoed back to the CLI. */
  state?: string;
  /** PKCE S256 challenge (base64url, no padding). */
  code_challenge?: string;
  /** Account name the CLI is linking (informational). */
  account?: string;
  /** Machine hostname (informational, shown under the card). */
  host?: string;
  /** CLI version (informational). */
  cli?: string;
}

/** Link parameters that must all be present to complete onboarding. */
export type LinkParams = OnboardParams &
  Required<Pick<OnboardParams, "callback" | "state" | "code_challenge">>;

/** JSON body for `POST /api/onboard/start`. */
export interface StartRequest {
  /** Present on the email-only stub path. */
  email?: string;
  /** Wallet driver id (`openfort`); the response then carries `consent`. */
  provider?: string;
  callback: string;
  state: string;
  code_challenge: string;
  account?: string;
  host?: string;
  cli?: string;
}

/** Wallet providers the page can offer, in display order. */
export const PROVIDERS = [{ id: "openfort", name: "Openfort" }] as const;

/**
 * `/onboard/<provider>/callback` → `<provider>`, or null for any other path.
 * The provider's consent page redirects here with the grant in the fragment.
 */
export function providerCallbackFromPath(pathname: string): string | null {
  const m = /^\/onboard\/([a-z0-9-]+)\/callback\/?$/.exec(pathname);
  return m ? m[1] : null;
}

/** JSON body for `POST /api/onboard/start` when a provider is chosen. */
export function buildProviderStartRequest(params: LinkParams, provider: string): StartRequest {
  const req: StartRequest = {
    provider,
    callback: params.callback,
    state: params.state,
    code_challenge: params.code_challenge,
  };
  if (params.account) req.account = params.account;
  if (params.host) req.host = params.host;
  if (params.cli) req.cli = params.cli;
  return req;
}

/**
 * Loose email check: one `@`, non-empty local and domain parts, a dot in
 * the domain, no whitespace. The server re-validates; this only gates the
 * Continue button.
 */
export function isValidEmail(value: string): boolean {
  const s = value.trim();
  if (s.length === 0 || /\s/.test(s)) return false;
  const at = s.indexOf("@");
  if (at <= 0 || at !== s.lastIndexOf("@")) return false;
  const domain = s.slice(at + 1);
  if (domain.length === 0) return false;
  const dot = domain.lastIndexOf(".");
  return dot > 0 && dot < domain.length - 1;
}

/** Parse `window.location.search` into {@link OnboardParams}. Empty values are dropped. */
export function parseOnboardParams(search: string): OnboardParams {
  const qs = new URLSearchParams(search);
  const pick = (key: keyof OnboardParams): string | undefined => {
    const v = qs.get(key);
    return v && v.length > 0 ? v : undefined;
  };
  return {
    callback: pick("callback"),
    state: pick("state"),
    code_challenge: pick("code_challenge"),
    account: pick("account"),
    host: pick("host"),
    cli: pick("cli"),
  };
}

/** True when the CLI-provided parameters needed to complete linking are present. */
export function hasLinkParams(params: OnboardParams): params is LinkParams {
  return Boolean(params.callback && params.state && params.code_challenge);
}

/**
 * Build the start request. Throws when the link params are missing so the
 * caller never posts a half-formed body.
 */
export function buildStartRequest(params: OnboardParams, email: string): StartRequest {
  if (!hasLinkParams(params)) {
    throw new Error("missing callback, state, or code_challenge");
  }
  const req: StartRequest = {
    email: email.trim(),
    callback: params.callback,
    state: params.state,
    code_challenge: params.code_challenge,
  };
  if (params.account) req.account = params.account;
  if (params.host) req.host = params.host;
  if (params.cli) req.cli = params.cli;
  return req;
}
