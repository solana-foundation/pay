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
