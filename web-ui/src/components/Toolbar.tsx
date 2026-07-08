type FilterMode = "all" | "mine" | "errors";

interface Props {
  inference?: boolean;
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
  inference = false,
  mode,
  onModeChange,
  search,
  onSearchChange,
  count,
  total,
  onClear,
  connected,
}: Props) {
  const unit = inference ? "connections" : "flows";
  return (
    <div className="toolbar">
      <h2>{inference ? "Connections" : "Flows"}</h2>
      <span className="count">
        {connected
          ? `${count} / ${total} ${unit}`
          : "Disconnected. Retrying..."}
      </span>
      <input
        className="filter"
        placeholder={inference ? "Filter by model..." : "Filter by path..."}
        value={search}
        onChange={(e) => onSearchChange(e.target.value)}
      />
      <span className="spacer" />
      {!inference && (
        <>
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
        </>
      )}
    </div>
  );
}

export type { FilterMode };
