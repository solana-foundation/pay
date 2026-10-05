import { describe, expect, it } from "vitest";
import type { PaymentFlow } from "../types";
import { channelMatches, paymentChannels, scopedPaymentChannels } from "./channels";

function flow(overrides: Partial<PaymentFlow>): PaymentFlow {
  return {
    id: "flow-1",
    protocol: "x402",
    scheme: "batch-settlement",
    resource: "/v1/chat/completions",
    status: "resource-delivered",
    clientIp: "127.0.0.1",
    startedAt: "2026-10-04T00:00:00.000Z",
    updatedAt: "2026-10-04T00:00:01.000Z",
    durationMs: 1000,
    steps: [],
    events: [],
    ...overrides,
  };
}

describe("paymentChannels", () => {
  it("groups batch requests and calculates remaining stablecoin capacity", () => {
    const channels = paymentChannels([
      flow({
        id: "open",
        payment: {
          channelId: "channel-1",
          action: "channel opened",
          depositAmount: "0.0100 USDC",
          asset: "USDC",
          recipient: "provider-wallet",
        },
      }),
      flow({
        id: "voucher",
        startedAt: "2026-10-04T00:00:02.000Z",
        updatedAt: "2026-10-04T00:00:03.000Z",
        payment: {
          channelId: "channel-1",
          action: "voucher",
          voucherAmount: "0.0030 USDC",
          asset: "USDC",
        },
      }),
      flow({
        id: "topup",
        startedAt: "2026-10-04T00:00:04.000Z",
        updatedAt: "2026-10-04T00:00:05.000Z",
        payment: {
          channelId: "channel-1",
          action: "channel topped up",
          depositAmount: "0.0050 USDC",
          asset: "USDC",
        },
      }),
    ]);

    expect(channels).toHaveLength(1);
    expect(channels[0]).toMatchObject({
      id: "channel-1",
      protocol: "x402",
      deposited: "0.015 USDC",
      consumed: "0.003 USDC",
      remaining: "0.012 USDC",
      usagePercent: 20,
    });
    expect(channels[0].requests.map((request) => request.action)).toEqual([
      "open",
      "voucher",
      "topup",
    ]);
  });

  it("uses the latest session capacity without summing repeated snapshots", () => {
    const channels = paymentChannels([
      flow({
        id: "session-open",
        protocol: "session",
        scheme: "session",
        session: {
          sessionId: "session-1",
          state: "open",
          action: "open",
          deposit: "10000",
          cumulative: "1000",
          currency: "USDC",
        },
      }),
      flow({
        id: "session-voucher",
        protocol: "session",
        scheme: "session",
        startedAt: "2026-10-04T00:00:02.000Z",
        updatedAt: "2026-10-04T00:00:03.000Z",
        session: {
          sessionId: "session-1",
          state: "open",
          action: "voucher",
          deposit: "10000",
          cumulative: "2500",
          currency: "USDC",
        },
      }),
    ]);

    expect(channels[0]).toMatchObject({
      protocol: "mpp",
      deposited: "0.01 USDC",
      consumed: "0.0025 USDC",
      remaining: "0.0075 USDC",
      usagePercent: 25,
    });
  });

  it("uses the server channel snapshot for authorization-only requests", () => {
    const channels = paymentChannels([
      flow({
        payment: {
          channelId: "channel-1",
          action: "authorization",
          authorizedAmount: "0.0050 USDC",
          chargeAmount: "1296",
          channelBalance: "15090",
          chargedCumulativeAmount: "1296",
          totalClaimed: "0",
          asset: "USDC",
        },
      }),
    ]);

    expect(channels[0]).toMatchObject({
      deposited: "0.01509 USDC",
      consumed: "0.001296 USDC",
      remaining: "0.013794 USDC",
      requestPrice: "0.001296 USDC",
      remainingRequests: 10,
      capacityTicks: 11,
      usedTicks: 1,
      requestsPerTick: 1,
    });
    expect(channels[0].usagePercent).toBeCloseTo(8.58, 2);
    expect(channels[0].requests[0].amount).toBe("0.001296 USDC");
  });

  it("searches channel metadata and request paths", () => {
    const [channel] = paymentChannels([
      flow({
        payment: {
          channelId: "channel-solana",
          asset: "USDC",
          recipient: "provider-wallet",
        },
      }),
    ]);

    expect(channelMatches(channel, "channel-solana")).toBe(true);
    expect(channelMatches(channel, "chat/completions")).toBe(true);
    expect(channelMatches(channel, "missing")).toBe(false);
  });

  it("keeps full channel history when a status filter matches one request", () => {
    const opened = flow({
      id: "open",
      payment: { channelId: "channel-1", action: "channel opened", depositAmount: "10000" },
    });
    const failed = flow({
      id: "failed",
      status: "failed",
      startedAt: "2026-10-04T00:00:02.000Z",
      updatedAt: "2026-10-04T00:00:03.000Z",
      payment: { channelId: "channel-1", action: "voucher", voucherAmount: "1000" },
    });

    const visible = scopedPaymentChannels(paymentChannels([opened, failed]), [failed], "");

    expect(visible).toHaveLength(1);
    expect(visible[0].requests.map(({ flow }) => flow.id)).toEqual(["open", "failed"]);
  });

  it("does not advertise spendable capacity after a channel closes", () => {
    const channels = paymentChannels([
      flow({
        id: "open",
        payment: {
          channelId: "channel-1",
          action: "channel opened",
          channelBalance: "10000",
          chargeAmount: "1000",
          chargedCumulativeAmount: "2000",
        },
      }),
      flow({
        id: "refund",
        startedAt: "2026-10-04T00:00:02.000Z",
        updatedAt: "2026-10-04T00:00:03.000Z",
        payment: {
          channelId: "channel-1",
          action: "refund",
          channelBalance: "10000",
          chargeAmount: "1000",
          chargedCumulativeAmount: "2000",
        },
      }),
    ]);

    expect(channels[0]).toMatchObject({
      state: "closed",
      remaining: "0 USDC",
      remainingRequests: 0,
      usagePercent: 100,
      capacityTicks: 10,
      usedTicks: 10,
    });
  });
});
