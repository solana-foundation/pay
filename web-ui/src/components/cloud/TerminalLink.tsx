import { PROVIDERS, type OnboardParams } from "../../cloud/lib/onboard";
import { PayWordmark } from "./PayWordmark";

interface Props {
  params: OnboardParams;
  /** Provider id whose consent hand-off is in flight, if any. */
  busy: string | null;
  error: string | null;
  onContinue: (provider: string) => void;
}

/**
 * The onboarding screen shown when the page was opened by `pay setup`: a
 * full-screen terminal with the pay.sh wordmark, the command that opened
 * it, and one action per wallet provider. Choosing a provider sends the
 * browser to that provider's own sign-in; pay keeps no account and no keys.
 */
export function TerminalLink({ params, busy, error, onContinue }: Props) {
  const who = [params.account, params.host].filter(Boolean).join("@");

  return (
    <section className="cloud-term" aria-label="pay setup">
      <div className="cloud-term-banner">
        <PayWordmark />
        <div className="cloud-term-tagline">Toolchain for agentic payments</div>
      </div>

      <div className="cloud-term-lines">
        <div className="cloud-term-line">
          <span className="cloud-term-prompt">$</span> pay setup --backend cloud
        </div>
        <div className="cloud-term-line cloud-term-line--muted">
          Linking {who || "this terminal"}
          {params.cli ? ` · pay ${params.cli}` : ""}
        </div>
      </div>

      <div className="cloud-term-lines">
        <div className="cloud-term-line">
          <span className="cloud-term-prompt">›</span> Choose where your wallet lives. You sign in
          with the provider; pay keeps no account and no keys.
        </div>
        <div className="cloud-term-actions">
          {PROVIDERS.map((p) => (
            <button
              key={p.id}
              type="button"
              className="cloud-term-button"
              disabled={busy !== null}
              aria-disabled={busy !== null}
              onClick={() => onContinue(p.id)}
            >
              {busy === p.id ? `Opening ${p.name}…` : `Continue with ${p.name}`}
            </button>
          ))}
          <span className="cloud-term-hint">Your terminal is waiting for this page.</span>
        </div>
        {error && (
          <div className="cloud-term-line cloud-term-line--error" role="alert">
            error: {error}
          </div>
        )}
      </div>
    </section>
  );
}
