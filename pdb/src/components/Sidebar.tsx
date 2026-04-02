import { useState, useEffect } from "react";

interface EndpointInfo {
  method: string;
  path: string;
  price: string;
  description: string;
}

interface Config {
  recipient: string;
  network: string;
  rpcUrl: string;
  endpoints: {
    mpp: EndpointInfo[];
    x402: EndpointInfo[];
    oauth: EndpointInfo[];
  };
}

function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);

  const handleCopy = () => {
    navigator.clipboard.writeText(text).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    });
  };

  return (
    <button
      className={`copy-btn${copied ? " copied" : ""}`}
      onClick={handleCopy}
    >
      {copied ? "copied" : "copy"}
    </button>
  );
}

export function Sidebar() {
  const [config, setConfig] = useState<Config | null>(null);

  useEffect(() => {
    fetch("/__debugger/api/config")
      .then((r) => {
        if (!r.ok) throw new Error("not found");
        return r.json();
      })
      .then(setConfig)
      .catch(() => {});
  }, []);

  const baseUrl = `http://${window.location.host}`;
  const firstMetered =
    config?.endpoints.mpp[0] || config?.endpoints.x402[0] || null;

  return (
    <>
      {/* ── Endpoints (top) ── */}
      <div className="sidebar-section">
        <h2 className="mpp">MPP Gated Endpoints</h2>
        {config?.endpoints.mpp.map((ep) => (
          <div className="ep mpp" key={ep.path}>
            <div className="left-ep">
              <span className="m">{ep.method}</span>
              <span className="p">{ep.path}</span>
            </div>
            <span className="pr">{ep.price}</span>
          </div>
        ))}
      </div>
      <div className="sidebar-section">
        <h2 className="x402">x402 Gated Endpoints</h2>
        {config?.endpoints.x402.map((ep) => (
          <div className="ep x4" key={ep.path}>
            <div className="left-ep">
              <span className="m">{ep.method}</span>
              <span className="p">{ep.path}</span>
            </div>
            <span className="pr">{ep.price}</span>
          </div>
        ))}
      </div>

      <div className="sidebar-section">
        <h2 className="mpp">OAuth Gated Endpoints</h2>
        {config?.endpoints.oauth.map((ep) => (
          <div
            className={`ep${ep.price !== "free" ? " mpp" : ""}`}
            key={ep.path}
          >
            <div className="left-ep">
              <span className="m">{ep.method}</span>
              <span className="p">{ep.path}</span>
            </div>
            <span className="pr">{ep.price}</span>
          </div>
        ))}
      </div>

      {config && (
        <div className="meta-list">
          <div className="meta-row">
            <span className="meta-label">Network</span>
            <a
              href="https://402.surfnet.dev"
              target="_blank"
              rel="noopener"
              className="meta-pill"
            >
              {config.network === "localnet" ? "SANDBOX" : config.network.toUpperCase()}
            </a>
          </div>
          <div className="meta-row">
            <span className="meta-label">Recipient</span>
            <a
              href={`https://explorer.solana.com/address/${config.recipient}/tokens?cluster=custom&customUrl=${encodeURIComponent(config.rpcUrl)}`}
              target="_blank"
              rel="noopener"
              className="meta-pill"
            >
              {config.recipient.slice(0, 4)}...{config.recipient.slice(-4)}
            </a>
          </div>
          <div className="meta-row">
            <span className="meta-label">Currency</span>
            <span className="meta-pill static">USDC</span>
          </div>
        </div>
      )}

      {/* ── Getting Started (pushed to bottom) ── */}
      <div className="getting-started">
        <h2>Getting started</h2>

        <div className="gs-step">
          <span className="gs-num">1</span>
          <div className="gs-content">
            <p className="gs-label">Install the CLI</p>
            <div className="code-block">
              <pre>brew install pay</pre>
              <CopyButton text="brew install pay" />
            </div>
          </div>
        </div>

        {firstMetered && (
          <div className="gs-step">
            <span className="gs-num">2</span>
            <div className="gs-content">
              <p className="gs-label">Try a gated endpoint</p>
              <div className="code-block">
                {(() => {
                  const methodFlag = firstMetered.method !== "GET" ? `-X ${firstMetered.method} ` : "";
                  const cmd = `pay --dev curl ${methodFlag}${baseUrl}/${firstMetered.path}`;
                  const display = `pay --dev curl ${methodFlag}\\\n  ${baseUrl}/${firstMetered.path}`;
                  return (
                    <>
                      <pre>{display}</pre>
                      <CopyButton text={cmd} />
                    </>
                  );
                })()}
              </div>
            </div>
          </div>
        )}

        <a
          href="https://402.surfnet.dev"
          target="_blank"
          rel="noopener"
          className="btn-stablecoins"
        >
          Top-up developer account
        </a>
      </div>
    </>
  );
}
