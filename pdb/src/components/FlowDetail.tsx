import type { PaymentFlow } from "../types";
import { SequenceDiagram } from "./SequenceDiagram";
import { EventLog } from "./EventLog";

interface Props {
  flow: PaymentFlow;
}

export function FlowDetail({ flow }: Props) {
  return (
    <div className="flow-detail">
      <SequenceDiagram steps={flow.steps} />
      <EventLog events={flow.events} />
    </div>
  );
}
