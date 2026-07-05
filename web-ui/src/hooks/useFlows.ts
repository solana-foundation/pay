import { useReducer, useEffect, useRef, useCallback } from "react";
import type {
  ConnectionSummary,
  PaymentFlow,
  ProviderSummary,
  SSEMessage,
} from "../types";
import { sortConnections } from "../lib/connections";

export interface FlowState {
  flows: PaymentFlow[];
  viewerIp: string | null;
  connected: boolean;
  providers: ProviderSummary[];
  // True once a `provider-status` SSE message has arrived; config-seeded
  // providers never overwrite live data.
  providersLive: boolean;
  // Per-connection aggregates (inference mode), newest activity first.
  connections: ConnectionSummary[];
}

export type FlowAction =
  | { type: "init"; viewerIp: string }
  | { type: "snapshot"; flows: PaymentFlow[] }
  | { type: "flow-created"; flow: PaymentFlow }
  | { type: "flow-updated"; flow: PaymentFlow }
  | { type: "provider-status"; providers: ProviderSummary[] }
  | { type: "seed-providers"; providers: ProviderSummary[] }
  | { type: "connections-snapshot"; connections: ConnectionSummary[] }
  | { type: "connection-updated"; connection: ConnectionSummary }
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
    case "connections-snapshot":
      // Full replacement; defensively re-sorted newest-activity first.
      return { ...state, connections: sortConnections(action.connections) };
    case "connection-updated": {
      // Upsert by id, then resort by updatedAt desc.
      const rest = state.connections.filter(
        (c) => c.id !== action.connection.id,
      );
      return {
        ...state,
        connections: sortConnections([action.connection, ...rest]),
      };
    }
    case "clear":
      // Clearing the log also drops per-connection aggregates so the grouped
      // view starts fresh; the backend repopulates them on new traffic.
      return { ...state, flows: [], connections: [] };
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
  connections: [],
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
        case "connections-snapshot":
          dispatch({
            type: "connections-snapshot",
            connections: msg.connections,
          });
          break;
        case "connection-updated":
          dispatch({ type: "connection-updated", connection: msg.connection });
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
