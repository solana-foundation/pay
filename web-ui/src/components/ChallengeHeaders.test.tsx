import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import {
  ChallengeHeaders,
  decodeChallengeHeader,
} from "./ChallengeHeaders";

function base64UrlJson(value: unknown): string {
  return btoa(JSON.stringify(value))
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");
}

function facts(decoded: ReturnType<typeof decodeChallengeHeader>) {
  return Object.fromEntries(
    decoded.sections.flatMap((section) =>
      section.facts.map((fact) => [fact.label, fact.value]),
    ),
  );
}

describe("challenge header decoding", () => {
  it("decodes MPP authentication parameters and its request token", () => {
    const request = base64UrlJson({
      amount: "125000",
      currency: "USDC",
      recipient: "provider-wallet",
      methodDetails: {
        network: "mainnet",
        decimals: 6,
        tokenProgram: "TokenProgram111",
      },
    });
    const decoded = decodeChallengeHeader(
      "www-authenticate",
      `Payment id="challenge-1", realm="blockrun", method="solana", intent="charge", request="${request}"`,
    );

    expect(decoded.protocol).toBe("MPP");
    expect(facts(decoded)).toMatchObject({
      Id: "challenge-1",
      Realm: "blockrun",
      Method: "solana",
      Intent: "charge",
      Amount: "0.125 USDC (125000 base units)",
      Currency: "USDC",
      Recipient: "provider-wallet",
      "Method Details · Network": "mainnet",
      "Method Details · Token Program": "TokenProgram111",
    });
  });

  it("decodes x402 offers, assets, timeouts, and channel state", () => {
    const raw = btoa(
      JSON.stringify({
        x402Version: 2,
        error: "cumulative_amount_mismatch",
        accepts: [
          {
            scheme: "batch-settlement",
            network: "solana:mainnet",
            amount: "100000",
            asset: "USDC",
            payTo: "recipient-wallet",
            maxTimeoutSeconds: 300,
            extra: {
              decimals: 6,
              tokenProgram: "TokenProgram111",
              channelState: {
                channelId: "channel-1",
                balance: "500000",
                totalClaimed: "200000",
              },
            },
          },
        ],
      }),
    );
    const decoded = decodeChallengeHeader("payment-required", raw);
    const allFacts = facts(decoded);

    expect(decoded.protocol).toBe("x402");
    expect(decoded.sections.map((section) => section.title)).toEqual([
      "Envelope",
      "Offer 1",
    ]);
    expect(allFacts).toMatchObject({
      "X402 Version": "2",
      Error: "cumulative_amount_mismatch",
      Scheme: "batch-settlement",
      Network: "solana:mainnet",
      Amount: "0.1 USDC (100000 base units)",
      Asset: "USDC",
      "Pay To": "recipient-wallet",
      "Max Timeout Seconds": "300",
      "Extra · Token Program": "TokenProgram111",
      "Extra · Channel State · Channel Id": "channel-1",
    });
  });

  it("recognizes an X402 WWW-Authenticate compatibility challenge", () => {
    const requirements = base64UrlJson({
      x402Version: 2,
      accepts: [
        {
          scheme: "batch-settlement",
          network: "solana:mainnet",
          amount: "5030",
          asset: "USDC",
          payTo: "recipient-wallet",
        },
      ],
    });
    const decoded = decodeChallengeHeader(
      "www-authenticate",
      `X402 requirements="${requirements}"`,
    );

    expect(decoded.protocol).toBe("x402");
    expect(decoded.sections.map((section) => section.title)).toEqual([
      "Challenge",
      "Envelope",
      "Offer 1",
    ]);
    expect(facts(decoded)).toMatchObject({
      "Authentication scheme": "X402",
      Carrier: "WWW-Authenticate",
      "X402 Version": "2",
      Scheme: "batch-settlement",
      Amount: "0.00503 USDC (5030 base units)",
    });
  });

  it("shows a 16 by 16 raw preview and a copy control", () => {
    const raw = "a".repeat(48);
    const html = renderToStaticMarkup(
      <ChallengeHeaders headers={{ "payment-required": raw }} />,
    );

    expect(html).toContain(`${"a".repeat(16)}…${"a".repeat(16)}`);
    expect(html).toContain('aria-label="Copy raw payment-required header"');
  });
});
