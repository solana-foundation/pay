import { useMemo } from "react";
import type { PaymentFlow } from "../types";
import { buildRawRequest, buildRawResponse } from "../utils/httpReplay";

interface Props {
  flow: PaymentFlow;
  mode: "request" | "response";
}

export function RawHttpPanel({ flow, mode }: Props) {
  const content = useMemo(
    () => (mode === "request" ? buildRawRequest(flow) : buildRawResponse(flow)),
    [flow, mode],
  );

  return (
    <div className="raw-http-panel">
      <div className="raw-http-header">
        <h3>{mode === "request" ? "Raw Request" : "Raw Response"}</h3>
        <span>Payment headers are redacted automatically.</span>
      </div>
      <pre className="raw-http-pre">{content}</pre>
    </div>
  );
}
