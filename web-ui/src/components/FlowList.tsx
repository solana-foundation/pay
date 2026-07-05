import { useRef, useEffect, useMemo, useState } from "react";
import type { ConnectionSummary, PaymentFlow, ProviderSummary } from "../types";
import { FlowRow } from "./FlowRow";
import { FlowDetail } from "./FlowDetail";
import { ConnectionGroupHeader } from "./ConnectionGroup";
import { groupFlows } from "../lib/connections";

interface Props {
  flows: PaymentFlow[];
  selectedId: string | null;
  onSelect: (id: string | null) => void;
  // Live provider list (inference mode) — for row badge brand colors.
  providers?: ProviderSummary[];
  // When present (inference mode), flows render grouped per connection.
  connections?: ConnectionSummary[];
}

export function FlowList({
  flows,
  selectedId,
  onSelect,
  providers,
  connections,
}: Props) {
  const listRef = useRef<HTMLDivElement>(null);
  const prevCountRef = useRef(flows.length);
  // Grouped view: manual expand/collapse overrides, keyed by group id.
  // Without an override, only the newest group (first in order) is expanded.
  const [expandOverrides, setExpandOverrides] = useState<
    Record<string, boolean>
  >({});

  const groups = useMemo(
    () => (connections ? groupFlows(flows, connections) : []),
    [flows, connections],
  );

  // Auto-scroll to bottom when new flows arrive
  useEffect(() => {
    if (flows.length > prevCountRef.current && listRef.current) {
      listRef.current.scrollTop = listRef.current.scrollHeight;
    }
    prevCountRef.current = flows.length;
  }, [flows.length]);

  if (!connections) {
    // ── Flat list (debugger mode) — unchanged ──
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

  // ── Grouped by connection (inference mode) ──
  if (groups.length === 0) {
    return (
      <div className="flow-list">
        <div className="flow-empty">Waiting for requests...</div>
      </div>
    );
  }

  const newestId = groups[0]?.id;

  return (
    <div className="flow-list" ref={listRef}>
      {groups.map((group) => {
        const expanded = expandOverrides[group.id] ?? group.id === newestId;
        return (
          <div className="conn-group" key={group.id}>
            <ConnectionGroupHeader
              connection={group.connection}
              flowCount={group.flows.length}
              expanded={expanded}
              onToggle={() =>
                setExpandOverrides((prev) => ({
                  ...prev,
                  [group.id]: !expanded,
                }))
              }
              providers={providers}
            />
            {expanded &&
              group.flows.map((flow) => (
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
      })}
    </div>
  );
}
