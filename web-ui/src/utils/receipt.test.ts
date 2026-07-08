import { describe, expect, it } from "vitest";
import type { PaymentFlow } from "../types";
import { parseReceipt, receiptSignature } from "./receipt";

function encodeJson(value: unknown): string {
  return btoa(JSON.stringify(value));
}

function flowWithHeaders(responseHeaders: Record<string, string>): PaymentFlow {
  return {
    id: "flow-1",
    protocol: "x402",
    resource: "/v1/messages",
    status: "resource-delivered",
    clientIp: "127.0.0.1",
    startedAt: "2026-04-02T00:00:00.000Z",
    updatedAt: "2026-04-02T00:00:01.000Z",
    durationMs: 1000,
    steps: [],
    events: [],
    responseHeaders,
  };
}

describe("receipt parsing", () => {
  it("reads x402 payment-response transaction signatures", () => {
    const receipt = parseReceipt(
      flowWithHeaders({
        "payment-response": encodeJson({
          transaction: "settlement-signature",
          amount: "1234",
        }),
      }),
    );

    expect(receiptSignature(receipt)).toBe("settlement-signature");
  });
});
