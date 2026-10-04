import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import type { PaymentFlow } from "../types";
import { FlowInspector } from "./FlowInspector";

function makeFlow(overrides: Partial<PaymentFlow> = {}): PaymentFlow {
  return {
    id: "flow-1",
    protocol: "x402",
    resource: "/v1/chat/completions",
    status: "resource-delivered",
    clientIp: "127.0.0.1",
    startedAt: "2026-10-04T00:00:00.000Z",
    updatedAt: "2026-10-04T00:00:01.000Z",
    durationMs: 1000,
    steps: [],
    events: [{ ts: "2026-10-04T00:00:01.000Z", message: "delivered" }],
    ...overrides,
  };
}

describe("FlowInspector", () => {
  it("orders inference, payment, request, response, and events", () => {
    const html = renderToStaticMarkup(
      <FlowInspector
        flow={makeFlow()}
        inference={<div>Inference details</div>}
        paymentVisualization={<div>Split details</div>}
      />,
    );

    const labels = ["Inference", "Payment", "Request", "Response", "Events"];
    for (let i = 1; i < labels.length; i += 1) {
      expect(html.indexOf(`>${labels[i - 1]}`)).toBeLessThan(
        html.indexOf(`>${labels[i]}`),
      );
    }
    expect(html).not.toContain(">Splits<");
    expect(html).not.toContain(">Channel<");
  });

  it("renders split details inside the payment tab", () => {
    const html = renderToStaticMarkup(
      <FlowInspector
        flow={makeFlow()}
        paymentVisualization={<div>Split details</div>}
      />,
    );

    expect(html).toContain("Split details");
    expect(html.indexOf(">Payment<")).toBeLessThan(html.indexOf(">Request<"));
  });
});
