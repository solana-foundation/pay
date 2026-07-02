import { useReducer, useEffect, useRef, useCallback } from "react";
import type { PaymentFlow, SSEMessage } from "../types";

interface FlowState {
  flows: PaymentFlow[];
  viewerIp: string | null;
  connected: boolean;
}

type FlowAction =
  | { type: "init"; viewerIp: string }
  | { type: "snapshot"; flows: PaymentFlow[] }
  | { type: "flow-created"; flow: PaymentFlow }
  | { type: "flow-updated"; flow: PaymentFlow }
  | { type: "clear" }
  | { type: "connected"; value: boolean };

function reducer(state: FlowState, action: FlowAction): FlowState {
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
    case "clear":
      return { ...state, flows: [] };
    case "connected":
      return { ...state, connected: action.value };
  }
}

const initial: FlowState = { flows: [], viewerIp: null, connected: false };

export function useFlows() {
  const [state, dispatch] = useReducer(reducer, initial);
  const esRef = useRef<EventSource | null>(null);

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
