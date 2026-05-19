import type { PaymentFlow, SessionInfo } from "../types";
import { explorerTokenUrl, useConfig } from "../hooks/useConfig";
import { formatUnits, shortAddr } from "../lib/format";

const U64_MAX = "18446744073709551615";
const SESSION_COLORS = [
  "#3fb950",
  "#58a6ff",
  "#39d2e0",
  "#d29922",
  "#db6d28",
  "#a371f7",
];

interface SessionRecipient {
  label: string;
  address?: string;
  bps: number;
  amount?: string;
}

type RibbonData = {
  recipient: SessionRecipient;
  thickness: number;
  srcTop: number;
  srcBottom: number;
  destTop: number;
  destBottom: number;
};

interface CapacityUsage {
  known: boolean;
  percent: number;
  percentLabel?: string;
  remainingAmount?: string;
  usedAmount?: string;
}

function applyBps(raw: string | undefined, bps: number): string | undefined {
  if (!raw || raw === U64_MAX || !/^\d+$/.test(raw)) return undefined;
  try {
    return ((BigInt(raw) * BigInt(bps)) / 10_000n).toString();
  } catch {
    return undefined;
  }
}

function actionLabel(action: SessionInfo["action"]): string {
  switch (action) {
    case "open":
      return "open";
    case "voucher":
      return "voucher";
    case "commit":
      return "commit";
    case "topUp":
      return "top up";
    case "close":
      return "close";
    default:
      return "challenge";
  }
}

function stateLabel(session: SessionInfo): string {
  if (session.state === "open") return "channel open";
  if (session.state === "opening") return "opening";
  if (session.state === "closed") return "closed";
  if (session.state === "failed") return "failed";
  return "settling";
}

function channelCap(session: SessionInfo): string | undefined {
  return session.deposit ?? session.approvedAmount ?? session.cap;
}

function meteredWatermark(session: SessionInfo): string | undefined {
  return session.cumulative;
}

function isBaseUnitAmount(value: string | undefined): value is string {
  return Boolean(value && value !== U64_MAX && /^\d+$/.test(value));
}

function capacityUsage(session: SessionInfo): CapacityUsage {
  const cap = channelCap(session);
  const used = meteredWatermark(session) ?? "0";
  if (!isBaseUnitAmount(cap) || !isBaseUnitAmount(used)) {
    return { known: false, percent: 0 };
  }

  try {
    const capValue = BigInt(cap);
    if (capValue <= 0n) return { known: false, percent: 0 };

    const usedValue = BigInt(used);
    const clampedUsed = usedValue > capValue ? capValue : usedValue;
    const basisPoints = Number((clampedUsed * 10_000n) / capValue);
    const percent = basisPoints / 100;
    const remaining = usedValue >= capValue ? 0n : capValue - usedValue;

    return {
      known: true,
      percent,
      percentLabel: formatPercent(percent),
      remainingAmount: remaining.toString(),
      usedAmount: usedValue.toString(),
    };
  } catch {
    return { known: false, percent: 0 };
  }
}

function formatPercent(percent: number): string {
  if (percent === 0 || percent === 100 || Number.isInteger(percent)) {
    return `${percent.toFixed(0)}%`;
  }
  if (percent < 1) {
    return `${percent.toFixed(2)}%`;
  }
  return `${percent.toFixed(1)}%`;
}

function sessionRecipients(session: SessionInfo, basis: string | undefined): SessionRecipient[] {
  const splitTotal = (session.splits ?? []).reduce((sum, split) => sum + split.bps, 0);
  const primaryBps = Math.max(0, 10_000 - splitTotal);
  const recipients: SessionRecipient[] = [];

  if (primaryBps > 0) {
    recipients.push({
      label: session.splits?.length ? "Primary" : "Provider",
      address: session.recipient,
      bps: primaryBps,
    });
  }

  for (const split of session.splits ?? []) {
    recipients.push({
      label: split.label ?? shortAddr(split.recipient),
      address: split.recipient,
      bps: split.bps,
    });
  }

  if (recipients.length === 0) {
    recipients.push({
      label: "Session",
      address: session.sessionId,
      bps: 10_000,
    });
  }

  return recipients.map((recipient) => ({
    ...recipient,
    amount: applyBps(basis, recipient.bps),
  }));
}

