import { useState, useMemo, useEffect, useRef } from "react";
import { useFlows } from "./hooks/useFlows";
import { useTheme } from "./hooks/useTheme";
import { ConfigProvider, useAppMode, useConfig } from "./hooks/useConfig";
import { Header } from "./components/Header";
import { Toolbar, type FilterMode, type ViewMode } from "./components/Toolbar";
import { FlowList } from "./components/FlowList";
import { ChannelList } from "./components/ChannelList";
import { Sidebar } from "./components/Sidebar";
import { ProviderSidebar } from "./components/ProviderSidebar";
import { paymentChannels, scopedPaymentChannels } from "./lib/channels";

function AppInner() {
  const config = useConfig();
  const appMode = useAppMode();
  const inference = appMode === "inference";
  const { flows, viewerIp, connected, clear, providers, connections } =
    useFlows(config?.providers);
  const { theme, toggle: toggleTheme } = useTheme();
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [mode, setMode] = useState<FilterMode>("all");
  const [search, setSearch] = useState("");
  const [viewMode, setViewMode] = useState<ViewMode>("flows");
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

  const scopedFlows = useMemo(() => {
    return flows.filter((f) => {
      if (mode === "mine" && viewerIp && f.clientIp !== viewerIp) return false;
      if (mode === "errors" && f.status !== "failed") return false;
      return true;
    });
  }, [flows, mode, viewerIp]);

  const filtered = useMemo(() => {
    const q = search.trim().toLowerCase();
    return scopedFlows.filter((f) => {
      if (inference) {
        if (!q) return true;
        return f.inference?.model?.toLowerCase().includes(q) ?? false;
      }
      if (q) {
        if (
          !f.resource.toLowerCase().includes(q) &&
          !f.protocol.toLowerCase().includes(q)
        )
          return false;
      }
      return true;
    });
  }, [scopedFlows, search, inference]);

  const allChannels = useMemo(() => paymentChannels(flows), [flows]);
  const visibleChannels = useMemo(
    () => scopedPaymentChannels(allChannels, scopedFlows, search),
    [allChannels, scopedFlows, search],
  );

  // Inference mode groups by connection and filters that grouped list by
  // model name only. undefined outside inference mode keeps FlowList flat.
  const visibleConnections = useMemo(() => {
    if (!inference || viewMode === "channels") return undefined;
    const q = search.trim().toLowerCase();
    if (!q) return connections;
    return connections.filter((c) =>
      c.models?.some((model) => model.toLowerCase().includes(q)),
    );
  }, [inference, connections, search, viewMode]);

  const toolbarCount =
    viewMode === "channels"
      ? visibleChannels.length
      : inference
        ? (visibleConnections?.length ?? 0)
        : filtered.length;
  const toolbarTotal =
    viewMode === "channels"
      ? allChannels.length
      : inference
        ? connections.length
        : flows.length;

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
          inference={inference}
          mode={mode}
          onModeChange={setMode}
          search={search}
          onSearchChange={setSearch}
          count={toolbarCount}
          total={toolbarTotal}
          onClear={clear}
          connected={connected}
          viewMode={viewMode}
          onViewModeChange={setViewMode}
        />
        {viewMode === "channels" ? (
          <ChannelList
            channels={visibleChannels}
            selectedId={selectedId}
            onSelect={handleSelect}
            providers={inference ? providers : undefined}
          />
        ) : (
          <FlowList
            flows={filtered}
            selectedId={selectedId}
            onSelect={handleSelect}
            providers={inference ? providers : undefined}
            connections={visibleConnections}
          />
        )}
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
