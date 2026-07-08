import { describe, expect, it } from "vitest";
import type { PaymentFlow } from "../types";
import { receiptLinkHref } from "../components/ReceiptLink";
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

  it("reads nested x402 settlement transaction signatures", () => {
    const receipt = parseReceipt(
      flowWithHeaders({
        "payment-response": encodeJson({
          settlement: {
            transactionId: "nested-settlement-signature",
          },
        }),
      }),
    );

    expect(receiptSignature(receipt)).toBe("nested-settlement-signature");
  });

  it("reads direct payment-response settlement references", () => {
    const receipt = parseReceipt(
      flowWithHeaders({
        "payment-response": "direct-settlement-signature",
      }),
    );

    expect(receiptSignature(receipt)).toBe("direct-settlement-signature");
  });

  it("matches receipt headers case-insensitively", () => {
    const receipt = parseReceipt(
      flowWithHeaders({
        "Payment-Response": "mixed-case-settlement-signature",
      }),
    );

    expect(receiptSignature(receipt)).toBe("mixed-case-settlement-signature");
  });

  it("falls back to payment-response when receipt url is empty", () => {
    const href = receiptLinkHref(
      flowWithHeaders({
        "payment-receipt-url": "",
        "payment-response": "settlement-signature",
      }),
      null,
    );

    expect(href).toBe("https://pay.sh/receipt/settlement-signature?view=advanced");
  });

  it("links sandbox inference receipts", () => {
    const href = receiptLinkHref(
      flowWithHeaders({
        "payment-response": "settlement-signature",
      }),
      { network: "sandbox" },
    );

    expect(href).toBe(
      "https://pay.sh/receipt/settlement-signature?network=sandbox&view=advanced",
    );
  });

  it("links x402 upto payment-response receipts from their embedded network", () => {
    const href = receiptLinkHref(
      flowWithHeaders({
        "payment-response": encodeJson({
          success: true,
          payer: "zAZwrBVzcCuYZYGq8SQysp5KD5xs8fCZZNK4g9D1uhi",
          transaction:
            "3ZbDMQdNGTVYpVEJ955y77PnTjwFzbxoZryhd56eLmwnwTessEsJrtjta2s6mZCWPePjG99KuWDM7G6QKD9anBC7",
          network: "solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1",
          amount: "30611",
        }),
      }),
      null,
    );

    expect(href).toBe(
      "https://pay.sh/receipt/3ZbDMQdNGTVYpVEJ955y77PnTjwFzbxoZryhd56eLmwnwTessEsJrtjta2s6mZCWPePjG99KuWDM7G6QKD9anBC7?network=sandbox&view=advanced",
    );
  });
});
