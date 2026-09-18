import { useRef, useState } from "react";
import { PrivyProvider, useLogin, usePrivy, useSigners } from "@privy-io/react-auth";
import { signerRequired, type PrivyLogin } from "../../cloud/lib/authorize";
import { PayWordmark } from "./PayWordmark";

interface Props {
  login: PrivyLogin;
  /** Another decision is in flight; buttons are disabled. */
  busy: boolean;
  /**
   * Approve the pending request with a Privy access token. Resolves once
   * the browser is being sent back to the client; rejects with the server's
   * error (an `ApiRequestError`) otherwise.
   */
  onApprove: (token: string) => Promise<void>;
}

/**
 * Privy's config, built once: a new object per render would re-create the
 * client. The modal follows the terminal theme (pay.sh surface colour,
 * green accent, the wordmark as logo). "Protected by Privy" and the legal
 * links are dashboard settings (Configuration → UI components).
 */
const PRIVY_CONFIG = {
  // pay-cloud creates the Solana wallet server-side with pay's key as a
  // signer, so Privy must not race it with a signer-less one.
  embeddedWallets: {
    ethereum: { createOnLogin: "off" as const },
    solana: { createOnLogin: "off" as const },
  },
  appearance: {
    theme: "#0e0e0e" as const,
    accentColor: "#7fec86" as const,
    logo: <PayWordmark />,
    landingHeader: "Sign in to pay",
    loginMessage: "Your wallet is created for you. pay only signs within your limits.",
    walletChainType: "solana-only" as const,
    showWalletLoginFirst: false,
  },
};

/** A readable line for whatever Privy or pay-cloud threw. */
function describe(err: unknown, fallback: string): string {
  if (err instanceof Error && err.message) return err.message;
  if (typeof err === "string" && err) return err;
  if (err && typeof err === "object") {
    const o = err as { message?: unknown; error?: unknown; privyErrorCode?: unknown };
    for (const v of [o.message, o.error, o.privyErrorCode]) {
      if (typeof v === "string" && v) return v;
    }
  }
  return fallback;
}

/**
 * Privy sign-in on the consent page. Privy's modal handles email, passkey
 * and social login; pay-cloud only ever sees the resulting access token.
 * When the user's existing Privy wallet does not list pay as a signer, the
 * user adds it from here (that needs the wallet owner, so it cannot happen
 * on the server) and the approval is retried.
 */
export function PrivyApprove(props: Props) {
  return (
    <PrivyProvider appId={props.login.app_id} config={PRIVY_CONFIG}>
      <PrivyButtons {...props} />
    </PrivyProvider>
  );
}

function PrivyButtons({ busy, onApprove }: Props) {
  const { ready, authenticated, user, getAccessToken, logout } = usePrivy();
  const { addSigners } = useSigners();
  // Problems stay on this line and the sign-in stays mounted, so a wrong
  // or expired code is retried in Privy's modal rather than ending the page.
  const [notice, setNotice] = useState<string | null>(null);
  // Approve only after a click on this page. Privy keeps its session in
  // the browser and reports "complete" for it on mount, which must not
  // approve an MCP host the user has not looked at.
  const requested = useRef(false);
  const { login } = useLogin({
    onComplete: () => {
      if (requested.current) void approveNow();
    },
    onError: (err) => {
      console.error("privy login", err);
      setNotice(`Privy sign-in did not complete (${describe(err, "unknown error")}). Try again.`);
    },
  });

  async function approveNow() {
    setNotice(null);
    const token = await getAccessToken();
    if (!token) {
      setNotice("Privy did not return a session. Sign in again.");
      return;
    }
    try {
      await onApprove(token);
    } catch (err) {
      const needed = signerRequired(err);
      if (!needed) {
        setNotice(describe(err, "Something went wrong."));
        return;
      }
      try {
        await addSigners({
          address: needed.address,
          signers: [{ signerId: needed.signerId, policyIds: needed.policyIds }],
        });
        await onApprove(token);
      } catch (again) {
        setNotice(describe(again, "Could not add pay as a signer."));
      }
    }
  }

  if (!ready) {
    return <div className="cloud-term-line cloud-term-line--muted">… loading sign-in</div>;
  }

  const who = user?.email?.address ?? user?.google?.email ?? user?.id;
  return (
    <>
      {notice && (
        <div className="cloud-term-line cloud-term-line--error" role="alert">
          {notice}
        </div>
      )}
      {authenticated ? (
        <div className="cloud-term-line cloud-term-line--muted">Signed in with Privy as {who}.</div>
      ) : (
        <div className="cloud-term-line">
          <span className="cloud-term-prompt">›</span> Sign in with Privy to use your pay wallet.
          The key stays in Privy's enclave; pay signs only within your limits.
        </div>
      )}
      <div className="cloud-term-actions">
        {authenticated ? (
          <>
            <button
              type="button"
              className="cloud-term-button"
              disabled={busy}
              onClick={() => void approveNow()}
            >
              {busy ? "Approving…" : "Approve"}
            </button>
            <button
              type="button"
              className="cloud-term-button cloud-term-button--ghost"
              disabled={busy}
              onClick={() => void logout()}
            >
              Switch account
            </button>
          </>
        ) : (
          <button
            type="button"
            className="cloud-term-button"
            disabled={busy}
            onClick={() => {
              requested.current = true;
              login();
            }}
          >
            Sign in with Privy
          </button>
        )}
      </div>
    </>
  );
}
