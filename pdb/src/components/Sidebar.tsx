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

  const baseUrl = `${window.location.protocol}//${window.location.host}`;
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
            <a
              href={`${baseUrl}/${ep.path.startsWith("/") ? ep.path.slice(1) : ep.path}`}
              target="_blank"
              rel="noopener"
              className="ep-link"
              title="Open in new tab"
            >
              <svg width="12" height="12" viewBox="0 0 20 20" fill="currentColor">
                <path fillRule="evenodd" d="M4.25 5.5a.75.75 0 00-.75.75v8.5c0 .414.336.75.75.75h8.5a.75.75 0 00.75-.75v-4a.75.75 0 011.5 0v4A2.25 2.25 0 0112.75 17h-8.5A2.25 2.25 0 012 14.75v-8.5A2.25 2.25 0 014.25 4h5a.75.75 0 010 1.5h-5z" clipRule="evenodd" />
                <path fillRule="evenodd" d="M6.194 12.753a.75.75 0 001.06.053L16.5 4.44v2.81a.75.75 0 001.5 0v-4.5a.75.75 0 00-.75-.75h-4.5a.75.75 0 000 1.5h2.553l-9.056 8.194a.75.75 0 00-.053 1.06z" clipRule="evenodd" />
              </svg>
            </a>
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
                  const path = firstMetered.path.startsWith("/") ? firstMetered.path : `/${firstMetered.path}`;
                  const cmd = `pay --sandbox curl ${methodFlag}${baseUrl}${path}`;
                  const display = `pay --sandbox curl ${methodFlag}\\\n  ${baseUrl}${path}`;
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
