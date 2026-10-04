import type { PaymentFlow, ProviderSummary } from "../types";
import { SequenceDiagram } from "./SequenceDiagram";
import { FlowInspector, type InspectorVisual } from "./FlowInspector";
import { PaymentSplits } from "./PaymentSplits";
import { hasReceiptLink, ReceiptLink } from "./ReceiptLink";
import { SessionChannel } from "./SessionChannel";
import { InferencePanel } from "./InferencePanel";
import { hasPaymentData, inferenceSteps } from "../lib/inference";

interface Props {
  flow: PaymentFlow;
  // Live provider list (inference mode) — for model badge brand colors.
  providers?: ProviderSummary[];
}

export function FlowDetail({ flow, providers }: Props) {
  const success = flow.status === "resource-delivered";
  const receiptLink = success && hasReceiptLink(flow) ? <ReceiptLink flow={flow} /> : null;
  // Un-metered inference flows get a simplified request → first token →
  // completed diagram; anything carrying payment data keeps today's diagram.
  const simplified = flow.inference && !hasPaymentData(flow);
  const steps = simplified ? inferenceSteps(flow) : flow.steps;
  const visuals: InspectorVisual[] = [];
  if (flow.inference) {
    visuals.push({
      id: "inference",
      label: "Inference",
      content: <InferencePanel flow={flow} providers={providers} />,
    });
  }
  if (flow.session) {
    visuals.push({
      id: "channel",
      label: "Channel",
      content: <SessionChannel flow={flow} />,
    });
  } else if (hasPaymentData(flow)) {
    visuals.push({
      id: "splits",
      label: "Splits",
      content: <PaymentSplits flow={flow} success={success} />,
    });
  }
  return (
    <div className={`flow-detail${flow.session ? " has-session" : ""}`}>
      <SequenceDiagram
        steps={steps}
        failed={flow.status === "failed"}
        success={success}
        deliveredContent={receiptLink}
      />
      <FlowInspector flow={flow} visuals={visuals} />
    </div>
  );
}
