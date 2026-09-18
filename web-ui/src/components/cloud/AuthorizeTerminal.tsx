import { useEffect, useState } from "react";
import {
  decisionTarget,
  describeScope,
  providerName,
  type Decision,
  type PendingView,
} from "../../cloud/lib/authorize";
import { PayWordmark } from "./PayWordmark";
import { PrivyApprove } from "./PrivyApprove";

interface Props {
  /** Pending request id from the URL, or null when missing/malformed. */
  requestId: string | null;
  load: (requestId: string) => Promise<PendingView>;
  /** Approve for this browser's wallet, or for the Privy user `privyToken` names. */
  approve: (requestId: string, privyToken?: string) => Promise<Decision>;
  deny: (requestId: string) => Promise<Decision>;
  /** Create a wallet with a provider; resolves to the provider's consent URL. */
  createWallet: (requestId: string, provider: string) => Promise<string>;
  /** Forget this browser's wallet cookie, so the page asks for a sign-in. */
  signOut: () => Promise<void>;
}

type Choice = "approve" | "privy" | "deny" | `create:${string}`;

type Phase =
  | { kind: "loading" }
  | { kind: "ready"; view: PendingView }
  | { kind: "deciding"; view: PendingView; choice: Choice }
  | { kind: "redirecting"; choice: Choice }
  | { kind: "error"; message: string };

/**
 * The OAuth consent screen: an MCP host asked to use this pay account.
 * Approve sends the browser back to the host with a code; Deny sends it
 * back with an error. Nothing else happens on this page.
 */
export function AuthorizeTerminal({
  requestId,
  load,
  approve,
  deny,
  createWallet,
  signOut,
}: Props) {
  const [phase, setPhase] = useState<Phase>(
    requestId
      ? { kind: "loading" }
      : { kind: "error", message: "This link is missing its request. Start again from your MCP client." },
  );

  useEffect(() => {
    if (!requestId) return;
    let cancelled = false;
    load(requestId)
      .then((view) => !cancelled && setPhase({ kind: "ready", view }))
      .catch((err) => {
        if (cancelled) return;
        setPhase({
          kind: "error",
          message: err instanceof Error ? err.message : "Something went wrong.",
        });
      });
    return () => {
      cancelled = true;
    };
  }, [requestId, load]);

  /** Approve as a signed-in Privy user; server refusals propagate to the caller. */
  async function approveWithPrivy(view: PendingView, token: string) {
    if (!requestId) return;
    setPhase({ kind: "deciding", view, choice: "privy" });
    try {
      // A brand-new wallet goes to the funding page first; it approves
      // the request itself and then returns to the host.
      const next = decisionTarget(await approve(requestId, token), view.client_name);
      setPhase({ kind: "redirecting", choice: "privy" });
      window.location.assign(next);
    } catch (err) {
      setPhase({ kind: "ready", view });
      throw err;
    }
  }

  async function decide(view: PendingView, choice: Choice) {
    if (!requestId) return;
    setPhase({ kind: "deciding", view, choice });
    try {
      let next: string;
      if (choice === "approve") next = decisionTarget(await approve(requestId), view.client_name);
      else if (choice === "deny") next = decisionTarget(await deny(requestId), view.client_name);
      else next = await createWallet(requestId, choice.slice("create:".length));
      setPhase({ kind: "redirecting", choice });
      window.location.assign(next);
    } catch (err) {
      setPhase({
        kind: "error",
        message: err instanceof Error ? err.message : "Something went wrong.",
      });
    }
  }

  return (
    <section className="cloud-term" aria-label="Authorize an MCP client">
      <div className="cloud-term-banner">
        <PayWordmark />
        <div className="cloud-term-tagline">Toolchain for agentic payments</div>
      </div>

      <div className="cloud-term-lines">
        <div className="cloud-term-line">
          <span className="cloud-term-prompt">$</span> pay connect
        </div>

        {phase.kind === "loading" && (
          <div className="cloud-term-line cloud-term-line--muted">… loading the request</div>
        )}

        {(phase.kind === "ready" || phase.kind === "deciding") && (
          <>
            <div className="cloud-term-line">
              <span className="cloud-term-prompt">›</span>{" "}
              <strong>{phase.view.client_name}</strong> wants to connect to your pay account.
            </div>
            <div className="cloud-term-line cloud-term-line--muted">
              It will be able to {describeScope(phase.view.scope)}. Every paid call is checked
              against your limits, and you can revoke this access at any time.
            </div>
            {phase.view.has_wallet ? (
              <div className="cloud-term-line cloud-term-line--muted">
                Paying from your pay wallet {phase.view.wallet_address}.{" "}
                <button
                  type="button"
                  className="cloud-term-link"
                  disabled={phase.kind === "deciding"}
                  onClick={() => {
                    void signOut().then(() => window.location.reload());
                  }}
                >
                  not you? sign out
                </button>
              </div>
            ) : phase.view.privy ? (
              <PrivyApprove
                login={phase.view.privy}
                busy={phase.kind === "deciding"}
                onApprove={(token) => approveWithPrivy(phase.view, token)}
              />
            ) : (
              <div className="cloud-term-line">
                <span className="cloud-term-prompt">›</span> You need a pay wallet first. Sign
                in with a provider to create one; it is yours, pay keeps no keys.
              </div>
            )}
            {phase.view.redirect_host && (
              <div className="cloud-term-line cloud-term-line--muted">
                After you decide, you return to {phase.view.redirect_host}.
              </div>
            )}
            <div className="cloud-term-actions">
              {phase.view.has_wallet ? (
                <button
                  type="button"
                  className="cloud-term-button"
                  disabled={phase.kind === "deciding"}
                  onClick={() => decide(phase.view, "approve")}
                >
                  {phase.kind === "deciding" && phase.choice === "approve"
                    ? "Approving…"
                    : "Approve"}
                </button>
              ) : (
                (phase.view.privy ? [] : phase.view.providers).map((provider) => (
                  <button
                    key={provider}
                    type="button"
                    className="cloud-term-button"
                    disabled={phase.kind === "deciding"}
                    onClick={() => decide(phase.view, `create:${provider}`)}
                  >
                    {phase.kind === "deciding" && phase.choice === `create:${provider}`
                      ? `Opening ${providerName(provider)}…`
                      : `Create wallet with ${providerName(provider)}`}
                  </button>
                ))
              )}
              <button
                type="button"
                className="cloud-term-button cloud-term-button--ghost"
                disabled={phase.kind === "deciding"}
                onClick={() => decide(phase.view, "deny")}
              >
                {phase.kind === "deciding" && phase.choice === "deny" ? "Denying…" : "Deny"}
              </button>
            </div>
          </>
        )}

        {phase.kind === "redirecting" && (
          <div className="cloud-term-line cloud-term-line--muted">
            {phase.choice === "approve" || phase.choice === "privy"
              ? "✔ Approved. Returning to your MCP client…"
              : phase.choice === "deny"
                ? "✖ Denied. Returning to your MCP client…"
                : "… Opening your wallet provider"}
          </div>
        )}

        {phase.kind === "error" && (
          <div className="cloud-term-line cloud-term-line--error" role="alert">
            error: {phase.message}
          </div>
        )}
      </div>
    </section>
  );
}
