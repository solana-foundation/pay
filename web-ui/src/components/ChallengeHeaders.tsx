import { useState } from "react";

const CHALLENGE_HEADER_ORDER = [
  "payment-required",
  "x-payment-required",
  "www-authenticate",
] as const;

interface ChallengeFact {
  readonly label: string;
  readonly value: string;
}

interface ChallengeSection {
  readonly title: string;
  readonly facts: ChallengeFact[];
}

/** A debugger-safe projection of one decoded HTTP 402 challenge header. */
export interface DecodedChallengeHeader {
  readonly name: string;
  readonly protocol: "MPP" | "x402";
  readonly rawValue: string;
  readonly sections: ChallengeSection[];
  readonly decodeError?: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function decodeBase64(value: string): string | null {
  try {
    const normalized = value.replace(/-/g, "+").replace(/_/g, "/");
    const padded = normalized + "=".repeat((4 - (normalized.length % 4)) % 4);
    const binary = atob(padded);
    const bytes = Uint8Array.from(binary, (character) => character.charCodeAt(0));
    return new TextDecoder().decode(bytes);
  } catch {
    return null;
  }
}

function decodeJsonValue(value: string): unknown | null {
  try {
    return JSON.parse(value) as unknown;
  } catch {
    const decoded = decodeBase64(value);
    if (!decoded) return null;
    try {
      return JSON.parse(decoded) as unknown;
    } catch {
      return null;
    }
  }
}

function humanize(segment: string): string {
  return segment
    .replace(/([a-z0-9])([A-Z])/g, "$1 $2")
    .replace(/[_-]+/g, " ")
    .replace(/^./, (character) => character.toUpperCase());
}

function pathLabel(path: string[]): string {
  return path.map(humanize).join(" · ");
}

function numericString(value: unknown): string | null {
  if (typeof value === "number" && Number.isFinite(value)) return String(value);
  if (typeof value === "string" && /^\d+$/.test(value)) return value;
  return null;
}

function displayAmount(
  key: string,
  value: unknown,
  container: Record<string, unknown>,
): string | null {
  const amountKeys = new Set([
    "amount",
    "cap",
    "suggestedDeposit",
    "minVoucherDelta",
    "balance",
    "totalClaimed",
    "chargedCumulativeAmount",
  ]);
  if (!amountKeys.has(key)) return null;
  const raw = numericString(value);
  if (!raw) return null;
  if (raw === "18446744073709551615") return "unbounded";
  const details = isRecord(container.methodDetails) ? container.methodDetails : undefined;
  const extra = isRecord(container.extra) ? container.extra : undefined;
  const decimalsValue = container.decimals ?? details?.decimals ?? extra?.decimals ?? 6;
  const decimals = Number(decimalsValue);
  if (!Number.isInteger(decimals) || decimals < 0 || decimals > 18) return raw;
  try {
    const divisor = 10n ** BigInt(decimals);
    const baseUnits = BigInt(raw);
    const whole = baseUnits / divisor;
    const fractional = (baseUnits % divisor)
      .toString()
      .padStart(decimals, "0")
      .replace(/0+$/, "");
    const display = fractional ? `${whole}.${fractional}` : whole.toString();
    const asset = container.currency ?? container.asset;
    const unit = typeof asset === "string" && asset.length <= 12 ? ` ${asset}` : " tokens";
    return `${display}${unit} (${raw} base units)`;
  } catch {
    return raw;
  }
}

function primitiveValue(value: unknown): string | null {
  if (typeof value === "string") return value;
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  if (value === null) return "null";
  return null;
}

function flattenFacts(
  value: unknown,
  path: string[] = [],
  facts: ChallengeFact[] = [],
  parent?: Record<string, unknown>,
): ChallengeFact[] {
  if (facts.length >= 100) return facts;
  const primitive = primitiveValue(value);
  if (primitive != null) {
    const key = path.at(-1) ?? "value";
    facts.push({
      label: pathLabel(path),
      value: parent ? (displayAmount(key, value, parent) ?? primitive) : primitive,
    });
    return facts;
  }
  if (Array.isArray(value)) {
    if (value.every((item) => primitiveValue(item) != null)) {
      facts.push({
        label: pathLabel(path),
        value: value.map(primitiveValue).join(", "),
      });
      return facts;
    }
    value.forEach((item, index) =>
      flattenFacts(item, [...path, String(index + 1)], facts),
    );
    return facts;
  }
  if (isRecord(value)) {
    for (const [key, nested] of Object.entries(value)) {
      flattenFacts(nested, [...path, key], facts, value);
    }
  }
  return facts;
}

function parseAuthParameters(value: string): Record<string, string> {
  const parameters: Record<string, string> = {};
  const matcher = /([A-Za-z][\w-]*)=(?:"([^"]*)"|([^,\s]+))/g;
  for (const match of value.matchAll(matcher)) {
    parameters[match[1]] = match[2] ?? match[3] ?? "";
  }
  return parameters;
}

