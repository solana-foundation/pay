import { useEffect, useRef, useState } from "react";
import { decisionTarget, type Decision } from "../../cloud/lib/authorize";
import {
  AMOUNT_PRESETS_CENTS,
  DEFAULT_CENTS,
  buildFundStartRequest,
  buildReturnUrl,
  continueLabel,
  explorerTxUrl,
  formatUsd,
  isConnectorFunding,
  parseCoinflowMessage,
  type FundParams,
  type FundStartResponse,
  type FundStatusResponse,
} from "../../cloud/lib/fund";
import { PayWordmark } from "./PayWordmark";
import { TerminalProgress, type ProgressLine } from "./TerminalProgress";

interface Props {
  params: FundParams;
  /** `POST /api/fund/start`. */
  start: (body: ReturnType<typeof buildFundStartRequest>) => Promise<FundStartResponse>;
  /** `GET /api/fund/{payment_id}`. */
  status: (paymentId: string) => Promise<FundStatusResponse>;
  /**
   * Connector origin: approve the pending OAuth request (by cookie) once the
   * wallet is funded or funding is skipped; resolves to the host redirect.
   */
  approve?: (request: string) => Promise<Decision>;
}

/** How long the page waits for the on-chain signature before returning anyway. */
const SIGNATURE_WAIT_MS = 20_000;
const STATUS_POLL_MS = 2_000;
/** Iframe height before Coinflow reports its own. */
const INITIAL_FRAME_HEIGHT = 560;

type Step =
  | { kind: "choose" }
  | { kind: "starting"; cents: number }
  | { kind: "checkout"; cents: number; checkout: FundStartResponse }
  | { kind: "done"; cents: number; checkout: FundStartResponse; paymentId: string };

/**
 * The funding screen opened by `pay topup` / `pay setup`: pick an amount,
 * see the exact total, pay inside Coinflow's card form, then return to the
 * terminal with the payment id. Only the address is needed, so it serves
 * every backend the same way.
 */
