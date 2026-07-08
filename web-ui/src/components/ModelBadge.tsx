import type { ProviderSummary } from "../types";
import { isHexColor, providerColor } from "../lib/inference";

interface Props {
  // Model id shown prominently; falls back to the provider slug when absent
  // (e.g. non-inference passthrough like /api/tags).
  model?: string;
  provider: string; // slug — supplies the brand color (model runs on it)
  providers?: ProviderSummary[];
}

/**
 * Primary badge for inference traffic. The model is the star; it borrows
 * the provider's brand color since the model runs on that provider.
 */
export function ModelBadge({ model, provider, providers }: Props) {
  const color = providerColor(provider, providers);
  const style = isHexColor(color)
    ? { color, background: `${color}22` }
    : undefined;
  return (
    <span
      className={`badge inference${model ? " model" : ""}`}
      style={style}
      title={model ? `${model} · ${provider}` : provider}
    >
      {model ?? provider.toUpperCase()}
    </span>
  );
}