function splitLabel(bps: number): string {
  const pct = bps / 100;
  return `${pct.toFixed(bps % 100 === 0 ? 0 : 2)}%`;
}

function ribbonPath(
  rib: RibbonData,
  barWidth: number,
  svgWidth: number,
  branchXFrac: number,
): string {
  const x0 = barWidth;
  const x1 = svgWidth - barWidth;
  const branchX = x0 + (x1 - x0) * branchXFrac;
  const cx = branchX + (x1 - branchX) * 0.5;
  return `M ${x0} ${rib.srcTop} L ${branchX} ${rib.srcTop} C ${cx} ${rib.srcTop}, ${cx} ${rib.destTop}, ${x1} ${rib.destTop} L ${x1} ${rib.destBottom} C ${cx} ${rib.destBottom}, ${cx} ${rib.srcBottom}, ${branchX} ${rib.srcBottom} L ${x0} ${rib.srcBottom} Z`;
}

function renderSessionSvg(
  ribbons: RibbonData[],
  recipBars: { top: number; center: number }[],
  palette: string[],
  barWidth: number,
  svgWidth: number,
  recipBarHeight: number,
  totalHeight: number,
  branchXFrac: number,
  prefix: string,
) {
  const isProgressLayer = prefix === "color";
  const barOpacity = isProgressLayer ? "0.6" : "0.2";
  const ribbonEdgeOpacity = isProgressLayer ? "0.6" : "0.22";
  const ribbonMidOpacity = isProgressLayer ? "0.6" : "0.12";

  return (
    <svg
      className="splits-svg session-splits-svg"
      width={svgWidth}
      height={totalHeight}
      viewBox={`0 0 ${svgWidth} ${totalHeight}`}
    >
      <defs>
        {ribbons.map((_, i) => (
          <linearGradient key={i} id={`session-rib-${prefix}-${i}`} x1="0" x2="1">
            <stop
              offset="0%"
              stopColor={palette[i % palette.length]}
              stopOpacity={ribbonEdgeOpacity}
            />
            <stop
              offset="50%"
              stopColor={palette[i % palette.length]}
              stopOpacity={ribbonMidOpacity}
            />
            <stop
              offset="100%"
              stopColor={palette[i % palette.length]}
              stopOpacity={ribbonEdgeOpacity}
            />
          </linearGradient>
        ))}
      </defs>
      {ribbons.map((rib, i) => {
        const d = ribbonPath(rib, barWidth, svgWidth, branchXFrac);
        return (
          <path
            key={i}
            d={d}
            fill={`url(#session-rib-${prefix}-${i})`}
            className="splits-ribbon"
          />
        );
      })}
      {ribbons.map((rib, i) => (
        <rect
          key={i}
          x={0}
          y={rib.srcTop}
          width={barWidth}
          height={rib.thickness}
          fill={palette[i % palette.length]}
          fillOpacity={barOpacity}
        />
      ))}
      {recipBars.map((bar, i) => (
        <rect
          key={i}
          x={svgWidth - barWidth}
          y={bar.top}
          width={barWidth}
          height={recipBarHeight}
          fill={palette[i % palette.length]}
          fillOpacity={barOpacity}
        />
      ))}
    </svg>
  );
}