export function FundTerminal({ params, start, status, approve }: Props) {
  const [cents, setCents] = useState<number>(params.cents ?? DEFAULT_CENTS);
  const [step, setStep] = useState<Step>({ kind: "choose" });
  const [error, setError] = useState<string | null>(null);
  const [leaving, setLeaving] = useState(false);
  const connector = isConnectorFunding(params) && !!approve;

  /** Connector origin: approve the pending request and go back to the host. */
  async function continueToClient() {
    if (!connector || !params.request || !approve) return;
    setError(null);
    setLeaving(true);
    try {
      window.location.assign(decisionTarget(await approve(params.request), params.client ?? ""));
    } catch (err) {
      setLeaving(false);
      setError(err instanceof Error ? err.message : "Could not return to your MCP client.");
    }
  }

  async function handleContinue() {
    if (!params.address) return;
    setError(null);
    setStep({ kind: "starting", cents });
    try {
      const checkout = await start(buildFundStartRequest(params, cents));
      setStep({ kind: "checkout", cents, checkout });
    } catch (err) {
      setError(err instanceof Error ? err.message : "Something went wrong.");
      setStep({ kind: "choose" });
    }
  }

  if (!params.address) {
    return (
      <Shell>
        <div className="cloud-term-line cloud-term-line--error" role="alert">
          error: this page needs an address. Open it from <code>pay topup</code>.
        </div>
      </Shell>
    );
  }

  if (step.kind === "done") {
    return (
      <Completion
        params={params}
        checkout={step.checkout}
        cents={step.cents}
        paymentId={step.paymentId}
        status={status}
        onContinue={connector ? continueToClient : undefined}
      />
    );
  }

  return (
    <Shell>
      {step.kind === "checkout" ? (
        <Checkout
          checkout={step.checkout}
          onSuccess={(paymentId) =>
            setStep({ kind: "done", cents: step.cents, checkout: step.checkout, paymentId })
          }
          onError={(message) => {
            setError(message);
            setStep({ kind: "choose" });
          }}
          onChangeAmount={() => setStep({ kind: "choose" })}
        />
      ) : (
        <div className="cloud-term-lines">
          {connector && (
            <div className="cloud-term-line cloud-term-line--muted">
              Your pay wallet is ready: {params.address}. It is empty, so add some USDC before
              {" "}
              {params.client ?? "your MCP client"} starts paying for calls.
            </div>
          )}
          <div className="cloud-term-line">
            <span className="cloud-term-prompt">›</span> How much USDC do you want to start with?
          </div>
          <div className="cloud-term-amounts" role="radiogroup" aria-label="Amount">
            {AMOUNT_PRESETS_CENTS.map((preset) => (
              <button
                key={preset}
                type="button"
                role="radio"
                aria-checked={preset === cents}
                className={
                  preset === cents
                    ? "cloud-term-amount cloud-term-amount--active"
                    : "cloud-term-amount"
                }
                disabled={step.kind === "starting"}
                onClick={() => setCents(preset)}
              >
                {formatUsd(preset)}
              </button>
            ))}
          </div>
          <div className="cloud-term-line cloud-term-line--muted">
            Card, Apple Pay or Google Pay. Fees are shown before you pay.
          </div>
          <div className="cloud-term-actions">
            <button
              type="button"
              className="cloud-term-button"
              disabled={step.kind === "starting"}
              onClick={handleContinue}
            >
              {step.kind === "starting" ? "Preparing checkout…" : "Continue"}
            </button>
            {connector ? (
              <button
                type="button"
                className="cloud-term-button cloud-term-button--ghost"
                disabled={step.kind === "starting" || leaving}
                onClick={continueToClient}
              >
                {leaving ? "Returning…" : `Skip, ${continueLabel(params).toLowerCase()}`}
              </button>
            ) : (
              <span className="cloud-term-hint">
                {params.callback
                  ? "Your terminal is waiting for this page."
                  : "Return to your terminal when you are done."}
              </span>
            )}
          </div>
        </div>
      )}

      {error && (
        <div className="cloud-term-line cloud-term-line--error" role="alert">
          error: {error}
        </div>
      )}
    </Shell>
  );
}

function Shell({ children }: { children: React.ReactNode }) {
  return (
    <section className="cloud-term" aria-label="pay topup">
      <div className="cloud-term-banner">
        <PayWordmark />
        <div className="cloud-term-tagline">Toolchain for agentic payments</div>
      </div>
      {children}
    </section>
  );
}

/** The fee table and Coinflow's card form. */
function Checkout({
  checkout,
  onSuccess,
  onError,
  onChangeAmount,
}: {
  checkout: FundStartResponse;
  onSuccess: (paymentId: string) => void;
  onError: (message: string) => void;
  onChangeAmount: () => void;
}) {
  const [height, setHeight] = useState(INITIAL_FRAME_HEIGHT);
  const frame = useRef<HTMLIFrameElement>(null);

  useEffect(() => {
    function onMessage(event: MessageEvent) {
      // Only the embedded frame may speak; its origin is fixed by the server.
      if (event.source !== frame.current?.contentWindow) return;
      const msg = parseCoinflowMessage(event.data, event.origin, checkout.checkout_origin);
      if (!msg) return;
      if (msg.kind === "height") setHeight(Math.max(320, msg.px));
      if (msg.kind === "success") onSuccess(msg.paymentId ?? "");
      if (msg.kind === "error") onError(msg.message ?? "The payment did not go through.");
    }
    window.addEventListener("message", onMessage);
    return () => window.removeEventListener("message", onMessage);
  }, [checkout.checkout_origin, onSuccess, onError]);

  const q = checkout.quote;
  const fees = q.card_fee_cents + q.protection_fee_cents + q.other_fee_cents;

  return (
    <div className="cloud-term-lines">
      <table className="cloud-term-table" aria-label="Purchase summary">
        <tbody>
          <tr>
            <td>USDC to your wallet</td>
            <td>{formatUsd(q.subtotal_cents)}</td>
          </tr>
          <tr className="cloud-term-table-muted">
            <td>Card and protection fees</td>
            <td>{fees === 0 ? "covered by pay" : formatUsd(fees)}</td>
          </tr>
          <tr className="cloud-term-table-total">
            <td>Charged to your card</td>
            <td>{formatUsd(q.total_cents)}</td>
          </tr>
        </tbody>
      </table>
      <div className="cloud-term-line cloud-term-line--muted">
        <button type="button" className="cloud-term-link" onClick={onChangeAmount}>
          change amount
        </button>
        {checkout.settlement === "merchant" && (
          <>
            {" · "}
            <span className="cloud-term-warn">
              {checkout.env} mode: settles to pay's merchant wallet, not this address
            </span>
          </>
        )}
      </div>
      <iframe
        ref={frame}
        className="cloud-term-frame"
        title="Coinflow checkout"
        src={checkout.link}
        style={{ height }}
        allow="payment *; clipboard-write"
        sandbox="allow-scripts allow-same-origin allow-forms allow-popups allow-popups-to-escape-sandbox"
      />
    </div>
  );
}

