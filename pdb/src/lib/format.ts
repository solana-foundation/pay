const U64_MAX = "18446744073709551615";
const USDC_MAINNET_MINT = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

export function shortAddr(value: string | undefined): string {
  if (!value) return "pending";
  if (value.length <= 14) return value;
  return `${value.slice(0, 5)}...${value.slice(-5)}`;
}

export function currencyLabel(currency: string | undefined): string {
  if (!currency || currency === USDC_MAINNET_MINT) return "USDC";
  return currency.length > 12 ? shortAddr(currency) : currency;
}

export function formatUnits(
  raw: string | undefined,
  decimals = 6,
  currency = "USDC",
  missing = "pending",
): string {
  const label = currencyLabel(currency);
  if (!raw) return missing;
  if (raw === U64_MAX) return "unbounded";
  if (!/^\d+$/.test(raw)) return raw;

  try {
    const value = BigInt(raw);
    const divisor = 10n ** BigInt(decimals);
    const whole = value / divisor;
    const fraction = value % divisor;
    if (fraction === 0n) return `${whole.toString()} ${label}`;

    const fractionText = fraction
      .toString()
      .padStart(decimals, "0")
      .replace(/0+$/, "");
    return `${whole.toString()}.${fractionText} ${label}`;
  } catch {
    return `${raw} ${label}`;
  }
}