function renderSessionFlowSvg(
  ribbons: RibbonData[],
  barWidth: number,
  svgWidth: number,
  totalHeight: number,
  branchXFrac: number,
) {
  const lineSpacing = 18;
  const lineCount = Math.ceil(svgWidth / lineSpacing) + 3;
  const lineXs = Array.from({ length: lineCount }, (_, i) => i * lineSpacing);

  return (
    <svg
      aria-hidden="true"
      className="session-flow-svg"
      width={svgWidth}
      height={totalHeight}
      viewBox={`0 0 ${svgWidth} ${totalHeight}`}
    >
      <defs>
        {ribbons.map((rib, i) => (
          <clipPath key={i} id={`session-flow-clip-${i}`}>
            <path d={ribbonPath(rib, barWidth, svgWidth, branchXFrac)} />
          </clipPath>
        ))}
      </defs>
      {ribbons.map((_, i) => (
        <g
          key={i}
          className="session-flow-dots"
          clipPath={`url(#session-flow-clip-${i})`}
          style={{ animationDelay: `${i * -0.12}s` }}
        >
          {lineXs.map((x) => (
            <line
              key={x}
              x1={x}
              x2={x}
              y1={0}
              y2={totalHeight}
              stroke={SESSION_COLORS[i % SESSION_COLORS.length]}
              strokeDasharray="1 6"
              strokeLinecap="round"
              strokeOpacity="0.95"
              strokeWidth={2}
            />
          ))}
        </g>
      ))}
    </svg>
  );
}

