import { describe, expect, it } from "vitest";
import type { PaymentFlow } from "../types";
import { parseChallenge } from "./PaymentSplits";

function x402Flow(required: unknown): PaymentFlow {
  return {
    id: "flow-1",
    protocol: "x402",
    scheme: "batch-settlement",
    resource: "/v1/chat/completions",
    status: "payment-required",
    clientIp: "127.0.0.1",
    startedAt: "2026-10-03T00:00:00.000Z",
    updatedAt: "2026-10-03T00:00:00.000Z",
    durationMs: 0,
    steps: [],
    events: [],
    challengeHeaders: {
      "payment-required": btoa(JSON.stringify(required)),
    },
  };
}

describe("payment split parsing", () => {
  it("renders an x402 batch offer as a recipient split", () => {
    const parsed = parseChallenge(
      x402Flow({
        x402Version: 2,
        accepts: [
          {
            scheme: "batch-settlement",
            amount: "100000",
            asset: "USDC",
            payTo: "channel-recipient",
          },
        ],
      }),
    );

    expect(parsed?.kind).toBe("payment");
    if (parsed?.kind !== "payment") throw new Error("expected payment split");
    expect(parsed.totalAmount).toBe(0.1);
    expect(parsed.recipients).toEqual([
      {
        label: "Channel recipient",
        address: "channel-recipient",
        amount: 0.1,
      },
    ]);
  });
});
