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

/**
 * Map a pay-side network slug to the pay.sh `?network=...` value.
 * Returns "" for mainnet (no query needed) and `null` for unknown slugs
 * (callers should then skip the receipt link). Mirrors
 * `pay_core::explorer::network_query` on the Rust side.
 */
function payReceiptNetwork(network: string): string | null {
  switch (network) {
    case "mainnet":
    case "mainnet-beta":
      return "";
    case "devnet":
      return "devnet";
    case "testnet":
      return "testnet";
    case "localnet":
    case "surfnet":
      return "sandbox";
    default:
      return null;
  }
}

/**
 * Build a pay.sh receipt URL for a transaction signature on the configured
 * network, e.g. `https://pay.sh/receipt/<sig>?network=sandbox&view=advanced`.
 * Returns `null` when there's no signature or the network is unrecognised.
 */
export function receiptUrl(
  signature: string | null | undefined,
  config: Config | null,
): string | null {
  if (!signature) return null;
  const net = payReceiptNetwork(config?.network ?? "mainnet");
  if (net === null) return null;
  const params = new URLSearchParams();
  if (net) params.set("network", net);
  params.set("view", "advanced");
  return `https://pay.sh/receipt/${signature}?${params.toString()}`;
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
