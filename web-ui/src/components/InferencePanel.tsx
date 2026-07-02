import type { PaymentFlow } from "../types";
import { formatTokPerSec } from "../lib/inference";

interface Props {
  flow: PaymentFlow;
}

function Row({ label, value }: { label: string; value: string }) {
  return (
    <div className="inference-row">
      <span className="inference-label">{label}</span>
      <span className="inference-value">{value}</span>
    </div>
  );
}

export function InferencePanel({ flow }: Props) {
  const inf = flow.inference;
  if (!inf) return null;
  const live = flow.status === "in-progress";

  const tokens =
    inf.tokensPrompt == null && inf.tokensCompletion == null
      ? "—"
      : `${inf.tokensPrompt ?? "—"} prompt / ${inf.tokensCompletion ?? "—"} completion`;

  return (
    <div className="inference-panel">
      <h3>
        Inference
        {live && (
          <span className="inference-live">
            <span className="inference-live-dot" />
            live
          </span>
        )}
      </h3>
      <Row label="Provider" value={inf.provider} />
      <Row label="Model" value={inf.model ?? "—"} />
      <Row label="Endpoint" value={inf.endpointKind ?? "other"} />
      <Row label="Streamed" value={inf.streamed ? "yes" : "no"} />
      <Row
        label="Time to first token"
        value={inf.ttftMs != null ? `${inf.ttftMs}ms` : "—"}
      />
      <Row label="Tokens" value={tokens} />
      <Row label="Throughput" value={formatTokPerSec(inf.tokensPerSec) ?? "—"} />
    </div>
  );
}
