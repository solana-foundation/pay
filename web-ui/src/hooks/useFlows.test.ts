import { describe, it, expect } from "vitest";
import { reducer, initialFlowState } from "./useFlows";
import type { FlowState } from "./useFlows";
import type { PaymentFlow, ProviderSummary } from "../types";

function makeFlow(overrides: Partial<PaymentFlow> = {}): PaymentFlow {
  return {
    id: "flow-1",
    protocol: "mpp",
    resource: "/v1/chat/completions",
    status: "in-progress",
    clientIp: "127.0.0.1",
    startedAt: "2026-07-01T12:00:00.000Z",
    updatedAt: "2026-07-01T12:00:00.000Z",
    durationMs: 0,
    steps: [],
    events: [],
    ...overrides,
  };
}

const ollama: ProviderSummary = {
  slug: "ollama",
  title: "Ollama",
  baseUrl: "http://127.0.0.1:11434",
  up: true,
  models: ["llama3.2:3b"],
  color: "#22c55e",
};

describe("useFlows reducer", () => {
  it("handles snapshot → flow-created → flow-updated → provider-status", () => {
    let state: FlowState = initialFlowState;

    const existing = makeFlow({ id: "flow-0", status: "resource-delivered" });
    state = reducer(state, { type: "snapshot", flows: [existing] });
    expect(state.flows).toEqual([existing]);

    const created = makeFlow({
      inference: { provider: "ollama", streamed: true },
    });
    state = reducer(state, { type: "flow-created", flow: created });
    expect(state.flows).toHaveLength(2);
    expect(state.flows[1].status).toBe("in-progress");

    const updated = makeFlow({
      status: "resource-delivered",
      durationMs: 1234,
      inference: {
        provider: "ollama",
        model: "llama3.2:3b",
        streamed: true,
        ttftMs: 182,
        tokensPrompt: 214,
        tokensCompletion: 512,
        tokensPerSec: 41.23,
      },
    });
    state = reducer(state, { type: "flow-updated", flow: updated });
    expect(state.flows).toHaveLength(2);
    expect(state.flows[1]).toEqual(updated);
    expect(state.flows[1].inference?.tokensPerSec).toBe(41.23);
    // flow-updated must not touch other flows
    expect(state.flows[0]).toEqual(existing);

    state = reducer(state, { type: "provider-status", providers: [ollama] });
    expect(state.providers).toEqual([ollama]);
    expect(state.providersLive).toBe(true);
  });

  it("handles http-protocol passthrough flows with and without inference", () => {
    // Bare passthrough exchange (no inference metadata yet)
    const bare = makeFlow({ id: "flow-http", protocol: "http" });
    let state = reducer(initialFlowState, { type: "flow-created", flow: bare });
    expect(state.flows[0].protocol).toBe("http");
    expect(state.flows[0].inference).toBeUndefined();

    // Same flow updated with inference data mid-stream
    const updated = makeFlow({
      id: "flow-http",
      protocol: "http",
      inference: { provider: "ollama", streamed: true, ttftMs: 90 },
    });
    state = reducer(state, { type: "flow-updated", flow: updated });
    expect(state.flows).toHaveLength(1);
    expect(state.flows[0].inference?.provider).toBe("ollama");
  });

  it("flow-updated for an unknown id leaves flows unchanged", () => {
    const flow = makeFlow();
    let state = reducer(initialFlowState, { type: "flow-created", flow });
    state = reducer(state, {
      type: "flow-updated",
      flow: makeFlow({ id: "flow-999" }),
    });
    expect(state.flows).toEqual([flow]);
  });

  it("seed-providers fills initial providers from config", () => {
    const state = reducer(initialFlowState, {
      type: "seed-providers",
      providers: [ollama],
    });
    expect(state.providers).toEqual([ollama]);
    expect(state.providersLive).toBe(false);
  });

  it("seed-providers never overwrites live SSE provider status", () => {
    const live: ProviderSummary = { ...ollama, up: false };
    let state = reducer(initialFlowState, {
      type: "provider-status",
      providers: [live],
    });
    state = reducer(state, { type: "seed-providers", providers: [ollama] });
    expect(state.providers).toEqual([live]);
  });

  it("provider-status replaces seeded providers", () => {
    let state = reducer(initialFlowState, {
      type: "seed-providers",
      providers: [ollama],
    });
    const next: ProviderSummary = { ...ollama, models: [] };
    state = reducer(state, { type: "provider-status", providers: [next] });
    expect(state.providers).toEqual([next]);
  });

  it("clear empties flows but keeps providers and connection state", () => {
    let state = reducer(initialFlowState, {
      type: "flow-created",
      flow: makeFlow(),
    });
    state = reducer(state, { type: "provider-status", providers: [ollama] });
    state = reducer(state, { type: "connected", value: true });
    state = reducer(state, { type: "clear" });
    expect(state.flows).toEqual([]);
    expect(state.providers).toEqual([ollama]);
    expect(state.connected).toBe(true);
  });
});
