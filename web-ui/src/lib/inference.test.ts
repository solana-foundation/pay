import { describe, it, expect } from "vitest";
import {
  portFromBaseUrl,
  formatTokPerSec,
  providerColor,
  isHexColor,
  hasPaymentData,
  inferenceSteps,
} from "./inference";
import { statusLabels } from "../components/StatusIndicator";
import type { PaymentFlow, ProviderSummary } from "../types";

function makeFlow(overrides: Partial<PaymentFlow> = {}): PaymentFlow {
  return {
    id: "flow-1",
    protocol: "mpp",
    resource: "/v1/chat/completions",
    status: "in-progress",
    clientIp: "127.0.0.1",
    startedAt: "2026-07-01T12:00:00.000Z",
    updatedAt: "2026-07-01T12:00:02.000Z",
    durationMs: 2000,
    steps: [],
    events: [],
    inference: { provider: "ollama", streamed: true },
    ...overrides,
  };
}

describe("portFromBaseUrl", () => {
  it("extracts an explicit port", () => {
    expect(portFromBaseUrl("http://127.0.0.1:11434")).toBe("11434");
    expect(portFromBaseUrl("http://localhost:1234/v1")).toBe("1234");
  });

  it("falls back to protocol defaults", () => {
    expect(portFromBaseUrl("http://example.com")).toBe("80");
    expect(portFromBaseUrl("https://example.com")).toBe("443");
  });

  it("returns null for invalid input", () => {
    expect(portFromBaseUrl("not a url")).toBeNull();
  });
});

describe("formatTokPerSec", () => {
  it("formats with one decimal", () => {
    expect(formatTokPerSec(41.23)).toBe("41.2 tok/s");
    expect(formatTokPerSec(41.28)).toBe("41.3 tok/s");
    expect(formatTokPerSec(0)).toBe("0.0 tok/s");
  });

  it("returns null when absent or not finite", () => {
    expect(formatTokPerSec(undefined)).toBeNull();
    expect(formatTokPerSec(NaN)).toBeNull();
    expect(formatTokPerSec(Infinity)).toBeNull();
  });
});

describe("providerColor / isHexColor", () => {
  const providers: ProviderSummary[] = [
    {
      slug: "ollama",
      title: "Ollama",
      baseUrl: "http://127.0.0.1:11434",
      up: true,
      models: [],
      color: "#22c55e",
    },
  ];

  it("looks up the brand color by slug", () => {
    expect(providerColor("ollama", providers)).toBe("#22c55e");
    expect(providerColor("lm-studio", providers)).toBeUndefined();
    expect(providerColor("ollama", undefined)).toBeUndefined();
  });

  it("only accepts 6-digit hex colors", () => {
    expect(isHexColor("#22c55e")).toBe(true);
    expect(isHexColor("#GGGGGG")).toBe(false);
    expect(isHexColor("red")).toBe(false);
    expect(isHexColor(undefined)).toBe(false);
  });
});

describe("statusLabels", () => {
  it("covers in-progress", () => {
    expect(statusLabels["in-progress"]).toBe("In Progress");
  });
});

describe("hasPaymentData", () => {
  it("is false for a plain inference passthrough flow", () => {
    expect(hasPaymentData(makeFlow())).toBe(false);
  });

  it("is false for a bare http flow without inference", () => {
    expect(
      hasPaymentData(makeFlow({ protocol: "http", inference: undefined })),
    ).toBe(false);
  });

  it("is true when payment data is present", () => {
    expect(hasPaymentData(makeFlow({ amount: "0.01" }))).toBe(true);
    expect(
      hasPaymentData(makeFlow({ challengeHeaders: { "x-payment": "..." } })),
    ).toBe(true);
    expect(
      hasPaymentData(
        makeFlow({
          steps: [
            { key: "challenge", label: "402", status: "completed", ts: null },
          ],
        }),
      ),
    ).toBe(true);
  });
});

describe("inferenceSteps", () => {
  it("in-progress without ttft: waiting on first token", () => {
    const steps = inferenceSteps(makeFlow());
    expect(steps.map((s) => [s.key, s.status])).toEqual([
      ["request", "completed"],
      ["first-token", "in-progress"],
      ["completed", "pending"],
    ]);
    expect(steps[0].ts).toBe("2026-07-01T12:00:00.000Z");
  });

  it("in-progress with ttft: streaming after first token", () => {
    const steps = inferenceSteps(
      makeFlow({
        inference: { provider: "ollama", streamed: true, ttftMs: 182 },
      }),
    );
    expect(steps.map((s) => s.status)).toEqual([
      "completed",
      "completed",
      "in-progress",
    ]);
    expect(steps[1].ts).toBe("2026-07-01T12:00:00.182Z");
    expect(steps[2].ts).toBeNull();
  });

  it("resource-delivered: all completed, with timestamps", () => {
    const steps = inferenceSteps(
      makeFlow({
        status: "resource-delivered",
        inference: { provider: "ollama", streamed: false, ttftMs: 182 },
      }),
    );
    expect(steps.every((s) => s.status === "completed")).toBe(true);
    expect(steps[2].ts).toBe("2026-07-01T12:00:02.000Z");
  });

  it("resource-delivered without ttft (non-streamed) still completes", () => {
    const steps = inferenceSteps(makeFlow({ status: "resource-delivered" }));
    expect(steps.every((s) => s.status === "completed")).toBe(true);
    expect(steps[1].ts).toBeNull();
  });

  it("failed before first token: remaining steps pending", () => {
    const steps = inferenceSteps(makeFlow({ status: "failed" }));
    expect(steps.map((s) => s.status)).toEqual([
      "completed",
      "pending",
      "pending",
    ]);
  });

  it("failed after first token: first token kept, completion pending", () => {
    const steps = inferenceSteps(
      makeFlow({
        status: "failed",
        inference: { provider: "ollama", streamed: true, ttftMs: 90 },
      }),
    );
    expect(steps.map((s) => s.status)).toEqual([
      "completed",
      "completed",
      "pending",
    ]);
  });
});