function decodeMppHeader(name: string, rawValue: string): DecodedChallengeHeader {
  const parameters = parseAuthParameters(rawValue);
  const handshake = Object.fromEntries(
    Object.entries(parameters).filter(([key]) => key !== "request"),
  );
  const sections: ChallengeSection[] = [
    { title: "Challenge", facts: flattenFacts(handshake) },
  ];
  const request = parameters.request ? decodeJsonValue(parameters.request) : null;
  if (request != null) {
    sections.push({ title: "Decoded request", facts: flattenFacts(request) });
  }
  return {
    name,
    protocol: "MPP",
    rawValue,
    sections: sections.filter((section) => section.facts.length > 0),
    decodeError:
      parameters.request && request == null
        ? "The embedded request token could not be decoded."
        : undefined,
  };
}

function decodeX402Header(name: string, rawValue: string): DecodedChallengeHeader {
  const decoded = decodeJsonValue(rawValue);
  if (!isRecord(decoded)) {
    return {
      name,
      protocol: "x402",
      rawValue,
      sections: [],
      decodeError: "The challenge envelope could not be decoded as JSON or base64 JSON.",
    };
  }

  const offersValue = decoded.accepts ?? decoded.offers;
  const offers = Array.isArray(offersValue) ? offersValue : [];
  const envelope = Object.fromEntries(
    Object.entries(decoded).filter(
      ([key]) => !["accepts", "offers", "extensions"].includes(key),
    ),
  );
  const sections: ChallengeSection[] = [];
  const envelopeFacts = flattenFacts(envelope);
  if (envelopeFacts.length > 0) {
    sections.push({ title: offers.length > 0 ? "Envelope" : "Requirements", facts: envelopeFacts });
  }
  offers.forEach((offer, index) => {
    sections.push({ title: `Offer ${index + 1}`, facts: flattenFacts(offer) });
  });
  if (decoded.extensions != null) {
    sections.push({ title: "Extensions", facts: flattenFacts(decoded.extensions) });
  }

  return { name, protocol: "x402", rawValue, sections };
}

/** Decode one supported HTTP 402 challenge header without trusting its contents. */
export function decodeChallengeHeader(
  name: string,
  rawValue: string,
): DecodedChallengeHeader {
  return name.toLowerCase() === "www-authenticate"
    ? decodeMppHeader(name, rawValue)
    : decodeX402Header(name, rawValue);
}

function middleTruncate(value: string): string {
  if (value.length <= 32) return value;
  return `${value.slice(0, 16)}…${value.slice(-16)}`;
}

function CopyRawButton({ value, name }: { value: string; name: string }) {
  const [copied, setCopied] = useState(false);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(value);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      setCopied(false);
    }
  };
  return (
    <button
      className={`challenge-copy${copied ? " copied" : ""}`}
      type="button"
      aria-label={`Copy raw ${name} header`}
      onClick={copy}
    >
      {copied ? "Copied" : "Copy"}
    </button>
  );
}

/** Render decoded payment challenge headers with their raw values available for copying. */
export function ChallengeHeaders({
  headers,
}: {
  headers?: Record<string, string>;
}) {
  const normalized = new Map(
    Object.entries(headers ?? {}).map(([name, value]) => [name.toLowerCase(), { name, value }]),
  );
  const challenges = CHALLENGE_HEADER_ORDER.flatMap((name) => {
    const header = normalized.get(name);
    return header ? [decodeChallengeHeader(header.name, header.value)] : [];
  });

  if (challenges.length === 0) {
    return <p className="inspector-empty">No payment challenge was captured.</p>;
  }

  return (
    <div className="challenge-list">
      {challenges.map((challenge) => (
        <section className="challenge-card" key={challenge.name.toLowerCase()}>
          <div className="challenge-card-header">
            <code>{challenge.name}</code>
            <span className="challenge-protocol">{challenge.protocol}</span>
          </div>
          {challenge.sections.map((section) => (
            <div className="challenge-section" key={section.title}>
              <h5>{section.title}</h5>
              <dl className="challenge-facts">
                {section.facts.map((fact, index) => (
                  <div className="challenge-fact" key={`${fact.label}-${index}`}>
                    <dt>{fact.label}</dt>
                    <dd title={fact.value}>{fact.value}</dd>
                  </div>
                ))}
              </dl>
            </div>
          ))}
          {challenge.decodeError && (
            <p className="challenge-error">{challenge.decodeError}</p>
          )}
          <div className="challenge-raw">
            <span>Raw</span>
            <code title={challenge.rawValue}>{middleTruncate(challenge.rawValue)}</code>
            <CopyRawButton value={challenge.rawValue} name={challenge.name} />
          </div>
        </section>
      ))}
    </div>
  );
}
