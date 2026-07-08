import type { ConnectionSummary, ProviderSummary } from "../types";
import { ModelBadge } from "./ModelBadge";
import {
  formatPaidUsd,
  formatTokenCount,
  shortPayer,
} from "../lib/connections";

interface Props {
  connection: ConnectionSummary | null; // null = trailing "other" group
  flowCount: number;
  expanded: boolean;
  onToggle: () => void;
  providers?: ProviderSummary[];
}

export function ConnectionGroupHeader({
  connection,
  flowCount,
  expanded,
  onToggle,
  providers,
}: Props) {
  const caret = (
    <span className={`conn-caret${expanded ? " open" : ""}`} aria-hidden="true">
      <svg width="10" height="10" viewBox="0 0 10 10" fill="none">
        <path
          d="M3.5 2L7 5L3.5 8"
          stroke="currentColor"
          strokeWidth="1.5"
          strokeLinecap="round"
          strokeLinejoin="round"
        />
      </svg>
    </span>
  );

  if (!connection) {
    // Unmatched flows (no connection shares payer or client IP).
    return (
      <button className="conn-header" onClick={onToggle}>
        {caret}
        <span className="conn-who other">other</span>
        <span className="conn-fill" />
        <span className="conn-reqs">
          {flowCount} flow{flowCount === 1 ? "" : "s"}
        </span>
      </button>
    );
  }

  const who = connection.payer
    ? shortPayer(connection.payer)
    : connection.clientIp;

  return (
    <button className="conn-header" onClick={onToggle}>
      {caret}
      <span className="conn-who" title={connection.payer ?? connection.clientIp}>
        {who}
      </span>
      {/* Models are primary (badged, brand-colored); provider is a dim label. */}
      {connection.models && connection.models.length > 0 ? (
        <span className="conn-model-badges">
          {connection.models.map((model) => (
            <ModelBadge
              key={model}
              model={model}
              provider={connection.provider ?? ""}
              providers={providers}
            />
          ))}
        </span>
      ) : (
        connection.provider && (
          <ModelBadge provider={connection.provider} providers={providers} />
        )
      )}
      {connection.models &&
        connection.models.length > 0 &&
        connection.provider && (
          <span className="provider-label" title={connection.provider}>
            {connection.provider}
          </span>
        )}
      <span className="conn-fill" />
      <span className="conn-reqs" title="ok / total requests">
        {connection.ok}/{connection.requests}
        {connection.failed > 0 && (
          <span className="conn-failed"> · {connection.failed} failed</span>
        )}
      </span>
      <span className="conn-tokens" title="prompt ↓ / completion ↑ tokens">
        ↓ {formatTokenCount(connection.tokensPrompt)} · ↑{" "}
        {formatTokenCount(connection.tokensCompletion)}
      </span>
      <span className="conn-paid">{formatPaidUsd(connection.paidUsd)}</span>
    </button>
  );
}
