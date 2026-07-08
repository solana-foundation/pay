import type { ReactNode } from "react";
import type { PaymentFlow, ProviderSummary } from "../types";
import { formatTokPerSec } from "../lib/inference";
import { ModelBadge } from "./ModelBadge";

interface Props {
  flow: PaymentFlow;
  providers?: ProviderSummary[];
}

function Row({ label, value }: { label: string; value: ReactNode }) {
  return (
    <div className="inference-row">
      <span className="inference-label">{label}</span>
      <span className="inference-value">{value}</span>
    </div>
  );
}

export function InferencePanel({ flow, providers }: Props) {
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
      {/* Model is the headline; provider is the muted secondary row. */}
      <Row
        label="Model"
        value={
          inf.model ? (
            <ModelBadge
              model={inf.model}
              provider={inf.provider}
              providers={providers}
            />
          ) : (
            "—"
          )
        }
      />
      <Row
        label="Provider"
        value={<span className="inference-muted">{inf.provider}</span>}
      />
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
