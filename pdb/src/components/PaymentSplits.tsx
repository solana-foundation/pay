import { useRef, useEffect, useState } from "react";
import type { PaymentFlow } from "../types";
import { Amount, formatUsd } from "./Amount";
import { useConfig, explorerTokenUrl } from "../hooks/useConfig";

function shortAddr(addr: string | undefined): string {
  if (!addr) return "";
  if (addr.length <= 12) return addr;
  return `${addr.slice(0, 4)}…${addr.slice(-4)}`;
}

function base64urlDecode(b64: string): string {
  try {
    const normalized = b64.replace(/-/g, "+").replace(/_/g, "/");
    const padded = normalized + "=".repeat((4 - (normalized.length % 4)) % 4);
    return atob(padded);
  } catch {
    return "";
  }
}

interface Recipient {
  label: string;
  address: string;
  amount: number;
  memo?: string;
}

interface ParsedChallenge {
  totalAmount: number;
  recipients: Recipient[];
  feePayerKey?: string;
}

function parseChallenge(flow: PaymentFlow): ParsedChallenge | null {
  const wwwAuth = flow.challengeHeaders?.["www-authenticate"];
  if (!wwwAuth) return null;
  const match = wwwAuth.match(/request="([^"]+)"/);
  if (!match) return null;
  const decoded = base64urlDecode(match[1]);
  if (!decoded) return null;

  try {
    const req = JSON.parse(decoded);
    const decimals = req.methodDetails?.decimals ?? 6;
    const divisor = Math.pow(10, decimals);
    const rawTotal = parseInt(req.amount ?? "0", 10);
    const rawSplits = (req.methodDetails?.splits ?? []) as Array<{
      recipient: string;
      amount: string;
      label?: string;
      memo?: string;
    }>;
    const splitsTotal = rawSplits.reduce(
      (sum, s) => sum + parseInt(s.amount, 10),
      0,
    );

    const recipients: Recipient[] = [
      {
        label: rawSplits.length > 0 ? "Primary" : "Recipient",
        address: req.recipient,
        amount: (rawTotal - splitsTotal) / divisor,
      },
      ...rawSplits.map((s) => ({
        label: s.label || shortAddr(s.recipient),
        address: s.recipient,
        amount: parseInt(s.amount, 10) / divisor,
        memo: s.memo,
      })),
    ];

    return {
      totalAmount: rawTotal / divisor,
      recipients,
      feePayerKey: req.methodDetails?.feePayerKey,
    };
  } catch {
    return null;
  }
}

const COLORS = [
  "#58a6ff",
  "#a371f7",
  "#39d2e0",
  "#3fb950",
  "#d29922",
  "#db6d28",
  "#f85149",
];

const GRAYS = [
  "#6e7681",
  "#848d97",
  "#8b949e",
  "#6e7681",
  "#848d97",
  "#8b949e",
  "#6e7681",
];

type RibbonData = {
  recipient: Recipient;
  thickness: number;
  srcTop: number;
  srcBottom: number;
  destTop: number;
  destBottom: number;
  color: string;
};

function renderSplitsSvg(
  ribbons: RibbonData[],
  recipBars: { top: number; center: number }[],
  palette: string[],
  BAR_W: number,
  SVG_W: number,
  SENDER_BAR_H: number,
  RECIP_BAR_H: number,
  TOTAL_H: number,
  BRANCH_X_FRAC: number,
  prefix: string,
) {
  return (
    <svg className="splits-svg" width={SVG_W} height={TOTAL_H} viewBox={`0 0 ${SVG_W} ${TOTAL_H}`}>
      <defs>
        {ribbons.map((_, i) => (
          <linearGradient key={i} id={`rib-${prefix}-${i}`} x1="0" x2="1">
            <stop offset="0%" stopColor={palette[i % palette.length]} stopOpacity={prefix === "gray" ? "0.35" : "0.45"} />
            <stop offset="50%" stopColor={palette[i % palette.length]} stopOpacity={prefix === "gray" ? "0.18" : "0.25"} />
            <stop offset="100%" stopColor={palette[i % palette.length]} stopOpacity={prefix === "gray" ? "0.35" : "0.45"} />
          </linearGradient>
        ))}
      </defs>
      {ribbons.map((rib, i) => {
        const x0 = BAR_W;
        const x1 = SVG_W - BAR_W;
        const branchX = x0 + (x1 - x0) * BRANCH_X_FRAC;
        const cx = branchX + (x1 - branchX) * 0.5;
        const d = `M ${x0} ${rib.srcTop} L ${branchX} ${rib.srcTop} C ${cx} ${rib.srcTop}, ${cx} ${rib.destTop}, ${x1} ${rib.destTop} L ${x1} ${rib.destBottom} C ${cx} ${rib.destBottom}, ${cx} ${rib.srcBottom}, ${branchX} ${rib.srcBottom} L ${x0} ${rib.srcBottom} Z`;
        return <path key={i} d={d} fill={`url(#rib-${prefix}-${i})`} className="splits-ribbon" />;
      })}
      <rect x={0} y={0} width={BAR_W} height={SENDER_BAR_H} fill={prefix === "gray" ? "var(--fg-muted)" : "var(--accent)"} />
      {recipBars.map((bar, i) => (
        <rect key={i} x={SVG_W - BAR_W} y={bar.top} width={BAR_W} height={RECIP_BAR_H} fill={palette[i % palette.length]} />
      ))}
    </svg>
  );
}

