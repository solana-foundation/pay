type FilterMode = "all" | "mine" | "errors";
export type ViewMode = "flows" | "channels";

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
  viewMode: ViewMode;
  onViewModeChange: (view: ViewMode) => void;
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
  viewMode,
  onViewModeChange,
}: Props) {
  const unit = viewMode === "channels" ? "channels" : inference ? "connections" : "flows";
  return (
    <div className="toolbar">
      <div className="view-toggle" role="group" aria-label="Debugger view">
        <button
          className={viewMode === "flows" ? "active" : ""}
          type="button"
          aria-pressed={viewMode === "flows"}
          onClick={() => onViewModeChange("flows")}
        >
          {inference ? "Connections" : "Flows"}
        </button>
        <button
          className={viewMode === "channels" ? "active" : ""}
          type="button"
          aria-pressed={viewMode === "channels"}
          onClick={() => onViewModeChange("channels")}
        >
          Channels
        </button>
      </div>
      <span className="count">
        {connected
          ? `${count} / ${total} ${unit}`
          : "Disconnected. Retrying..."}
      </span>
      <input
        className="filter"
        placeholder={
          viewMode === "channels"
            ? "Filter channels..."
            : inference
              ? "Filter by model..."
              : "Filter by path..."
        }
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
