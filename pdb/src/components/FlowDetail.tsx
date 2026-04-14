import type { PaymentFlow } from "../types";
import { SequenceDiagram } from "./SequenceDiagram";
import { EventLog } from "./EventLog";
import { PaymentSplits } from "./PaymentSplits";

interface Props {
  flow: PaymentFlow;
}

export function FlowDetail({ flow }: Props) {
  const success = flow.status === "resource-delivered";
  return (
    <div className="flow-detail">
      <SequenceDiagram steps={flow.steps} failed={flow.status === "failed"} success={success} />
      <PaymentSplits flow={flow} success={success} />
      <EventLog events={flow.events} />
    </div>
  );
}
