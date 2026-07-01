import type { ReactNode } from "react";
import type { FlowStep } from "../types";

function fmtTime(iso: string): string {
  const d = new Date(iso);
  const hh = String(d.getHours()).padStart(2, "0");
  const mm = String(d.getMinutes()).padStart(2, "0");
  const ss = String(d.getSeconds()).padStart(2, "0");
  const ms = String(d.getMilliseconds()).padStart(3, "0");
  return `${hh}:${mm}:${ss}.${ms}`;
}

interface Props {
  steps: FlowStep[];
  failed?: boolean;
  success?: boolean;
  deliveredContent?: ReactNode;
}

const ROW_H = 52;
const DELIVERED_CONTENT_H = 28;
const R = 7;
const CX = 10;

export function SequenceDiagram({ steps, failed, success, deliveredContent }: Props) {
  const rowHeights = steps.map((step) =>
    step.key === "delivery" && deliveredContent ? ROW_H + DELIVERED_CONTENT_H : ROW_H,
  );
  const rowOffsets = rowHeights.map((_, i) =>
    rowHeights.slice(0, i).reduce((sum, height) => sum + height, 0),
  );
  const totalH = rowHeights.reduce((sum, height) => sum + height, 0);

  // Find the first non-completed step index (where failure icon goes)
  const failedIdx = failed
    ? steps.findIndex((s) => s.status !== "completed")
    : -1;

  return (
    <div className="sequence-diagram">
      <h3>Flow</h3>
      <div className="seq-container">
        <svg width={CX * 2} height={totalH} className="seq-track">
          {steps.map((step, i) => {
            const cy = rowOffsets[i] + R + 1;
            const isLast = i === steps.length - 1;
            const completed = step.status === "completed";
            const isFailed = i === failedIdx;

            const color = isFailed
              ? "var(--red)"
              : !success && !failed
                ? completed
                  ? "var(--fg-muted)"
                  : "var(--border)"
                : completed
                  ? "var(--green)"
                  : step.status === "in-progress"
                    ? "var(--yellow)"
                    : "var(--fg-muted)";

            const lineColor = !success && !failed
              ? (completed ? "var(--fg-muted)" : "var(--border)")
              : (completed ? "var(--green)" : "var(--border)");
            const showCheck = isLast && completed && !failed;

            return (
              <g key={step.key}>
                {!isLast && (
                  <line
                    x1={CX} y1={cy + R}
                    x2={CX} y2={rowOffsets[i + 1] + 1}
                    stroke={lineColor}
                    strokeWidth={3}
                  />
                )}
                <circle cx={CX} cy={cy} r={R} fill={color} />
                {showCheck && (
                  <path
                    d={`M${CX - 3.5} ${cy}L${CX - 1} ${cy + 2.5}L${CX + 3.5} ${cy - 2.5}`}
                    stroke="white"
                    strokeWidth="1.8"
                    strokeLinecap="round"
                    strokeLinejoin="round"
                    fill="none"
                  />
                )}
                {isFailed && (
                  <g>
                    <line x1={CX - 3} y1={cy - 3} x2={CX + 3} y2={cy + 3} stroke="white" strokeWidth="1.8" strokeLinecap="round" />
                    <line x1={CX + 3} y1={cy - 3} x2={CX - 3} y2={cy + 3} stroke="white" strokeWidth="1.8" strokeLinecap="round" />
                  </g>
                )}
              </g>
            );
          })}
        </svg>
        <div className="seq-labels">
          {steps.map((step, i) => (
            <div className="seq-row" key={step.key} style={{ height: rowHeights[i] }}>
              <div className={`step-label${step.status === "pending" ? " pending" : ""}${i === failedIdx ? " failed" : ""}`}>
                {step.label}
              </div>
              {step.ts && <div className="step-ts">{fmtTime(step.ts)}</div>}
              {step.key === "delivery" && deliveredContent}
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}
