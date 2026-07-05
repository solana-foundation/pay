import { describe, it, expect } from "vitest";
import {
  shortPayer,
  formatPaidUsd,
  formatTokenCount,
  sortConnections,
  matchConnection,
  groupFlows,
} from "./connections";
import type { ConnectionSummary, PaymentFlow } from "../types";

function makeConnection(
  overrides: Partial<ConnectionSummary> = {},
): ConnectionSummary {
  return {
    id: "conn-1",
    clientIp: "127.0.0.1",
    requests: 3,
    ok: 2,
    failed: 1,
    tokensPrompt: 100,
    tokensCompletion: 200,
    paidUsd: 0.0125,
    startedAt: "2026-07-01T12:00:00.000Z",
    updatedAt: "2026-07-01T12:05:00.000Z",
    ...overrides,
  };
}

function makeFlow(overrides: Partial<PaymentFlow> = {}): PaymentFlow {
  return {
    id: "flow-1",
    protocol: "http",
    resource: "/v1/chat/completions",
    status: "resource-delivered",
    clientIp: "127.0.0.1",
    startedAt: "2026-07-01T12:00:00.000Z",
    updatedAt: "2026-07-01T12:00:02.000Z",
    durationMs: 2000,
    steps: [],
    events: [],
    ...overrides,
  };
}

const PAYER = "F82JhLEREBbYidcKZ3EMbUW3QYUNijk3s9YMoNmUUtog";

describe("shortPayer", () => {
  it("shortens a pubkey to 4…4", () => {
    expect(shortPayer(PAYER)).toBe("F82J…Utog");
  });

  it("leaves short values unchanged", () => {
    expect(shortPayer("abc")).toBe("abc");
  });
});

describe("formatPaidUsd", () => {
  it("renders 4 decimals with a $ prefix", () => {
    expect(formatPaidUsd(0.0125)).toBe("$0.0125");
    expect(formatPaidUsd(0)).toBe("$0.0000");
    expect(formatPaidUsd(1.5)).toBe("$1.5000");
  });
});

describe("formatTokenCount", () => {
  it("keeps small counts as-is", () => {
    expect(formatTokenCount(0)).toBe("0");
    expect(formatTokenCount(999)).toBe("999");
  });

  it("compacts thousands and millions", () => {
    expect(formatTokenCount(1000)).toBe("1k");
    expect(formatTokenCount(1234)).toBe("1.2k");
    expect(formatTokenCount(2_500_000)).toBe("2.5M");
  });
});

describe("sortConnections", () => {
  it("sorts by updatedAt desc without mutating input", () => {
    const older = makeConnection({
      id: "conn-old",
      updatedAt: "2026-07-01T12:00:00.000Z",
    });
    const newer = makeConnection({
      id: "conn-new",
      updatedAt: "2026-07-01T13:00:00.000Z",
    });
    const input = [older, newer];
    const sorted = sortConnections(input);
    expect(sorted.map((c) => c.id)).toEqual(["conn-new", "conn-old"]);
    expect(input.map((c) => c.id)).toEqual(["conn-old", "conn-new"]);
  });
});

describe("matchConnection / groupFlows", () => {
  const walletConn = makeConnection({
    id: "conn-wallet",
    payer: PAYER,
    clientIp: "10.0.0.5",
    updatedAt: "2026-07-01T13:00:00.000Z",
  });
  const ipConn = makeConnection({
    id: "conn-ip",
    clientIp: "127.0.0.1",
    updatedAt: "2026-07-01T12:30:00.000Z",
  });
  const connections = [walletConn, ipConn]; // already newest-first

  it("matches by payer when both set", () => {
    const flow = makeFlow({ payer: PAYER, clientIp: "9.9.9.9" });
    expect(matchConnection(flow, connections)?.id).toBe("conn-wallet");
  });

  it("matches payer-less connections by clientIp", () => {
    const flow = makeFlow({ clientIp: "127.0.0.1" });
    expect(matchConnection(flow, connections)?.id).toBe("conn-ip");
  });

  it("falls back to any connection sharing the clientIp (402 handshakes)", () => {
    // Handshake flow: no payer yet, but same client IP as the wallet-keyed
    // connection created by its paid twin.
    const handshake = makeFlow({
      protocol: "mpp",
      status: "payment-required",
      clientIp: "10.0.0.5",
    });
    expect(matchConnection(handshake, connections)?.id).toBe("conn-wallet");
  });

  it("returns undefined when nothing matches", () => {
    const flow = makeFlow({ clientIp: "203.0.113.7" });
    expect(matchConnection(flow, connections)).toBeUndefined();
  });

  it("groups flows per connection with a trailing other group", () => {
    const paid = makeFlow({ id: "f-paid", payer: PAYER, clientIp: "10.0.0.5" });
    const local = makeFlow({ id: "f-local", clientIp: "127.0.0.1" });
    const stray = makeFlow({ id: "f-stray", clientIp: "203.0.113.7" });

    const groups = groupFlows([paid, local, stray], connections);
    expect(groups.map((g) => g.id)).toEqual(["conn-wallet", "conn-ip", "other"]);
    expect(groups[0].flows.map((f) => f.id)).toEqual(["f-paid"]);
    expect(groups[1].flows.map((f) => f.id)).toEqual(["f-local"]);
    expect(groups[2].connection).toBeNull();
    expect(groups[2].flows.map((f) => f.id)).toEqual(["f-stray"]);
  });

  it("omits the other group when every flow matches", () => {
    const groups = groupFlows(
      [makeFlow({ clientIp: "127.0.0.1" })],
      connections,
    );
    expect(groups.map((g) => g.id)).toEqual(["conn-wallet", "conn-ip"]);
  });

  it("keeps empty connection groups so aggregates stay visible", () => {
    const groups = groupFlows([], connections);
    expect(groups).toHaveLength(2);
    expect(groups.every((g) => g.flows.length === 0)).toBe(true);
  });
});