/**
 * After Coinflow reports success: wait briefly for pay-cloud to hear the
 * on-chain signature from the webhook, then return to the CLI (or tell the
 * user to). The CLI keeps watching the balance either way.
 */
function Completion({
  params,
  checkout,
  cents,
  paymentId,
  status,
  onContinue,
}: {
  params: FundParams;
  checkout: FundStartResponse;
  cents: number;
  paymentId: string;
  status: (paymentId: string) => Promise<FundStatusResponse>;
  /** Connector origin: approve and return to the host once the USDC landed. */
  onContinue?: () => Promise<void>;
}) {
  const [lines, setLines] = useState<ProgressLine[]>([
    { text: `Charged ${formatUsd(checkout.quote.total_cents)}`, state: "done" },
    { text: `Sending ${formatUsd(cents).slice(1)} USDC to ${params.address ?? ""}`, state: "active" },
  ]);

  useEffect(() => {
    let cancelled = false;
    const startedAt = Date.now();

    function finish(signature?: string) {
      if (cancelled) return;
      const sent: ProgressLine = signature
        ? { text: `Confirmed: ${explorerTxUrl(signature, checkout.env)}`, state: "done" }
        : { text: "USDC is on its way; your terminal will confirm the transfer.", state: "done" };
      if (onContinue) {
        setLines((l) => [l[0], sent, { text: `${continueLabel(params)}…`, state: "active" }]);
        void onContinue();
      } else if (params.callback) {
        setLines((l) => [l[0], sent, { text: "Returning to your terminal", state: "active" }]);
        window.location.assign(buildReturnUrl(params.callback, params.state, paymentId, signature));
      } else {
        setLines((l) => [l[0], sent, { text: "Done. Return to your terminal.", state: "done" }]);
      }
    }

    async function poll() {
      while (!cancelled) {
        if (paymentId) {
          try {
            const res = await status(paymentId);
            if (res.status === "disbursed") return finish(res.signature);
            if (res.status === "failed") {
              setLines((l) => [
                l[0],
                { text: "Coinflow reported a failure. Nothing was sent.", state: "error" },
              ]);
              return;
            }
          } catch {
            // Status is best effort; keep waiting until the deadline.
          }
        }
        if (Date.now() - startedAt > SIGNATURE_WAIT_MS) return finish();
        await new Promise((r) => setTimeout(r, STATUS_POLL_MS));
      }
    }
    void poll();
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps -- onContinue is stable for the page's life
  }, [paymentId, params.callback, params.state, params.address, checkout.env, status]);

  return <TerminalProgress title={onContinue ? "pay connect" : "pay topup"} lines={lines} />;
}
