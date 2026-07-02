import type { PaymentFlow } from "../types";
import { SequenceDiagram } from "./SequenceDiagram";
import { EventLog } from "./EventLog";
import { PaymentSplits } from "./PaymentSplits";
import { hasReceiptLink, ReceiptLink } from "./ReceiptLink";
import { SessionChannel } from "./SessionChannel";
import { InferencePanel } from "./InferencePanel";
import { hasPaymentData, inferenceSteps } from "../lib/inference";

interface Props {
  flow: PaymentFlow;
}

export function FlowDetail({ flow }: Props) {
  const success = flow.status === "resource-delivered";
  const receiptLink = success && hasReceiptLink(flow) ? <ReceiptLink flow={flow} /> : null;
  // Un-metered inference flows get a simplified request → first token →
  // completed diagram; anything carrying payment data keeps today's diagram.
  const simplified = flow.inference && !hasPaymentData(flow);
  const steps = simplified ? inferenceSteps(flow) : flow.steps;
  return (
    <div className={`flow-detail${flow.session ? " has-session" : ""}`}>
      <SequenceDiagram
        steps={steps}
        failed={flow.status === "failed"}
        success={success}
        deliveredContent={receiptLink}
      />
      <div className="flow-middle">
        {flow.inference ? (
          <InferencePanel flow={flow} />
        ) : flow.session ? (
          <SessionChannel flow={flow} />
        ) : (
          <PaymentSplits flow={flow} success={success} />
        )}
      </div>
      <EventLog events={flow.events} />
    </div>
  );
}
