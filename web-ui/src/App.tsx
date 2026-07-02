import { useState, useMemo, useEffect, useRef } from "react";
import { useFlows } from "./hooks/useFlows";
import { useTheme } from "./hooks/useTheme";
import { ConfigProvider, useAppMode, useConfig } from "./hooks/useConfig";
import { Header } from "./components/Header";
import { Toolbar, type FilterMode } from "./components/Toolbar";
import { FlowList } from "./components/FlowList";
import { Sidebar } from "./components/Sidebar";
import { ProviderSidebar } from "./components/ProviderSidebar";

function AppInner() {
  const config = useConfig();
  const appMode = useAppMode();
  const inference = appMode === "inference";
  const { flows, viewerIp, connected, clear, providers } = useFlows(
    config?.providers,
  );
  const { theme, toggle: toggleTheme } = useTheme();
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [mode, setMode] = useState<FilterMode>("all");
  const [search, setSearch] = useState("");
  const [providerFilter, setProviderFilter] = useState<string | null>(null);
  const [sidebarOpen, setSidebarOpen] = useState(() => {
    const stored = localStorage.getItem("sidebarOpen");
    return stored === null ? true : stored === "true";
  });

  // Track whether the user has manually clicked a flow row.
  // Until they do, auto-expand the latest flow as it arrives.
  const userClicked = useRef(false);
  const prevFlowCount = useRef(0);

  const handleSelect = (id: string | null) => {
    userClicked.current = true;
    setSelectedId(id);
  };

  useEffect(() => {
    if (!userClicked.current && flows.length > prevFlowCount.current && flows.length > 0) {
      setSelectedId(flows[flows.length - 1].id);
    }
    prevFlowCount.current = flows.length;
  }, [flows]);

  const filtered = useMemo(() => {
    return flows.filter((f) => {
      if (mode === "mine" && viewerIp && f.clientIp !== viewerIp) return false;
      if (mode === "errors" && f.status !== "failed") return false;
      if (inference && providerFilter && f.inference?.provider !== providerFilter)
        return false;
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
  }, [flows, mode, viewerIp, search, inference, providerFilter]);

  return (
    <div className="app">
      <div className="main">
        <Header
          theme={theme}
          onToggleTheme={toggleTheme}
          sidebarOpen={sidebarOpen}
          onToggleSidebar={() => {
            const next = !sidebarOpen;
            setSidebarOpen(next);
            localStorage.setItem("sidebarOpen", String(next));
          }}
          title={config?.title}
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
          providers={inference ? providers : undefined}
          providerFilter={providerFilter}
          onProviderFilterChange={inference ? setProviderFilter : undefined}
        />
        <FlowList
          flows={filtered}
          selectedId={selectedId}
          onSelect={handleSelect}
          providers={inference ? providers : undefined}
        />
      </div>
      <div className={`sidebar${sidebarOpen ? "" : " collapsed"}`}>
        {inference ? <ProviderSidebar providers={providers} /> : <Sidebar />}
      </div>
    </div>
  );
}

export function App() {
  return (
    <ConfigProvider>
      <AppInner />
    </ConfigProvider>
  );
}
