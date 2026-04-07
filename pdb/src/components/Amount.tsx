/**
 * Shared amount display component.
 *
 * Formats a numeric USD amount as "$X.XX" with trailing zeros stripped,
 * rendered in a consistent pill style (monospace, subtle background,
 * rounded corners).
 */

/** Format a number as a clean dollar string: $100, $49.99, $0.01 */
export function formatUsd(value: number): string {
  if (value === 0) return "$0";
  // Use up to 4 decimals, then strip trailing zeros
  const fixed = value.toFixed(4);
  const trimmed = fixed.replace(/\.?0+$/, "");
  return `$${trimmed}`;
}

interface Props {
  /** Amount in USD (float). */
  value: number;
  /** Optional extra CSS class. */
  className?: string;
}

export function Amount({ value, className }: Props) {
  return (
    <span className={`amount-pill${className ? ` ${className}` : ""}`}>
      {formatUsd(value)}
    </span>
  );
}
