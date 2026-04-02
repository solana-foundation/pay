import { useState, useMemo } from "react";
import { useFlows } from "./hooks/useFlows";
import { useTheme } from "./hooks/useTheme";
import { Header } from "./components/Header";
import { Toolbar, type FilterMode } from "./components/Toolbar";
import { FlowList } from "./components/FlowList";
import { Sidebar } from "./components/Sidebar";

export function App() {
  const { flows, viewerIp, connected, clear } = useFlows();
  const { theme, toggle: toggleTheme } = useTheme();
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [mode, setMode] = useState<FilterMode>("all");
  const [search, setSearch] = useState("");
  const [sidebarOpen, setSidebarOpen] = useState(true);

  const filtered = useMemo(() => {
    return flows.filter((f) => {
      if (mode === "mine" && viewerIp && f.clientIp !== viewerIp) return false;
      if (mode === "errors" && f.status !== "failed") return false;
      if (search) {
        const q = search.toLowerCase();
        if (
          !f.resource.toLowerCase().includes(q) &&
          !f.protocol.toLowerCase().includes(q)
        )
          return false;
      }
      return true;
    });
  }, [flows, mode, viewerIp, search]);

  return (
    <div className="app">
      <div className="main">
        <Header
          theme={theme}
          onToggleTheme={toggleTheme}
          sidebarOpen={sidebarOpen}
          onToggleSidebar={() => setSidebarOpen(!sidebarOpen)}
        />
        <Toolbar
          mode={mode}
          onModeChange={setMode}
          search={search}
          onSearchChange={setSearch}
          count={filtered.length}
          total={flows.length}
          onClear={clear}
          connected={connected}
        />
        <FlowList
          flows={filtered}
          selectedId={selectedId}
          onSelect={setSelectedId}
        />
      </div>
      <div className={`sidebar${sidebarOpen ? "" : " collapsed"}`}>
        <Sidebar />
      </div>
    </div>
  );
}
