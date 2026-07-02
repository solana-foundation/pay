import { useRef, useEffect } from "react";
import type { PaymentFlow, ProviderSummary } from "../types";
import { FlowRow } from "./FlowRow";
import { FlowDetail } from "./FlowDetail";

interface Props {
  flows: PaymentFlow[];
  selectedId: string | null;
  onSelect: (id: string | null) => void;
  // Live provider list (inference mode) — for row badge brand colors.
  providers?: ProviderSummary[];
}

export function FlowList({ flows, selectedId, onSelect, providers }: Props) {
  const listRef = useRef<HTMLDivElement>(null);
  const prevCountRef = useRef(flows.length);

  // Auto-scroll to bottom when new flows arrive
  useEffect(() => {
    if (flows.length > prevCountRef.current && listRef.current) {
      listRef.current.scrollTop = listRef.current.scrollHeight;
    }
    prevCountRef.current = flows.length;
  }, [flows.length]);

  if (flows.length === 0) {
    return (
      <div className="flow-list">
        <div className="flow-empty">Waiting for payment flows...</div>
      </div>
    );
  }

  return (
    <div className="flow-list" ref={listRef}>
      {flows.map((flow) => (
        <div key={flow.id}>
          <FlowRow
            flow={flow}
            selected={selectedId === flow.id}
            onClick={() =>
              onSelect(selectedId === flow.id ? null : flow.id)
            }
            providers={providers}
          />
          {selectedId === flow.id && <FlowDetail flow={flow} />}
        </div>
      ))}
    </div>
  );
}
