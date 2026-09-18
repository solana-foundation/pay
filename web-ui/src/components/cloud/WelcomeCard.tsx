import { useState, type FormEvent, type ReactNode } from "react";
import { EmailField } from "./EmailField";
import { isValidEmail } from "../../cloud/lib/onboard";

interface Props {
  /** False when the CLI link params are missing; keeps Continue disabled. */
  canContinue: boolean;
  /** True while the start request is in flight. */
  submitting: boolean;
  /** Error message shown under the button, if any. */
  error: string | null;
  /** Inline note shown under the button (e.g. "Open this page from pay setup…"). */
  note?: ReactNode;
  onContinue: (email: string) => void;
}

export function WelcomeCard({ canContinue, submitting, error, note, onContinue }: Props) {
  const [email, setEmail] = useState("");
  const valid = isValidEmail(email);
  const enabled = valid && canContinue && !submitting;

  function handleSubmit(e: FormEvent) {
    e.preventDefault();
    if (enabled) onContinue(email);
  }

  return (
    <form className="cloud-card" onSubmit={handleSubmit} noValidate>
      <h1 className="cloud-title">Welcome to Pay</h1>
      <p className="cloud-subtitle">Log in or sign up to get started.</p>
      <EmailField value={email} onChange={setEmail} disabled={submitting} />
      <button
        type="submit"
        className="cloud-continue"
        disabled={!enabled}
        aria-disabled={!enabled}
      >
        {submitting ? "Continuing…" : "Continue"}
      </button>
      {error && (
        <p className="cloud-error" role="alert">
          {error}
        </p>
      )}
      {note && <p className="cloud-note">{note}</p>}
    </form>
  );
}
