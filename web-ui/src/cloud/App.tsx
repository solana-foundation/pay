import { useEffect, useMemo, useState } from "react";
import { AuthorizeTerminal } from "../components/cloud/AuthorizeTerminal";
import { FundTerminal } from "../components/cloud/FundTerminal";
import { TerminalLink } from "../components/cloud/TerminalLink";
import { TerminalProgress, type ProgressLine } from "../components/cloud/TerminalProgress";
import { WelcomeCard } from "../components/cloud/WelcomeCard";
import {
  ApiRequestError,
  approveHeaders,
  buildConnectorStartRequest,
  isAuthorizePath,
  parseAuthorizeRequest,
  type Decision,
  type PendingView,
} from "./lib/authorize";
import {
  isFundPath,
  parseFundParams,
  type FundStartResponse,
  type FundStatusResponse,
} from "./lib/fund";
import {
  buildProviderStartRequest,
  hasLinkParams,
  parseOnboardParams,
  providerCallbackFromPath,
} from "./lib/onboard";

/** Error body returned by pay-cloud on validation failure. */
interface ApiError {
  error?: string;
  message?: string;
  details?: Record<string, unknown>;
}

async function postJson<T>(
  path: string,
  body: unknown,
  headers: Record<string, string> = {},
): Promise<T> {
  const res = await fetch(path, {
    method: "POST",
    headers: { "content-type": "application/json", ...headers },
    body: JSON.stringify(body),
  });
  return unwrap<T>(res);
}

async function getJson<T>(path: string): Promise<T> {
  return unwrap<T>(await fetch(path, { headers: { accept: "application/json" } }));
}

async function unwrap<T>(res: Response): Promise<T> {
  const json = (await res.json().catch(() => ({}))) as ApiError & T;
  if (!res.ok) {
    throw new ApiRequestError(
      json.message ?? `Request failed (${res.status})`,
      json.error,
      res.status,
      json.details,
    );
  }
  return json;
}

export function App() {
  const params = useMemo(() => parseOnboardParams(window.location.search), []);
  const linked = hasLinkParams(params);
  const callbackProvider = useMemo(
    () => providerCallbackFromPath(window.location.pathname),
    [],
  );
  const funding = useMemo(() => isFundPath(window.location.pathname), []);
  const authorizing = useMemo(() => isAuthorizePath(window.location.pathname), []);
  const terminal = linked || callbackProvider !== null || funding || authorizing;

  useEffect(() => {
    document.documentElement.dataset.cloudTheme = terminal ? "terminal" : "light";
  }, [terminal]);

  if (authorizing) {
    const api = (id: string) => `/api/oauth/authorize/${encodeURIComponent(id)}`;
    return (
      <main className="cloud-page cloud-page--terminal">
        <AuthorizeTerminal
          requestId={parseAuthorizeRequest(window.location.search)}
          load={(id) => getJson<PendingView>(api(id))}
          approve={(id, token) => postJson<Decision>(`${api(id)}/approve`, {}, approveHeaders(token))}
          signOut={async () => {
            await fetch("/api/session/logout", { method: "POST" });
          }}
          deny={(id) => postJson<Decision>(`${api(id)}/deny`, {})}
          createWallet={async (id, provider) => {
            const res = await postJson<{ consent?: string }>(
              "/api/onboard/start",
              buildConnectorStartRequest(id, provider),
            );
            if (!res.consent) throw new Error("The server did not return a sign-in link.");
            return res.consent;
          }}
        />
      </main>
    );
  }

  if (funding) {
    return (
      <main className="cloud-page cloud-page--terminal">
        <FundTerminal
          params={parseFundParams(window.location.search)}
          start={(body) => postJson<FundStartResponse>("/api/fund/start", body)}
          status={(id) => getJson<FundStatusResponse>(`/api/fund/${encodeURIComponent(id)}`)}
          approve={(request) =>
            postJson<Decision>(`/api/oauth/authorize/${encodeURIComponent(request)}/approve`, {})
          }
        />
      </main>
    );
  }

  if (callbackProvider) {
    return (
      <main className="cloud-page cloud-page--terminal">
        <ProviderCallback provider={callbackProvider} />
      </main>
    );
  }

  if (linked) {
    return (
      <main className="cloud-page cloud-page--terminal">
        <LinkFlow params={params} />
      </main>
    );
  }

  return (
    <main className="cloud-page">
      <WelcomeCard
        canContinue={false}
        submitting={false}
        error={null}
        note={
          <>
            Open this page from <code>pay setup</code> to link your terminal.
          </>
        }
        onContinue={() => undefined}
      />
    </main>
  );
}

/** Step 1: the user picks a provider; we open a session and hand off. */
function LinkFlow({ params }: { params: ReturnType<typeof parseOnboardParams> }) {
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  async function handleContinue(provider: string) {
    if (!hasLinkParams(params)) return;
    setBusy(provider);
    setError(null);
    try {
      const res = await postJson<{ consent?: string }>(
        "/api/onboard/start",
        buildProviderStartRequest(params, provider),
      );
      if (!res.consent) throw new Error("The server did not return a sign-in link.");
      window.location.assign(res.consent);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Something went wrong.");
      setBusy(null);
    }
  }

  return <TerminalLink params={params} busy={busy} error={error} onContinue={handleContinue} />;
}

/**
 * Step 2: the provider redirected back with the grant in the URL fragment.
 * Post it to the server, which provisions the wallet, then follow the
 * redirect to the CLI. The fragment never travels in a URL we log.
 */
function ProviderCallback({ provider }: { provider: string }) {
  const [lines, setLines] = useState<ProgressLine[]>([
    { text: `Signed in with ${provider}`, state: "done" },
    { text: "Creating your wallet", state: "active" },
  ]);

  useEffect(() => {
    let cancelled = false;
    const fragment = window.location.hash;
    // Drop the secrets from the address bar and history right away.
    window.history.replaceState(null, "", window.location.pathname);

    (async () => {
      try {
        if (!fragment || fragment.length < 2) {
          throw new Error("The provider did not return a sign-in result. Run pay setup again.");
        }
        const res = await postJson<{ redirect: string; address: string; origin?: string }>(
          `/api/onboard/${provider}/complete`,
          { fragment },
        );
        if (cancelled) return;
        setLines([
          { text: `Signed in with ${provider}`, state: "done" },
          { text: `Wallet created: ${res.address}`, state: "done" },
          {
            text:
              res.origin === "connector"
                ? "Returning to your MCP client"
                : "Returning to your terminal",
            state: "active",
          },
        ]);
        window.location.assign(res.redirect);
      } catch (err) {
        if (cancelled) return;
        setLines([
          { text: `Signed in with ${provider}`, state: "done" },
          {
            text: err instanceof Error ? err.message : "Something went wrong.",
            state: "error",
          },
          { text: "Go back to where you started and try again.", state: "error" },
        ]);
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [provider]);

  return <TerminalProgress title={`pay setup --backend cloud`} lines={lines} />;
}
