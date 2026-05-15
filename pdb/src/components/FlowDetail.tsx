import type { PaymentFlow } from "../types";
import { SequenceDiagram } from "./SequenceDiagram";
import { EventLog } from "./EventLog";
import { PaymentSplits } from "./PaymentSplits";
import { SessionChannel } from "./SessionChannel";

interface Props {
  flow: PaymentFlow;
}

export function FlowDetail({ flow }: Props) {
  const success = flow.status === "resource-delivered";
  return (
    <div className={`flow-detail${flow.session ? " has-session" : ""}`}>
      <SequenceDiagram steps={flow.steps} failed={flow.status === "failed"} success={success} />
      <div className="flow-middle">
        {flow.session ? (
          <SessionChannel flow={flow} />
        ) : (
          <PaymentSplits flow={flow} success={success} />
        )}
      </div>
      <EventLog events={flow.events} />
    </div>
  );
}
