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
}

export function SequenceDiagram({ steps }: Props) {
  return (
    <div className="sequence-diagram">
      <h3>Flow</h3>
      {steps.map((step, i) => {
        const isLast = i === steps.length - 1;
        // Line between steps inherits the status of the current step
        const lineCompleted = step.status === "completed" && !isLast;

        return (
          <div className="step" key={step.key}>
            <div className="step-track">
              <div className={`step-circle ${step.status}`} />
              {!isLast && (
                <div
                  className={`step-line${lineCompleted ? " completed" : ""}`}
                />
              )}
            </div>
            <div className="step-content">
              <div
                className={`step-label${step.status === "pending" ? " pending" : ""}`}
              >
                {step.label}
              </div>
              {step.ts && (
                <div className="step-ts">{fmtTime(step.ts)}</div>
              )}
            </div>
          </div>
        );
      })}
    </div>
  );
}
