import type { PaymentFlow } from "../types";
import { SequenceDiagram } from "./SequenceDiagram";
import { EventLog } from "./EventLog";
import { PaymentSplits } from "./PaymentSplits";

interface Props {
  flow: PaymentFlow;
}

export function FlowDetail({ flow }: Props) {
  return (
    <div className="flow-detail">
      <SequenceDiagram steps={flow.steps} failed={flow.status === "failed"} />
      <PaymentSplits flow={flow} />
      <EventLog events={flow.events} />
    </div>
  );
}
