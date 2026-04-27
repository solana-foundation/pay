import { createContext, useContext, useState, useEffect } from "react";
import type { ReactNode } from "react";

interface Config {
  recipient: string;
  network: string;
  rpcUrl: string;
}

const ConfigContext = createContext<Config | null>(null);

export function ConfigProvider({ children }: { children: ReactNode }) {
  const [config, setConfig] = useState<Config | null>(null);

  useEffect(() => {
    fetch("/__402/pdb/api/config")
      .then((r) => {
        if (!r.ok) throw new Error("not found");
        return r.json();
      })
      .then(setConfig)
      .catch(() => {});
  }, []);

  return (
    <ConfigContext.Provider value={config}>{children}</ConfigContext.Provider>
  );
}

export function useConfig(): Config | null {
  return useContext(ConfigContext);
}

/** Build an explorer URL for an address's token page on the right network. */
export function explorerTokenUrl(
  address: string,
  config: Config | null,
): string {
  const base = `https://explorer.solana.com/address/${address}/tokens`;
  if (!config) return base;
  if (config.network === "devnet") return `${base}?cluster=devnet`;
  if (config.network === "localnet" && config.rpcUrl) {
    return `${base}?cluster=custom&customUrl=${encodeURIComponent(config.rpcUrl)}`;
  }
  return base;
}