export function SessionChannel({ flow }: { flow: PaymentFlow }) {
  const session = flow.session;
  const config = useConfig();
  if (!session) return null;

  const isOpen = session.state === "open";
  const currency = session.currency ?? "USDC";
  const decimals = session.decimals ?? 6;
  const cap = channelCap(session);
  const watermark = meteredWatermark(session);
  const hasVoucher = Boolean(watermark);
  const basis = watermark ?? cap;
  const capacity = capacityUsage(session);
  const recipients = sessionRecipients(session, basis);

  const recipBarHeight = 48;
  const barWidth = 4;
  const gap = 2;
  const svgWidth = 280;
  const branchXFrac = 1 / 3;
  const recipGap = 2;

  const flows = recipients.map((recipient) => {
    const pct = recipient.bps / 10_000;
    const thickness =
      pct >= 0.5
        ? recipBarHeight
        : pct >= 0.25
          ? recipBarHeight / 2
          : pct >= 0.1
            ? recipBarHeight / 3
            : recipBarHeight / 6;
    return { recipient, thickness };
  });

  const senderBarHeight =
    flows.reduce((sum, ribbon) => sum + ribbon.thickness, 0) +
    gap * Math.max(0, flows.length - 1);
  let stackY = 0;
  const ribbons = flows.map((ribbon, i) => {
    const srcTop = stackY;
    const srcBottom = stackY + ribbon.thickness;
    stackY += ribbon.thickness + gap;
    const destTop = i * (recipBarHeight + recipGap);
    return {
      ...ribbon,
      srcTop,
      srcBottom,
      destTop,
      destBottom: destTop + recipBarHeight,
    };
  });
  const totalHeight = Math.max(
    senderBarHeight,
    recipBarHeight * recipients.length + recipGap * Math.max(0, recipients.length - 1),
  );
  const recipBars = recipients.map((_, i) => {
    const top = i * (recipBarHeight + recipGap);
    return { top, center: top + recipBarHeight / 2 };
  });
  const progressPercent = Math.max(0, Math.min(100, capacity.percent));
  const overlayStyle = capacity.known
    ? { clipPath: `inset(0 ${100 - progressPercent}% 0 0)` }
    : undefined;

  return (
    <div className={`session-channel${isOpen ? " is-open" : ""}`}>
      <div className="session-channel-header">
        <h3>Session Channel</h3>
        <span className={`session-state ${session.state}`}>
          <span className="session-state-dot" />
          {stateLabel(session)}
        </span>
      </div>

      <div className="splits-layout session-ribbon-layout">
        <div
          className="splits-left-info"
          style={{
            height: totalHeight,
            display: "flex",
            alignItems: "flex-start",
            justifyContent: "flex-end",
          }}
        >
          <div
            className="splits-sender-label"
            style={{ marginTop: Math.max(0, senderBarHeight / 2 - 20) }}
          >
            <div className="splits-label-name">{hasVoucher ? "Watermark" : "Channel cap"}</div>
            <div className="splits-label-amount session-label-amount">
              {formatUnits(basis, decimals, currency)}
            </div>
            <div className="splits-label-memo">
              {capacity.percentLabel ? `${capacity.percentLabel} used` : actionLabel(session.action)}
            </div>
          </div>
        </div>

        <div
          className="splits-svg-stack session-svg-stack"
          style={{ width: svgWidth, height: totalHeight, position: "relative" }}
        >
          {renderSessionSvg(
            ribbons,
            recipBars,
            SESSION_COLORS,
            barWidth,
            svgWidth,
            recipBarHeight,
            totalHeight,
            branchXFrac,
            "background",
          )}
          {session.state !== "opening" && (
            <div className="session-color-overlay" style={overlayStyle}>
              {renderSessionSvg(
                ribbons,
                recipBars,
                SESSION_COLORS,
                barWidth,
                svgWidth,
                recipBarHeight,
                totalHeight,
                branchXFrac,
                "color",
              )}
              {isOpen &&
                renderSessionFlowSvg(
                  ribbons,
                  barWidth,
                  svgWidth,
                  totalHeight,
                  branchXFrac,
                )}
            </div>
          )}
        </div>

        <div className="splits-right-info" style={{ height: totalHeight }}>
          {ribbons.map((rib, i) => (
            <div
              key={`${rib.recipient.label}-${rib.recipient.bps}-${i}`}
              className="splits-recip-label"
              style={{ top: recipBars[i].center }}
            >
              {rib.recipient.address ? (
                <a
                  className="splits-label-name splits-addr-link"
                  href={explorerTokenUrl(rib.recipient.address, config)}
                  target="_blank"
                  rel="noopener"
                  title={rib.recipient.address}
                >
                  {rib.recipient.label}
                </a>
              ) : (
                <div className="splits-label-name">{rib.recipient.label}</div>
              )}
              <div className="splits-label-amount session-label-amount">
                {hasVoucher && rib.recipient.amount
                  ? formatUnits(rib.recipient.amount, decimals, currency)
                  : splitLabel(rib.recipient.bps)}
                <span className="splits-pct">({splitLabel(rib.recipient.bps)})</span>
              </div>
            </div>
          ))}
        </div>
      </div>

      <div className="session-metrics">
        <div className="session-metric">
          <span className="session-metric-label">session</span>
          <span className="session-metric-value" title={session.sessionId}>
            {shortAddr(session.sessionId)}
          </span>
        </div>
        <div className="session-metric">
          <span className="session-metric-label">mode</span>
          <span className="session-metric-value">{session.mode ?? "pending"}</span>
        </div>
        <div className="session-metric">
          <span className="session-metric-label">cap</span>
          <span className="session-metric-value">{formatUnits(cap, decimals, currency)}</span>
        </div>
        <div className="session-metric">
          <span className="session-metric-label">watermark</span>
          <span className="session-metric-value">
            {watermark ? formatUnits(watermark, decimals, currency) : "no voucher"}
          </span>
        </div>
        <div className="session-metric">
          <span className="session-metric-label">used</span>
          <span className="session-metric-value">{capacity.percentLabel ?? "unknown"}</span>
        </div>
        <div className="session-metric">
          <span className="session-metric-label">left</span>
          <span className="session-metric-value">
            {capacity.remainingAmount
              ? formatUnits(capacity.remainingAmount, decimals, currency)
              : "unknown"}
          </span>
        </div>
        <div className="session-metric">
          <span className="session-metric-label">vouchers</span>
          <span className="session-metric-value">{session.voucherCount ?? 0}</span>
        </div>
        <div className="session-metric">
          <span className="session-metric-label">min delta</span>
          <span className="session-metric-value">
            {formatUnits(session.minVoucherDelta, decimals, currency)}
          </span>
        </div>
      </div>
    </div>
  );
}
