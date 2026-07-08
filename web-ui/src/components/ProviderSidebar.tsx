import { useState } from "react";
import type { ProviderSummary } from "../types";
import { isHexColor, portFromBaseUrl } from "../lib/inference";

interface Props {
  providers: ProviderSummary[];
}

function ProviderCard({ provider }: { provider: ProviderSummary }) {
  const [collapsed, setCollapsed] = useState(false);
  const port = portFromBaseUrl(provider.baseUrl);
  const dotStyle =
    provider.up && isHexColor(provider.color)
      ? { background: provider.color }
      : undefined;
  const expandable = provider.up && provider.models.length > 0;
  const expanded = expandable && !collapsed;
  const pricingByModel = new Map(
    (provider.modelPricing ?? []).map((item) => [item.model, item]),
  );

  return (
    <div className={`provider-card${provider.up ? "" : " down"}`}>
      <button
        className="provider-head"
        onClick={() => setCollapsed((c) => !c)}
        disabled={!expandable}
        title={provider.baseUrl}
      >
        <span
          className={`provider-dot ${provider.up ? "up" : "down"}`}
          style={dotStyle}
        />
        <span className="provider-title">{provider.title}</span>
        {provider.up ? (
          <span className="provider-meta">
            {port && <span className="provider-port">:{port}</span>}
            {" · "}
            {provider.models.length} model
            {provider.models.length === 1 ? "" : "s"}
          </span>
        ) : (
          <span className="provider-meta">not detected</span>
        )}
      </button>
      {expanded && (
        <div className="provider-models">
          {provider.models.map((model) => {
            const pricing = pricingByModel.get(model);
            const variantDescription = pricing?.description;
            const title = [model, pricing?.price, variantDescription]
              .filter(Boolean)
              .join(" · ");
            return (
              <div className="provider-model" key={model} title={title}>
                <div className="provider-model-row">
                  <span className="provider-model-name">{model}</span>
                  <span className="provider-model-price">
                    {pricing?.price ?? "unpriced"}
                  </span>
                </div>
                {variantDescription && (
                  <div className="provider-model-description">
                    {variantDescription}
                  </div>
                )}
              </div>
            );
          })}
        </div>
      )}
      {provider.up && provider.version && (
        <div className="provider-version">v{provider.version}</div>
      )}
    </div>
  );
}

export function ProviderSidebar({ providers }: Props) {
  return (
    <div className="sidebar-section">
      <h2 className="providers">Providers</h2>
      {providers.length === 0 && (
        <div className="provider-empty">No providers detected</div>
      )}
      {providers.map((provider) => (
        <ProviderCard key={provider.slug} provider={provider} />
      ))}
    </div>
  );
}
