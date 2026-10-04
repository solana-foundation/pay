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
      ? null
      : `${inf.tokensPrompt ?? "not reported"} input / ${inf.tokensCompletion ?? "not reported"} output`;
  const totalTokens =
    inf.tokensPrompt != null || inf.tokensCompletion != null
      ? (inf.tokensPrompt ?? 0) + (inf.tokensCompletion ?? 0)
      : null;

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
      {inf.model && (
        <Row
          label="Model"
          value={
            <ModelBadge
              model={inf.model}
              provider={inf.provider}
              providers={providers}
            />
          }
        />
      )}
      <Row
        label="Provider"
        value={<span className="inference-muted">{inf.provider}</span>}
      />
      <Row label="Endpoint" value={inf.endpointKind ?? "other"} />
      <Row label="Streamed" value={inf.streamed ? "yes" : "no"} />
      {inf.ttftMs != null && (
        <Row label="Time to first token" value={`${inf.ttftMs}ms`} />
      )}
      {tokens && <Row label="Tokens" value={tokens} />}
      {totalTokens != null && <Row label="Total tokens" value={totalTokens} />}
      {(inf.tokensCached != null || inf.tokensReasoning != null) && (
        <Row
          label="Token details"
          value={`${inf.tokensCached ?? "—"} cached / ${inf.tokensReasoning ?? "—"} reasoning`}
        />
      )}
      {inf.finishReason && <Row label="Finish reason" value={inf.finishReason} />}
      {inf.responseId && <Row label="Response ID" value={inf.responseId} />}
      {inf.tokensPerSec != null && (
        <Row label="Throughput" value={formatTokPerSec(inf.tokensPerSec)} />
      )}
    </div>
  );
}
