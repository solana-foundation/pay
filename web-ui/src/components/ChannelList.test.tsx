import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import type { PaymentChannel } from "../lib/channels";
import { ChannelList } from "./ChannelList";

describe("ChannelList", () => {
  it("explains remaining x402 autonomy and renders increment ticks", () => {
    const channel: PaymentChannel = {
      id: "channel-1",
      protocol: "x402",
      scheme: "batch-settlement",
      state: "open",
      currency: "USDC",
      decimals: 6,
      deposited: "0.01509 USDC",
      consumed: "0.001296 USDC",
      remaining: "0.013794 USDC",
      usagePercent: 8.58,
      requestPrice: "0.001296 USDC",
      remainingRequests: 10,
      capacityTicks: 11,
      usedTicks: 1,
      requestsPerTick: 1,
      startedAt: "2026-10-04T00:00:00.000Z",
      updatedAt: "2026-10-04T00:00:01.000Z",
      requests: [],
    };

    const html = renderToStaticMarkup(
      <ChannelList channels={[channel]} selectedId={null} onSelect={() => undefined} />,
    );

    expect(html).toContain("10 requests left");
    expect(html).toContain("at the latest 0.001296 USDC charge");
    expect(html.match(/class="used"/g)).toHaveLength(1);
    expect(html.match(/class="available"/g)).toHaveLength(10);
    expect(html).toContain('aria-valuetext="10 requests left');
  });
});
