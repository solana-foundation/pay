import type { ProviderSummary } from "../types";

type FilterMode = "all" | "mine" | "errors";

interface Props {
  mode: FilterMode;
  onModeChange: (mode: FilterMode) => void;
  search: string;
  onSearchChange: (search: string) => void;
  count: number;
  total: number;
  onClear: () => void;
  connected: boolean;
  // Per-provider filter (inference mode only; pills hidden when absent)
  providers?: ProviderSummary[];
  providerFilter?: string | null;
  onProviderFilterChange?: (slug: string | null) => void;
}

export function Toolbar({
  mode,
  onModeChange,
  search,
  onSearchChange,
  count,
  total,
  onClear,
  connected,
  providers,
  providerFilter,
  onProviderFilterChange,
}: Props) {
  return (
    <div className="toolbar">
      <h2>Flows</h2>
      <span className="count">
        {connected
          ? `${count} / ${total} flows`
          : "Disconnected. Retrying..."}
      </span>
      <input
        className="filter"
        placeholder="Filter by path..."
        value={search}
        onChange={(e) => onSearchChange(e.target.value)}
      />
      <span className="spacer" />
      {providers && providers.length > 0 && onProviderFilterChange && (
        <span className="provider-pills">
          {providers.map((p) => (
            <button
              key={p.slug}
              className={providerFilter === p.slug ? "active" : ""}
              onClick={() =>
                onProviderFilterChange(
                  providerFilter === p.slug ? null : p.slug,
                )
              }
              title={p.title}
            >
              {p.slug}
            </button>
          ))}
        </span>
      )}
      <button
        className={mode === "mine" ? "active" : ""}
        onClick={() => onModeChange("mine")}
      >
        This device
      </button>
      <button
        className={mode === "errors" ? "active" : ""}
        onClick={() => onModeChange("errors")}
      >
        Errors
      </button>
      <button
        className={mode === "all" ? "active" : ""}
        onClick={() => onModeChange("all")}
      >
        All
      </button>
      <button onClick={onClear}>Clear</button>
    </div>
  );
}

export type { FilterMode };
