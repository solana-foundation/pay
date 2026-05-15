import { useMemo, useState } from "react";
import type { PaymentFlow } from "../types";
import {
  buildReplayCommand,
  replaySupportMessage,
} from "../utils/httpReplay";
import { PaymentSplits } from "./PaymentSplits";

interface Props {
  flow: PaymentFlow;
  success: boolean;
}

export function ReplayPanel({ flow, success }: Props) {
  const [copiedKind, setCopiedKind] = useState<string | null>(null);
  const curlCommand = useMemo(() => buildReplayCommand(flow, "curl"), [flow]);
  const httpieCommand = useMemo(() => buildReplayCommand(flow, "httpie"), [flow]);
  const payFetchCommand = useMemo(() => buildReplayCommand(flow, "pay-fetch"), [flow]);
  const payFetchNote = replaySupportMessage(flow);

  async function copyCommand(kind: string, command: string | null) {
    if (!command) return;
    await copyToClipboard(command);
    setCopiedKind(kind);
    window.setTimeout(() => setCopiedKind((current) => (current === kind ? null : current)), 1600);
  }

  return (
    <div className="replay-panel">
      <div className="replay-card">
        <div className="replay-header">
          <div>
            <h3>Replay</h3>
            <p>Re-run this request in your terminal with redacted payment headers.</p>
          </div>
        </div>
        <div className="replay-actions">
          <CopyButton
            label="Copy as curl"
            copied={copiedKind === "curl"}
            onClick={() => copyCommand("curl", curlCommand)}
          />
          <CopyButton
            label="Copy as HTTPie"
            copied={copiedKind === "httpie"}
            onClick={() => copyCommand("httpie", httpieCommand)}
          />
          <CopyButton
            label="Copy as pay fetch"
            copied={copiedKind === "pay-fetch"}
            disabled={!payFetchCommand}
            title={payFetchNote ?? undefined}
            onClick={() => copyCommand("pay-fetch", payFetchCommand)}
          />
        </div>
        {payFetchNote && <div className="replay-note">{payFetchNote}</div>}
      </div>
      <PaymentSplits flow={flow} success={success} />
    </div>
  );
}

function CopyButton({
  copied,
  disabled,
  label,
  title,
  onClick,
}: {
  copied: boolean;
  disabled?: boolean;
  label: string;
  title?: string;
  onClick: () => void;
}) {
  return (
    <button
      className={`replay-button${copied ? " copied" : ""}`}
      disabled={disabled}
      onClick={onClick}
      title={title}
      type="button"
    >
      {copied ? "Copied" : label}
    </button>
  );
}

async function copyToClipboard(text: string): Promise<void> {
  if (navigator.clipboard?.writeText) {
    await navigator.clipboard.writeText(text);
    return;
  }

  const textarea = document.createElement("textarea");
  textarea.value = text;
  textarea.setAttribute("readonly", "true");
  textarea.style.position = "absolute";
  textarea.style.left = "-9999px";
  document.body.appendChild(textarea);
  textarea.select();
  document.execCommand("copy");
  document.body.removeChild(textarea);
}
