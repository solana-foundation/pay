import type { Protocol } from "../types";

export function ProtocolBadge({
  protocol,
  scheme,
}: {
  protocol: Protocol;
  scheme?: string;
}) {
  // Render "PROTOCOL:SCHEME" (e.g. MPP:CHARGE, X402:EXACT). Session is an MPP
  // intent, so it belongs to the mpp family → "MPP:SESSION" (the `session` class
  // is kept for its distinct color). Fall back to the bare protocol otherwise.
  const family = protocol === "session" ? "mpp" : protocol;
  const sub = protocol === "session" ? (scheme ?? "session") : scheme;
  const label = sub ? `${family}:${sub}` : family;
  return (
    <span className={`badge ${protocol}`} title={label}>
      {label.toUpperCase()}
    </span>
  );
}
