import { useReducer, useEffect, useRef, useCallback } from "react";
import type { PaymentFlow, ProviderSummary, SSEMessage } from "../types";

export interface FlowState {
  flows: PaymentFlow[];
  viewerIp: string | null;
  connected: boolean;
  providers: ProviderSummary[];
  // True once a `provider-status` SSE message has arrived; config-seeded
  // providers never overwrite live data.
  providersLive: boolean;
}

export type FlowAction =
  | { type: "init"; viewerIp: string }
  | { type: "snapshot"; flows: PaymentFlow[] }
  | { type: "flow-created"; flow: PaymentFlow }
  | { type: "flow-updated"; flow: PaymentFlow }
  | { type: "provider-status"; providers: ProviderSummary[] }
  | { type: "seed-providers"; providers: ProviderSummary[] }
  | { type: "clear" }
  | { type: "connected"; value: boolean };

export function reducer(state: FlowState, action: FlowAction): FlowState {
  switch (action.type) {
    case "init":
      return { ...state, viewerIp: action.viewerIp };
    case "snapshot":
      return { ...state, flows: action.flows };
    case "flow-created":
      return { ...state, flows: [...state.flows, action.flow] };
    case "flow-updated":
      return {
        ...state,
        flows: state.flows.map((f) =>
          f.id === action.flow.id ? action.flow : f,
        ),
      };
    case "provider-status":
      return { ...state, providers: action.providers, providersLive: true };
    case "seed-providers":
      // Initial fill from /api/config; SSE is the source of truth once live.
      return state.providersLive
        ? state
        : { ...state, providers: action.providers };
    case "clear":
      return { ...state, flows: [] };
    case "connected":
      return { ...state, connected: action.value };
  }
}

export const initialFlowState: FlowState = {
  flows: [],
  viewerIp: null,
  connected: false,
  providers: [],
  providersLive: false,
};

export function useFlows(initialProviders?: ProviderSummary[]) {
  const [state, dispatch] = useReducer(reducer, initialFlowState);
  const esRef = useRef<EventSource | null>(null);

  useEffect(() => {
    if (initialProviders) {
      dispatch({ type: "seed-providers", providers: initialProviders });
    }
  }, [initialProviders]);

  useEffect(() => {
    const es = new EventSource("/__402/pdb/logs/stream");
    esRef.current = es;

    console.log("[PDB] Connecting SSE: /__402/pdb/logs/stream");
    es.onopen = () => {
      console.log("[PDB] SSE connected");
      dispatch({ type: "connected", value: true });
    };

    es.onmessage = (ev) => {
      console.log("[PDB SSE]", ev.data.slice(0, 100));
      const msg: SSEMessage = JSON.parse(ev.data);
      switch (msg.type) {
        case "init":
          dispatch({ type: "init", viewerIp: msg.viewerIp });
          break;
        case "snapshot":
          dispatch({ type: "snapshot", flows: msg.flows });
          break;
        case "flow-created":
          dispatch({ type: "flow-created", flow: msg.flow });
          break;
        case "flow-updated":
          dispatch({ type: "flow-updated", flow: msg.flow });
          break;
        case "provider-status":
          dispatch({ type: "provider-status", providers: msg.providers });
          break;
      }
    };

    es.onerror = (e) => {
      console.error("[PDB] SSE error", e);
      dispatch({ type: "connected", value: false });
    };

    return () => {
      es.close();
      esRef.current = null;
    };
  }, []);

  const clear = useCallback(() => dispatch({ type: "clear" }), []);

  return { ...state, clear };
}
