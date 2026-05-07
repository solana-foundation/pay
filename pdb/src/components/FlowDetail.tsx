import { useState } from "react";
import type { PaymentFlow } from "../types";
import { SequenceDiagram } from "./SequenceDiagram";
import { EventLog } from "./EventLog";
import { ReplayPanel } from "./ReplayPanel";
import { RawHttpPanel } from "./RawHttpPanel";

interface Props {
  flow: PaymentFlow;
}

export function FlowDetail({ flow }: Props) {
  const success = flow.status === "resource-delivered";
  const [tab, setTab] = useState<"replay" | "request" | "response">("replay");

  return (
    <div className="flow-detail">
      <SequenceDiagram steps={flow.steps} failed={flow.status === "failed"} success={success} />
      <div className="detail-panel">
        <div className="detail-tabs">
          <button
            className={`detail-tab${tab === "replay" ? " active" : ""}`}
            onClick={() => setTab("replay")}
            type="button"
          >
            Replay
          </button>
          <button
            className={`detail-tab${tab === "request" ? " active" : ""}`}
            onClick={() => setTab("request")}
            type="button"
          >
            Request
          </button>
          <button
            className={`detail-tab${tab === "response" ? " active" : ""}`}
            onClick={() => setTab("response")}
            type="button"
          >
            Response
          </button>
        </div>
        <div className="detail-content">
          {tab === "replay" && <ReplayPanel flow={flow} success={success} />}
          {tab === "request" && <RawHttpPanel flow={flow} mode="request" />}
          {tab === "response" && <RawHttpPanel flow={flow} mode="response" />}
        </div>
      </div>
      <EventLog events={flow.events} />
    </div>
  );
}