export function PaymentSplits({ flow, success }: { flow: PaymentFlow; success: boolean }) {
  // Track transition from non-success → success to trigger animation
  const prevSuccess = useRef(success);
  const [animating, setAnimating] = useState(false);
  // Bump key to force SVG re-mount (restarts CSS animation)
  const [animKey, setAnimKey] = useState(0);

  useEffect(() => {
    if (success && !prevSuccess.current) {
      setAnimating(true);
      setAnimKey((k) => k + 1);
      const timer = setTimeout(() => setAnimating(false), 600);
      prevSuccess.current = true;
      return () => clearTimeout(timer);
    }
    prevSuccess.current = success;
  }, [success]);
  const config = useConfig();
  const parsed = parseChallenge(flow);

  if (!parsed) {
    return (
      <div className="splits">
        <h3>Payment Splits</h3>
        <div className="splits-empty">No challenge data to parse</div>
      </div>
    );
  }

  const N = parsed.recipients.length;
  const total = parsed.totalAmount;

  // Dimensions
  const RECIP_BAR_H = 56;
  const BAR_W = 4;
  const GAP = 2;
  const SVG_W = 280;
  const BRANCH_X_FRAC = 1 / 3;

  // 1) Compute thickness for each flow
  const flows = parsed.recipients.map((r) => {
    const pct = r.amount / total;
    const thickness = pct >= 0.5 ? RECIP_BAR_H : pct >= 0.25 ? RECIP_BAR_H / 2 : pct >= 0.1 ? RECIP_BAR_H / 3 : RECIP_BAR_H / 6;
    return { recipient: r, thickness };
  });

  // 2) Sender bar height = sum of all thicknesses + gaps
  const SENDER_BAR_H = flows.reduce((s, f) => s + f.thickness, 0) + GAP * (N - 1);

  // 3) Stack flows on the sender bar
  let stackY = 0;
  const ribbons = flows.map((f, i) => {
    const srcTop = stackY;
    const srcBottom = stackY + f.thickness;
    stackY += f.thickness + GAP;

    const destTop = i * (RECIP_BAR_H + 2);
    const destBottom = destTop + RECIP_BAR_H;

    const palette = success || animating ? COLORS : GRAYS;
    return {
      ...f,
      srcTop,
      srcBottom,
      destTop,
      destBottom,
      color: palette[i % palette.length],
    };
  });

  const RECIP_GAP = 2;
  const TOTAL_H = Math.max(SENDER_BAR_H, RECIP_BAR_H * N + RECIP_GAP * (N - 1));
  const recipBars = parsed.recipients.map((_, i) => {
    const top = i * (RECIP_BAR_H + RECIP_GAP);
    return { top, center: top + RECIP_BAR_H / 2 };
  });

  return (
    <div className="splits">
      <h3>Payment Splits</h3>

      <div className="splits-layout">
        {/* Left labels */}
        <div className="splits-left-info" style={{ height: TOTAL_H, display: 'flex', alignItems: 'flex-start', justifyContent: 'flex-end' }}>
          <div className="splits-sender-label" style={{ marginTop: Math.max(0, SENDER_BAR_H / 2 - 20) }}>
            <div className="splits-label-name">Payer</div>
            <div className="splits-label-amount">
              <Amount value={total} />
            </div>
          </div>
        </div>

        {/* SVG stack — gray base + colored overlay with CSS clip-path reveal */}
        <div className="splits-svg-stack" style={{ width: SVG_W, height: TOTAL_H, position: "relative" }}>
          {renderSplitsSvg(ribbons, recipBars, GRAYS, BAR_W, SVG_W, SENDER_BAR_H, RECIP_BAR_H, TOTAL_H, BRANCH_X_FRAC, "gray")}
          {(success || animating) && (
            <div
              key={animKey}
              className={`splits-color-overlay${animating ? " splits-sweeping" : ""}`}
            >
              {renderSplitsSvg(ribbons, recipBars, COLORS, BAR_W, SVG_W, SENDER_BAR_H, RECIP_BAR_H, TOTAL_H, BRANCH_X_FRAC, "color")}
            </div>
          )}
        </div>

        {/* Right labels */}
        <div className="splits-right-info" style={{ height: TOTAL_H }}>
          {ribbons.map((rib, i) => {
            const pct = ((rib.recipient.amount / total) * 100).toFixed(1);
            return (
              <div key={i} className="splits-recip-label" style={{ top: recipBars[i].center }}>
                <a
                  className="splits-label-name splits-addr-link"
                  href={explorerTokenUrl(rib.recipient.address, config)}
                  target="_blank"
                  rel="noopener"
                  title={rib.recipient.address}
                >
                  {rib.recipient.label}
                </a>
                <div className="splits-label-amount">
                  <Amount value={rib.recipient.amount} /> <span className="splits-pct">({pct}%)</span>
                </div>
                {rib.recipient.memo && (
                  <div className="splits-label-memo">{rib.recipient.memo}</div>
                )}
              </div>
            );
          })}
        </div>
      </div>

      {parsed.feePayerKey && (
        <div className="splits-fee-note">
          <span className="splits-fee-dot" />
          Fees sponsored by{" "}
          <a
            href={explorerTokenUrl(parsed.feePayerKey, config)}
            target="_blank"
            rel="noopener"
            className="splits-fee-link"
          >
            Operator
          </a>
        </div>
      )}
    </div>
  );
}
